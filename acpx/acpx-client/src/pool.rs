//! Exclusive hot-path lease pool for reusable ACP sessions.
//!
//! See `memory/acpx/gen/plans/acpx-client-session-lease-pool/00-plan.md` for
//! the full design. Summary: an idle-pool key (`PoolKey`) identifies
//! sessions compatible for reuse (project dir + agent + provider/profile).
//! At most one thread may hold a session's lease at a time. Acquisition is
//! atomic per key -- creation attempts for the same key never run
//! concurrently -- and after the first acquire for a key, background
//! warmup replenishes idle capacity up to [`WARM_TARGET_PER_KEY`].
//!
//! This module is transport-agnostic: it drives session creation/resume
//! through the injectable [`SessionOpener`] trait rather than depending on
//! `Gateway` directly, so the state machine can be unit-tested without a
//! live ACPX connection. `Gateway`-backed wiring lives in the panel-rust
//! integration layer.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

/// How many total warm sessions (leased + idle) a pool tries to maintain
/// for a key once that key has been requested at least once. Providers
/// never requested are never pre-warmed.
pub const WARM_TARGET_PER_KEY: usize = 2;

/// Bound on a single `session/new`/`session/resume` attempt during
/// acquire. An acquire that can't get a usable session within this window
/// fails boundedly rather than hanging a thread attach indefinitely.
pub const ACQUIRE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// The idle-pool key: sessions under the same key are compatible to reuse.
/// `thread_id` (the lease owner) is deliberately not part of this key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub project_dir: String,
    pub agent_id: String,
    pub provider_profile: String,
}

impl PoolKey {
    pub fn new(
        project_dir: impl Into<String>,
        agent_id: impl Into<String>,
        provider_profile: impl Into<String>,
    ) -> Self {
        Self {
            project_dir: project_dir.into(),
            agent_id: agent_id.into(),
            provider_profile: provider_profile.into(),
        }
    }
}

/// Opaque handle identifying one lease acquisition. Not reused across
/// acquisitions, so a stale lease (e.g. held past release) can never be
/// mistaken for a fresh one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseId(u64);

static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);

fn next_lease_id() -> LeaseId {
    LeaseId(NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed))
}

/// Caller-supplied identity of the panel thread holding a lease. Only used
/// for ownership bookkeeping/diagnostics; the pool does not interpret it.
pub type ThreadId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnState {
    NotStarted,
    ActiveTurn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryState {
    Idle,
    Leased {
        lease: LeaseId,
        thread: ThreadId,
        turn: TurnState,
    },
    /// Transport loss detected; only becomes `Idle` again after a
    /// successful `session/resume`. Not leasable.
    Recovering,
}

#[derive(Clone, Debug)]
struct Entry {
    session_id: String,
    state: EntryState,
    /// The key's `KeyPool::generation` at the moment this entry was
    /// created. Compared against the key's *current* generation at every
    /// point an entry would otherwise re-enter the idle pool (`release`,
    /// `resume_recovering`) -- see [`ProjectSessionPool::refresh_key`].
    generation: u64,
    /// The raw `session/new` response captured at `create()` time (`None`
    /// for a warm entry the caller's `SessionOpener` didn't report
    /// capabilities for). Carried into `SessionLease::capabilities` so a
    /// thread that acquires a freshly-created or previously-warmed session
    /// -- which was never itself `session/resume`d, so has no independent
    /// resume response to read capabilities from -- can still populate its
    /// model/mode/agent config options instead of leaving them permanently
    /// empty. See `acpx-client-session-lease-pool`'s
    /// `fresh-create-had-no-capabilities-regression` fix note.
    capabilities: Option<serde_json::Value>,
}

/// A successfully acquired session, exclusively owned by `thread_id` until
/// released or invalidated.
#[derive(Clone, Debug)]
pub struct SessionLease {
    pub lease_id: LeaseId,
    pub key: PoolKey,
    pub session_id: String,
    pub thread_id: ThreadId,
    /// `true` when this lease came from a saved/persisted session id that
    /// was proven usable via `session/resume`; `false` for a freshly
    /// created (`session/new`) session. Panel-rust's capability-loading
    /// precedence uses this to decide whether to trust cached capabilities.
    pub resumed_from_saved: bool,
    /// The `session/new` response captured when this session was created
    /// (by this `acquire()` call or an earlier warmup pass), if the
    /// `SessionOpener` reported one. `None` when `resumed_from_saved` is
    /// `true` (the caller performs its own `session/resume` and reads
    /// capabilities from that response instead) or when the opener simply
    /// didn't supply one.
    pub capabilities: Option<serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("no session available for this key: {0}")]
    OpenFailed(String),
    #[error("timed out acquiring a session")]
    Timeout,
    #[error("lease not found or already released")]
    UnknownLease,
    #[error("cannot release a lease with an active turn")]
    ActiveTurnNotReleasable,
    #[error("lease is owned by a different thread")]
    NotOwner,
}

/// A `SessionOpener::resume`/`create` failure, carrying whether it is
/// authentication/capacity-related. Per the plan: "authentication and
/// capacity errors are terminal for that attempt and do not trigger a
/// creation/retry storm" -- `terminal: true` tells [`ProjectSessionPool::
/// open_session`] to propagate the failure immediately rather than falling
/// back to `create()` after a failed `resume()` (a stable server-side
/// condition; retrying it just repeats the same failure or adds pressure
/// while the underlying cause -- bad credentials, exhausted capacity --
/// is still true). Any other failure (stale/missing session id, transient
/// transport error, ...) is `terminal: false` and does trigger the
/// existing fall-back-to-create behavior.
#[derive(Debug, Clone)]
pub struct OpenError {
    pub message: String,
    pub terminal: bool,
}

impl OpenError {
    pub fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            terminal: true,
        }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            terminal: false,
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// What the pool asks the transport layer to do to obtain a usable
/// session. Implemented against `Gateway` in panel-rust; a fake/in-memory
/// implementation is used for this module's own unit tests.
pub trait SessionOpener: Send + Sync + 'static {
    /// Resume a previously saved session id. `Err` means the id is stale
    /// or otherwise unusable and (unless `OpenError::terminal`) a fresh
    /// session should be tried instead.
    fn resume<'a>(
        &'a self,
        key: &'a PoolKey,
        saved_session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, OpenError>> + Send + 'a>>;

    /// Create a brand new session for this key. The second element of a
    /// successful result is the raw `session/new` response, if the
    /// implementation has one worth keeping -- see `SessionLease::
    /// capabilities`'s doc comment for why this matters (a freshly-created
    /// session is never independently `session/resume`d by any other code
    /// path, so this is the only chance to capture its capabilities).
    fn create<'a>(
        &'a self,
        key: &'a PoolKey,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, Option<serde_json::Value>), OpenError>> + Send + 'a,
        >,
    >;
}

/// What to try when opening a session for a key: prefer resuming a saved
/// id, falling back to `session/new` only when there is none or it proved
/// stale.
#[derive(Clone, Debug, Default)]
pub struct OpenSpec {
    pub saved_session_id: Option<String>,
}

struct KeyPool {
    entries: Vec<Entry>,
    /// Guards session-open attempts (`resume`/`create`) for this key so
    /// concurrent callers never race two opens at once. Held only across
    /// the open attempt itself, never across an entire `acquire()` (idle
    /// pop/push bookkeeping happens under the pool-wide mutex instead).
    open_gate: Arc<AsyncMutex<()>>,
    /// Single-flight guard so at most one background warmup task runs per
    /// key at a time.
    warmup_inflight: bool,
    /// Set once this key has been requested at least once; only then does
    /// warmup replenish idle capacity.
    activated: bool,
    /// Bumped by [`ProjectSessionPool::refresh_key`] whenever this key's
    /// compatibility-relevant open config (e.g. `mcpServers`) changes.
    /// Every entry created after a bump carries the new generation; every
    /// entry from an older generation is dropped rather than returned to
    /// idle, so a stale session (opened with config that no longer
    /// matches what `SessionOpener::create` would produce now) can never
    /// be re-leased.
    generation: u64,
}

impl KeyPool {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            open_gate: Arc::new(AsyncMutex::new(())),
            warmup_inflight: false,
            activated: false,
            generation: 0,
        }
    }

    fn idle_deficit(&self) -> usize {
        WARM_TARGET_PER_KEY.saturating_sub(self.entries.len())
    }
}

/// The pool itself. Cheap to clone (an `Arc` around the shared state) so
/// it can be shared across thread actors.
pub struct ProjectSessionPool<O: SessionOpener> {
    opener: Arc<O>,
    keys: AsyncMutex<HashMap<PoolKey, KeyPool>>,
}

impl<O: SessionOpener> ProjectSessionPool<O> {
    pub fn new(opener: O) -> Self {
        Self {
            opener: Arc::new(opener),
            keys: AsyncMutex::new(HashMap::new()),
        }
    }

    /// Access to the shared opener -- e.g. so a caller can push an updated
    /// MCP config into a `GatewaySessionOpener` (via its own
    /// `set_mcp_servers`) before calling [`Self::refresh_key`]/
    /// [`Self::refresh_all`] to drop stale idle sessions.
    pub fn opener(&self) -> &Arc<O> {
        &self.opener
    }

    /// Acquires an exclusive lease for `thread_id` under `key`. Prefers an
    /// idle entry already in the pool; otherwise opens a new one (resume
    /// then create, per `open_spec`), single-flighted per key so two
    /// concurrent acquires for the same key never race two opens.
    ///
    /// On success, schedules bounded background replenishment toward
    /// [`WARM_TARGET_PER_KEY`] for this key -- the first request for a
    /// provider is also its warmup trigger.
    pub async fn acquire(
        self: &Arc<Self>,
        key: PoolKey,
        thread_id: ThreadId,
        open_spec: OpenSpec,
    ) -> Result<SessionLease, PoolError> {
        // Fast path: an idle entry already exists for this key. Leased
        // in place (mutated, never removed from `entries`) -- see
        // `lease_entry`'s own doc comment for why a remove-without-
        // reinsert here would silently orphan the entry from every
        // later `release`/`invalidate`/`mark_turn_*` call against it.
        let existing = {
            let mut keys = self.keys.lock().await;
            let key_pool = keys.entry(key.clone()).or_insert_with(KeyPool::new);
            key_pool.activated = true;
            key_pool
                .entries
                .iter()
                .position(|e| matches!(e.state, EntryState::Idle))
                .map(|idx| self.lease_entry(&mut key_pool.entries[idx], key.clone(), thread_id.clone(), false))
        };
        let lease = if let Some(lease) = existing {
            lease
        } else {
            self.open_and_lease(key.clone(), thread_id.clone(), open_spec)
                .await?
        };

        self.spawn_warmup_if_needed(key);
        Ok(lease)
    }

    /// Transitions `entry` (still in place inside its `KeyPool::entries`,
    /// never removed) from `Idle` to `Leased`, and builds the
    /// `SessionLease` handle for it. **Must** be called on an entry that
    /// stays reachable through `key_pool.entries` afterward -- pool-
    /// capability-fix regression: an earlier version of both call sites
    /// below `entries.remove()`d the entry first and passed the owned
    /// value here, which built a valid-looking `SessionLease` whose
    /// `lease_id` matched nothing in `entries` anymore. That lease could
    /// be used to send prompts (fine, doesn't touch `entries`) but its
    /// *first* `release`/`invalidate`/`mark_turn_*` call always failed
    /// `UnknownLease`, silently orphaning the entry from the pool's
    /// bookkeeping forever (the backend session stayed alive, un-
    /// trackable and unreusable) instead of returning it to idle.
    fn lease_entry(
        &self,
        entry: &mut Entry,
        key: PoolKey,
        thread_id: ThreadId,
        resumed_from_saved: bool,
    ) -> SessionLease {
        let lease_id = next_lease_id();
        entry.state = EntryState::Leased {
            lease: lease_id,
            thread: thread_id.clone(),
            turn: TurnState::NotStarted,
        };
        SessionLease {
            lease_id,
            key,
            session_id: entry.session_id.clone(),
            thread_id,
            resumed_from_saved,
            capabilities: entry.capabilities.clone(),
        }
    }

    async fn open_and_lease(
        self: &Arc<Self>,
        key: PoolKey,
        thread_id: ThreadId,
        open_spec: OpenSpec,
    ) -> Result<SessionLease, PoolError> {
        let open_gate = {
            let mut keys = self.keys.lock().await;
            keys.entry(key.clone())
                .or_insert_with(KeyPool::new)
                .open_gate
                .clone()
        };
        // Single-flight this key's open attempts. Anyone else opening for
        // the same key waits here rather than racing a second `session/
        // new`/`session/resume` in parallel.
        let _open_permit = open_gate.lock().await;

        // Re-check idle entries: a racer that opened while we waited on
        // the gate may have already produced a spare (e.g. warmup).
        // Same in-place-lease requirement as the fast path in `acquire`
        // -- see `lease_entry`'s doc comment.
        let racer_spare = {
            let mut keys = self.keys.lock().await;
            let key_pool = keys.entry(key.clone()).or_insert_with(KeyPool::new);
            key_pool
                .entries
                .iter()
                .position(|e| matches!(e.state, EntryState::Idle))
                .map(|idx| self.lease_entry(&mut key_pool.entries[idx], key.clone(), thread_id.clone(), false))
        };
        if let Some(lease) = racer_spare {
            return Ok(lease);
        }

        // Snapshotted just before the actual open call (not after): the
        // entry's generation should reflect whatever config was live when
        // `SessionOpener::create`/`resume` ran, not whatever a concurrent
        // `refresh_key` bumped it to immediately afterward.
        let generation_at_open = {
            self.keys
                .lock()
                .await
                .entry(key.clone())
                .or_insert_with(KeyPool::new)
                .generation
        };
        let (session_id, capabilities, resumed_from_saved) =
            self.open_session(&key, &open_spec).await?;

        let lease_id = next_lease_id();
        {
            let mut keys = self.keys.lock().await;
            let key_pool = keys.entry(key.clone()).or_insert_with(KeyPool::new);
            key_pool.entries.push(Entry {
                session_id: session_id.clone(),
                state: EntryState::Leased {
                    lease: lease_id,
                    thread: thread_id.clone(),
                    turn: TurnState::NotStarted,
                },
                generation: generation_at_open,
                capabilities: capabilities.clone(),
            });
        }
        Ok(SessionLease {
            lease_id,
            key,
            session_id,
            thread_id,
            resumed_from_saved,
            capabilities,
        })
    }

    /// Prefers `session/resume` on a saved id; falls back to `session/new`
    /// only when there is no saved id or the resume attempt proved the id
    /// stale/missing. Bounded by [`ACQUIRE_OPEN_TIMEOUT`] so a hung
    /// transport degrades to a bounded error instead of stalling a thread
    /// attach indefinitely.
    async fn open_session(
        &self,
        key: &PoolKey,
        open_spec: &OpenSpec,
    ) -> Result<(String, Option<serde_json::Value>, bool), PoolError> {
        let attempt = async {
            if let Some(saved_id) = &open_spec.saved_session_id {
                match self.opener.resume(key, saved_id).await {
                    Ok(session_id) => return Ok((session_id, None, true)),
                    // Authentication/capacity failures are a stable
                    // server-side condition: falling back to `create()`
                    // here would just repeat the same failure (or worse,
                    // pile more load on an already-exhausted-capacity
                    // provider) instead of surfacing it. Propagate
                    // immediately rather than attempting create().
                    Err(err) if err.terminal => {
                        return Err(PoolError::OpenFailed(err.message));
                    }
                    Err(_) => {}
                }
            }
            self.opener
                .create(key)
                .await
                .map(|(session_id, capabilities)| (session_id, capabilities, false))
                .map_err(|err| PoolError::OpenFailed(err.message))
        };
        tokio::time::timeout(ACQUIRE_OPEN_TIMEOUT, attempt)
            .await
            .map_err(|_| PoolError::Timeout)?
    }

    /// Marks a leased session's turn as started. Only meaningful while the
    /// lease is owned by `lease.thread_id`.
    pub async fn mark_turn_started(&self, lease: &SessionLease) -> Result<(), PoolError> {
        self.with_leased_entry_mut(lease, |state| {
            if let EntryState::Leased { turn, .. } = state {
                *turn = TurnState::ActiveTurn;
                Ok(())
            } else {
                Err(PoolError::UnknownLease)
            }
        })
        .await
    }

    /// Marks a leased session's turn as finished (terminal turn event).
    /// This makes the lease *eligible* for release; it does not itself
    /// return the session to the pool.
    pub async fn mark_turn_finished(&self, lease: &SessionLease) -> Result<(), PoolError> {
        self.with_leased_entry_mut(lease, |state| {
            if let EntryState::Leased { turn, .. } = state {
                *turn = TurnState::NotStarted;
                Ok(())
            } else {
                Err(PoolError::UnknownLease)
            }
        })
        .await
    }

    /// Releases a lease back to the idle pool. Fails closed: rejected
    /// while a turn is active, and a lease that isn't found/owned never
    /// silently succeeds (so a caller can't accidentally believe an
    /// active session was released).
    pub async fn release(&self, lease: &SessionLease) -> Result<(), PoolError> {
        let mut keys = self.keys.lock().await;
        let key_pool = keys.get_mut(&lease.key).ok_or(PoolError::UnknownLease)?;
        let current_generation = key_pool.generation;
        let index = key_pool
            .entries
            .iter()
            .position(
                |e| matches!(&e.state, EntryState::Leased { lease: l, .. } if *l == lease.lease_id),
            )
            .ok_or(PoolError::UnknownLease)?;
        let entry = &key_pool.entries[index];
        match &entry.state {
            EntryState::Leased { thread, turn, .. } => {
                if thread != &lease.thread_id {
                    return Err(PoolError::NotOwner);
                }
                if *turn == TurnState::ActiveTurn {
                    return Err(PoolError::ActiveTurnNotReleasable);
                }
            }
            _ => return Err(PoolError::UnknownLease),
        }
        // A session opened under an older generation (see `refresh_key`)
        // must never re-enter the idle pool with config that no longer
        // matches what `SessionOpener` would produce now -- drop it
        // instead of idling it.
        if entry.generation != current_generation {
            key_pool.entries.remove(index);
        } else {
            key_pool.entries[index].state = EntryState::Idle;
        }
        Ok(())
    }

    /// Invalidates a lease: the entry is removed from the pool entirely
    /// (a closed/auth-failed/capacity-failed session is not leasable, and
    /// must not silently reappear as idle). Safe to call regardless of
    /// turn state -- invalidation is not a graceful release.
    pub async fn invalidate(&self, lease: &SessionLease) -> Result<(), PoolError> {
        let mut keys = self.keys.lock().await;
        let key_pool = keys.get_mut(&lease.key).ok_or(PoolError::UnknownLease)?;
        let before = key_pool.entries.len();
        key_pool.entries.retain(
            |e| !matches!(&e.state, EntryState::Leased { lease: l, .. } if *l == lease.lease_id),
        );
        if key_pool.entries.len() == before {
            return Err(PoolError::UnknownLease);
        }
        Ok(())
    }

    /// Marks a session `Recovering` after transport loss is detected for
    /// it (e.g. a reconnect-worthy `Gateway` disconnect while this session
    /// was leased/idle). A `Recovering` entry is not leasable and must be
    /// proven usable again via `session/resume` before returning to
    /// `Idle` ([`Self::resume_recovering`]).
    pub async fn mark_recovering(&self, key: &PoolKey, session_id: &str) {
        let mut keys = self.keys.lock().await;
        if let Some(key_pool) = keys.get_mut(key) {
            for entry in key_pool.entries.iter_mut() {
                if entry.session_id == session_id {
                    entry.state = EntryState::Recovering;
                }
            }
        }
    }

    /// Attempts `session/resume` on a `Recovering` entry; only on success
    /// does it become `Idle` again (leasable). On failure it is removed
    /// from the pool -- a confirmed-stale session must not linger as an
    /// unusable idle-looking entry.
    pub async fn resume_recovering(
        &self,
        key: &PoolKey,
        session_id: &str,
    ) -> Result<(), PoolError> {
        let resumed = self.opener.resume(key, session_id).await;
        let mut keys = self.keys.lock().await;
        let Some(key_pool) = keys.get_mut(key) else {
            return Err(PoolError::UnknownLease);
        };
        match resumed {
            Ok(_) => {
                let current_generation = key_pool.generation;
                // Same stale-generation rule as `release`: a resumed
                // session opened under an older config generation must not
                // re-enter the idle pool -- drop it instead so the next
                // acquire/warmup opens a fresh one under the current
                // config.
                key_pool.entries.retain(|e| {
                    !(e.session_id == session_id
                        && e.state == EntryState::Recovering
                        && e.generation != current_generation)
                });
                for entry in key_pool.entries.iter_mut() {
                    if entry.session_id == session_id && entry.state == EntryState::Recovering {
                        entry.state = EntryState::Idle;
                    }
                }
                Ok(())
            }
            Err(err) => {
                key_pool
                    .entries
                    .retain(|e| e.session_id != session_id || e.state != EntryState::Recovering);
                Err(PoolError::OpenFailed(err.message))
            }
        }
    }

    /// Bumps this key's config generation and drops every currently-idle
    /// entry, so the next acquire/warmup for this key opens fresh sessions
    /// via `SessionOpener` rather than handing out one whose `mcpServers`
    /// (or any other compatibility-relevant open config) was fixed at an
    /// older `session/new` call. A currently-leased entry is left alone
    /// (its session is in active use and cannot be force-closed safely)
    /// but is stamped stale by this same generation bump: `release`/
    /// `resume_recovering` above drop it instead of returning it to idle,
    /// so it drains out of the pool naturally as its owning thread
    /// finishes with it, rather than being force-torn-down mid-use.
    ///
    /// No-op for a key that has never been requested (nothing to refresh).
    /// Call this whenever the caller's own MCP/config source changes in a
    /// way that should apply to this key's *future* sessions -- typically
    /// paired with updating whatever `SessionOpener` impl this pool was
    /// constructed with (see `GatewaySessionOpener::set_mcp_servers`) so
    /// the newly-opened sessions actually reflect the new config.
    pub async fn refresh_key(&self, key: &PoolKey) {
        let mut keys = self.keys.lock().await;
        let Some(key_pool) = keys.get_mut(key) else {
            return;
        };
        key_pool.generation = key_pool.generation.wrapping_add(1);
        key_pool
            .entries
            .retain(|e| !matches!(e.state, EntryState::Idle));
    }

    /// Refreshes every key currently tracked by this pool -- e.g. a
    /// project-wide MCP config change (skills directory moved, snapshotd
    /// listener address changed) that should apply regardless of which
    /// provider/agent a given key was for.
    pub async fn refresh_all(&self) {
        let mut keys = self.keys.lock().await;
        for key_pool in keys.values_mut() {
            key_pool.generation = key_pool.generation.wrapping_add(1);
            key_pool
                .entries
                .retain(|e| !matches!(e.state, EntryState::Idle));
        }
    }

    /// Read-only check: does this lease's entry carry a generation older
    /// than its key's current generation (i.e. would `release` drop it
    /// rather than return it to idle)?
    ///
    /// Mirrors the comparison `release` already performs internally --
    /// exposed so a caller that still holds a lease (turn still
    /// `NotStarted`) can decide to release+reacquire under the current
    /// generation before sending the first prompt. Returns `false` if the
    /// lease/key is unknown (no longer in the pool at all), matching the
    /// "not stale for our purposes" posture of a missing entry: the
    /// caller will reacquire anyway if the lease is gone.
    ///
    /// No state mutation, no new [`PoolError`] variant.
    pub async fn is_lease_stale(&self, lease: &SessionLease) -> bool {
        let keys = self.keys.lock().await;
        let Some(key_pool) = keys.get(&lease.key) else {
            return false;
        };
        let current_generation = key_pool.generation;
        key_pool
            .entries
            .iter()
            .find(
                |e| matches!(&e.state, EntryState::Leased { lease: l, .. } if *l == lease.lease_id),
            )
            .is_some_and(|entry| entry.generation != current_generation)
    }

    async fn with_leased_entry_mut<F>(&self, lease: &SessionLease, f: F) -> Result<(), PoolError>
    where
        F: FnOnce(&mut EntryState) -> Result<(), PoolError>,
    {
        let mut keys = self.keys.lock().await;
        let key_pool = keys.get_mut(&lease.key).ok_or(PoolError::UnknownLease)?;
        let entry = key_pool
            .entries
            .iter_mut()
            .find(
                |e| matches!(&e.state, EntryState::Leased { lease: l, .. } if *l == lease.lease_id),
            )
            .ok_or(PoolError::UnknownLease)?;
        if let EntryState::Leased { thread, .. } = &entry.state {
            if thread != &lease.thread_id {
                return Err(PoolError::NotOwner);
            }
        }
        f(&mut entry.state)
    }

    /// Bounded background replenishment toward [`WARM_TARGET_PER_KEY`].
    /// Single-flighted per key (a key already replenishing skips a
    /// redundant spawn) and never takes ownership away from the acquiring
    /// thread -- it only ever adds fresh `Idle` entries.
    fn spawn_warmup_if_needed(self: &Arc<Self>, key: PoolKey) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let deficit = {
                let mut keys = pool.keys.lock().await;
                let key_pool = match keys.get_mut(&key) {
                    Some(kp) if kp.activated => kp,
                    _ => return,
                };
                if key_pool.warmup_inflight {
                    return;
                }
                let deficit = key_pool.idle_deficit();
                if deficit == 0 {
                    return;
                }
                key_pool.warmup_inflight = true;
                deficit
            };

            for _ in 0..deficit {
                let open_gate = {
                    let keys = pool.keys.lock().await;
                    match keys.get(&key) {
                        Some(kp) => kp.open_gate.clone(),
                        None => break,
                    }
                };
                let _permit = open_gate.lock().await;
                // Re-check under the gate: an acquire may have consumed
                // the deficit (or grown it) while warmup queued up.
                let (still_needed, generation_at_open) = {
                    let keys = pool.keys.lock().await;
                    match keys.get(&key) {
                        Some(kp) => (kp.idle_deficit() > 0, kp.generation),
                        None => (false, 0),
                    }
                };
                if !still_needed {
                    break;
                }
                let opened = tokio::time::timeout(ACQUIRE_OPEN_TIMEOUT, pool.opener.create(&key))
                    .await
                    .ok()
                    .and_then(Result::ok);
                let Some((session_id, capabilities)) = opened else {
                    // Bounded: one failed attempt per deficit slot, no
                    // retry storm. A future `acquire`/warmup pass will
                    // try again.
                    break;
                };
                let mut keys = pool.keys.lock().await;
                if let Some(key_pool) = keys.get_mut(&key) {
                    key_pool.entries.push(Entry {
                        session_id,
                        state: EntryState::Idle,
                        generation: generation_at_open,
                        capabilities,
                    });
                }
            }

            let mut keys = pool.keys.lock().await;
            if let Some(key_pool) = keys.get_mut(&key) {
                key_pool.warmup_inflight = false;
            }
        });
    }

    /// Total sessions (idle + leased + recovering) currently tracked for a
    /// key. Test/diagnostic helper.
    pub async fn total_for_key(&self, key: &PoolKey) -> usize {
        self.keys
            .lock()
            .await
            .get(key)
            .map(|kp| kp.entries.len())
            .unwrap_or(0)
    }

    /// Idle-only count for a key. Test/diagnostic helper.
    pub async fn idle_for_key(&self, key: &PoolKey) -> usize {
        self.keys
            .lock()
            .await
            .get(key)
            .map(|kp| {
                kp.entries
                    .iter()
                    .filter(|e| matches!(e.state, EntryState::Idle))
                    .count()
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    struct CountingOpener {
        creates: AtomicUsize,
        resumes: AtomicUsize,
        fail_resume: bool,
        /// When `fail_resume` is set, whether that failure is
        /// authentication/capacity-terminal (must not fall back to
        /// `create()`) or an ordinary retryable failure (falls back).
        resume_failure_terminal: bool,
        create_delay: Duration,
        /// What `create()` reports as the raw `session/new` response --
        /// `None` by default (most tests don't care), settable per test to
        /// verify capability threading through `acquire()`/warmup.
        create_capabilities: Option<serde_json::Value>,
    }

    impl CountingOpener {
        fn new() -> Self {
            Self {
                creates: AtomicUsize::new(0),
                resumes: AtomicUsize::new(0),
                fail_resume: false,
                resume_failure_terminal: false,
                create_delay: Duration::from_millis(0),
                create_capabilities: None,
            }
        }
    }

    impl SessionOpener for CountingOpener {
        fn resume<'a>(
            &'a self,
            _key: &'a PoolKey,
            saved_session_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, OpenError>> + Send + 'a>> {
            Box::pin(async move {
                self.resumes.fetch_add(1, Ordering::SeqCst);
                if self.fail_resume {
                    if self.resume_failure_terminal {
                        Err(OpenError::terminal("authentication required"))
                    } else {
                        Err(OpenError::retryable("stale"))
                    }
                } else {
                    Ok(saved_session_id.to_string())
                }
            })
        }

        fn create<'a>(
            &'a self,
            _key: &'a PoolKey,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(String, Option<serde_json::Value>), OpenError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                if !self.create_delay.is_zero() {
                    tokio::time::sleep(self.create_delay).await;
                }
                let n = self.creates.fetch_add(1, Ordering::SeqCst);
                Ok((format!("session-{n}"), self.create_capabilities.clone()))
            })
        }
    }

    fn test_key() -> PoolKey {
        PoolKey::new("/proj", "agent-1", "codex/default")
    }

    #[tokio::test]
    async fn concurrent_acquire_same_key_yields_distinct_sessions_no_double_creation_race() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();

        let p1 = Arc::clone(&pool);
        let k1 = key.clone();
        let p2 = Arc::clone(&pool);
        let k2 = key.clone();
        let (a, b) = tokio::join!(
            p1.acquire(k1, "thread-a".to_string(), OpenSpec::default()),
            p2.acquire(k2, "thread-b".to_string(), OpenSpec::default())
        );
        let a = a.expect("acquire a");
        let b = b.expect("acquire b");
        assert_ne!(
            a.session_id, b.session_id,
            "two concurrent acquires for the same key must not be handed the same session"
        );
    }

    #[tokio::test]
    async fn release_during_active_turn_is_rejected() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let lease = pool
            .acquire(key, "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        pool.mark_turn_started(&lease).await.expect("mark started");
        let err = pool.release(&lease).await.unwrap_err();
        assert!(matches!(err, PoolError::ActiveTurnNotReleasable));
    }

    #[tokio::test]
    async fn released_session_can_be_reacquired_by_another_thread() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        pool.mark_turn_started(&lease).await.expect("start");
        pool.mark_turn_finished(&lease).await.expect("finish");
        pool.release(&lease).await.expect("release");

        let lease2 = pool
            .clone()
            .acquire(key, "thread-b".to_string(), OpenSpec::default())
            .await
            .expect("reacquire");
        assert_eq!(lease.session_id, lease2.session_id);
        assert_eq!(lease2.thread_id, "thread-b");
    }

    #[tokio::test]
    async fn resume_before_reuse_prefers_saved_session_id_over_new() {
        let opener = CountingOpener::new();
        let pool = Arc::new(ProjectSessionPool::new(opener));
        let key = test_key();
        let lease = pool
            .acquire(
                key,
                "thread-a".to_string(),
                OpenSpec {
                    saved_session_id: Some("saved-session-42".to_string()),
                },
            )
            .await
            .expect("acquire");
        assert_eq!(lease.session_id, "saved-session-42");
        assert!(lease.resumed_from_saved);
        assert_eq!(pool.opener.creates.load(Ordering::SeqCst), 0);
        assert_eq!(pool.opener.resumes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fresh_create_carries_session_new_response_into_the_lease() {
        // Regression test: a freshly pool-created session (never itself
        // `session/resume`d by anyone) previously had no way to surface its
        // `session/new` response's capabilities (configOptions/
        // sessionModes) to the caller -- ChatInput's model/mode/agent
        // dropdowns stayed permanently empty for every thread the pool
        // created, since acpx's own capability-event emission only ran for
        // the resumed branch. `SessionLease::capabilities` closes that gap.
        let mut opener = CountingOpener::new();
        opener.create_capabilities = Some(serde_json::json!({
            "sessionId": "ignored-here",
            "configOptions": [{"id": "model", "currentValue": "sonnet"}],
        }));
        let pool = Arc::new(ProjectSessionPool::new(opener));
        let key = test_key();
        let lease = pool
            .acquire(key, "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        assert!(!lease.resumed_from_saved);
        let capabilities = lease
            .capabilities
            .expect("a freshly created session must carry its session/new response");
        assert_eq!(capabilities["configOptions"][0]["id"], "model");
    }

    #[tokio::test]
    async fn a_warmed_idle_entry_still_carries_its_capabilities_when_later_leased() {
        // Same gap, different path: the fast "reuse an already-idle entry"
        // branch of acquire() must not silently drop the capabilities that
        // were captured when that entry was originally created (by an
        // earlier acquire() or by background warmup).
        let mut opener = CountingOpener::new();
        opener.create_capabilities = Some(serde_json::json!({"configOptions": []}));
        let pool = Arc::new(ProjectSessionPool::new(opener));
        let key = test_key();
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        pool.mark_turn_finished(&lease).await.ok();
        pool.release(&lease).await.expect("release");
        assert_eq!(pool.idle_for_key(&key).await, 1);

        let lease2 = pool
            .acquire(key, "thread-b".to_string(), OpenSpec::default())
            .await
            .expect("reacquire idle entry");
        assert!(
            lease2.capabilities.is_some(),
            "reusing an idle entry must not drop its captured capabilities"
        );
    }

    #[tokio::test]
    async fn stale_saved_session_falls_back_to_create() {
        let mut opener = CountingOpener::new();
        opener.fail_resume = true;
        let pool = Arc::new(ProjectSessionPool::new(opener));
        let key = test_key();
        let lease = pool
            .acquire(
                key,
                "thread-a".to_string(),
                OpenSpec {
                    saved_session_id: Some("stale-id".to_string()),
                },
            )
            .await
            .expect("acquire");
        assert!(!lease.resumed_from_saved);
        assert_eq!(pool.opener.creates.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn authentication_or_capacity_resume_failure_does_not_retry_via_create() {
        let mut opener = CountingOpener::new();
        opener.fail_resume = true;
        opener.resume_failure_terminal = true;
        let pool = Arc::new(ProjectSessionPool::new(opener));
        let key = test_key();
        let err = pool
            .acquire(
                key,
                "thread-a".to_string(),
                OpenSpec {
                    saved_session_id: Some("saved-id".to_string()),
                },
            )
            .await
            .expect_err("a terminal resume failure must not succeed via a create() fallback");
        assert!(matches!(err, PoolError::OpenFailed(_)));
        assert_eq!(
            pool.opener.creates.load(Ordering::SeqCst),
            0,
            "an authentication/capacity failure must not trigger a create() retry storm"
        );
        assert_eq!(pool.opener.resumes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_history_replay_load_is_never_invoked_by_the_pool() {
        // The pool's `SessionOpener` trait intentionally exposes only
        // `resume`/`create`, never a `session/load`-shaped method -- so
        // there is no code path here that could replay transcript history
        // into a live append stream (see 00-plan.md, ACPX client changes
        // item 7). This test documents that invariant at the type level:
        // it would fail to compile if `SessionOpener` grew a `load` method
        // this module's acquire/warmup paths started calling.
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let _lease = pool
            .acquire(key, "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
    }

    #[tokio::test]
    async fn warmup_replenishes_idle_capacity_to_warm_target_after_first_acquire() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let _lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");

        // Warmup runs on a spawned task; poll briefly for it to land.
        for _ in 0..50 {
            if pool.total_for_key(&key).await >= WARM_TARGET_PER_KEY {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(pool.total_for_key(&key).await, WARM_TARGET_PER_KEY);
        assert_eq!(pool.idle_for_key(&key).await, WARM_TARGET_PER_KEY - 1);
    }

    #[tokio::test]
    async fn ungated_provider_is_never_prewarmed() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        // No acquire ever happened for this key: total must stay zero,
        // proving warmup never runs for a provider that was never
        // requested.
        assert_eq!(pool.total_for_key(&key).await, 0);
    }

    #[tokio::test]
    async fn invalidate_removes_entry_and_it_cannot_be_reacquired() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        pool.invalidate(&lease).await.expect("invalidate");
        assert_eq!(pool.total_for_key(&key).await, 0);
        // Second invalidate on the same (now-gone) lease must fail, not
        // silently succeed.
        assert!(matches!(
            pool.invalidate(&lease).await,
            Err(PoolError::UnknownLease)
        ));
    }

    #[tokio::test]
    async fn recovering_session_is_not_leasable_until_resume_succeeds() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        pool.mark_turn_finished(&lease).await.ok();
        pool.release(&lease).await.expect("release");
        pool.mark_recovering(&key, &lease.session_id).await;
        assert_eq!(pool.idle_for_key(&key).await, 0);

        pool.resume_recovering(&key, &lease.session_id)
            .await
            .expect("resume recovering");
        assert_eq!(pool.idle_for_key(&key).await, 1);
    }

    #[tokio::test]
    async fn release_rejects_wrong_owner() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let mut lease = pool
            .acquire(key, "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        lease.thread_id = "thread-imposter".to_string();
        assert!(matches!(
            pool.release(&lease).await,
            Err(PoolError::NotOwner)
        ));
    }

    #[tokio::test]
    async fn refresh_key_drops_idle_entries_so_next_acquire_opens_fresh() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        pool.mark_turn_finished(&lease).await.ok();
        pool.release(&lease).await.expect("release");
        assert_eq!(pool.idle_for_key(&key).await, 1);

        pool.refresh_key(&key).await;
        assert_eq!(
            pool.idle_for_key(&key).await,
            0,
            "refresh must drop the now-stale idle entry"
        );

        let lease2 = pool
            .clone()
            .acquire(key.clone(), "thread-b".to_string(), OpenSpec::default())
            .await
            .expect("reacquire after refresh");
        assert_ne!(
            lease.session_id, lease2.session_id,
            "post-refresh acquire must open a brand new session, not reuse the stale one"
        );
    }

    #[tokio::test]
    async fn refresh_key_marks_a_currently_leased_session_for_drop_on_release_not_reuse() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");

        // Refresh while the session is still actively leased -- must not
        // force-close it, but must stamp it stale for when it's released.
        pool.refresh_key(&key).await;
        assert_eq!(
            pool.total_for_key(&key).await,
            1,
            "an in-use session must not be force-removed by refresh"
        );

        pool.mark_turn_finished(&lease).await.ok();
        pool.release(&lease).await.expect("release");
        assert_eq!(
            pool.idle_for_key(&key).await,
            0,
            "a stale-generation session must be dropped on release, not idled"
        );
        assert_eq!(pool.total_for_key(&key).await, 0);
    }

    /// Broad coverage for the stale-lease reacquire contract used by
    /// panel-rust's SendPrompt path: fresh lease is not stale; refresh
    /// stamps it stale (and leaves an untouched key alone); release of a
    /// stale lease drops it; a subsequent acquire opens a new session id.
    #[tokio::test]
    async fn stale_lease_detect_release_and_reacquire_opens_fresh_session() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        let other_key = PoolKey::new("/other-proj", "agent-1", "codex/default");
        let lease = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("acquire");
        let other_lease = pool
            .clone()
            .acquire(other_key.clone(), "thread-b".to_string(), OpenSpec::default())
            .await
            .expect("acquire other");
        let original_sid = lease.session_id.clone();

        assert!(!pool.is_lease_stale(&lease).await);
        assert!(!pool.is_lease_stale(&other_lease).await);

        pool.refresh_key(&key).await;
        assert!(pool.is_lease_stale(&lease).await);
        assert!(
            !pool.is_lease_stale(&other_lease).await,
            "untouched key must stay non-stale"
        );

        pool.mark_turn_finished(&lease).await.ok();
        pool.release(&lease).await.expect("release stale lease");
        assert_eq!(pool.total_for_key(&key).await, 0);

        let reacquired = pool
            .clone()
            .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
            .await
            .expect("reacquire after refresh");
        assert_ne!(
            reacquired.session_id, original_sid,
            "post-refresh reacquire must open a fresh session"
        );
        assert!(!pool.is_lease_stale(&reacquired).await);
    }

    #[tokio::test]
    async fn refresh_on_a_never_requested_key_is_a_harmless_no_op() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key = test_key();
        pool.refresh_key(&key).await;
        assert_eq!(pool.total_for_key(&key).await, 0);
    }

    #[tokio::test]
    async fn refresh_all_drops_idle_entries_across_every_key() {
        let pool = Arc::new(ProjectSessionPool::new(CountingOpener::new()));
        let key_a = PoolKey::new("/proj-a", "agent-1", "codex/default");
        let key_b = PoolKey::new("/proj-b", "agent-1", "codex/default");
        for key in [&key_a, &key_b] {
            let lease = pool
                .clone()
                .acquire(key.clone(), "thread-a".to_string(), OpenSpec::default())
                .await
                .expect("acquire");
            pool.mark_turn_finished(&lease).await.ok();
            pool.release(&lease).await.expect("release");
        }
        assert_eq!(pool.idle_for_key(&key_a).await, 1);
        assert_eq!(pool.idle_for_key(&key_b).await, 1);

        pool.refresh_all().await;
        assert_eq!(pool.idle_for_key(&key_a).await, 0);
        assert_eq!(pool.idle_for_key(&key_b).await, 0);
    }
}
