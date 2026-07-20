//! N supervised backend processes keyed by agent name, with
//! restart-on-crash + backoff, per `04-phased-plan.md` Phase 2 step 5.
//!
//! **Concurrency (added post-Phase-6, see `acpx/COVERAGE.md`'s "real
//! multi-agent concurrency" section):** each running process is handed out
//! as a [`SharedBackendProcess`] (`Arc<tokio::sync::Mutex<BackendProcess>>`)
//! rather than an exclusive `&mut BackendProcess` borrow tied to the
//! `Supervisor`'s own lifetime. This lets a caller (`acpx-core::router`)
//! release the `Supervisor`'s/`Router`'s own lock *before* doing the
//! actual (potentially many-second, real-LLM-latency) stdio round trip
//! against one specific backend -- two callers targeting two *different*
//! agent ids proceed fully in parallel; two callers targeting the *same*
//! agent id still correctly serialize on that one process's own mutex,
//! since one backend's stdin/stdout is a single duplex stream with no
//! request/response demuxing (`acpx-core::router`'s `read_matching_response`
//! doc comment) -- you cannot interleave two in-flight requests on one
//! child process's stdio regardless of locking strategy.

use crate::backoff;
use crate::framing::FramedWriter;
use crate::process::{BackendProcess, ProcessError, SpawnSpec};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// A supervised backend process, shared behind a per-process lock so
/// distinct agents' in-flight requests never contend with each other.
/// See this module's doc comment.
pub type SharedBackendProcess = Arc<Mutex<BackendProcess>>;

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("no spawn spec registered for agent {0}")]
    UnknownAgent(String),
    /// The agent's process recently crashed and is inside its backoff
    /// window -- callers should wait `retry_after` before calling
    /// `ensure_running` again rather than spin-looping respawn attempts.
    #[error("agent {agent_id} is in crash backoff, retry after {retry_after:?}")]
    Backoff {
        agent_id: String,
        retry_after: Duration,
    },
}

/// Liveness snapshot for one supervised agent, as reported by
/// [`Supervisor::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Registered (or not) but never successfully spawned yet.
    NotStarted,
    /// A process is currently running.
    Running,
    /// The most recently spawned process has exited, with its exit code if
    /// available (`None` if killed by a signal on unix).
    Exited { code: Option<i32> },
}

/// Per-agent restart bookkeeping: how many consecutive spawn attempts have
/// failed to stay alive past `backoff::STABLE_AFTER`, and when the most
/// recent spawn attempt happened (used both to gate the next attempt and to
/// detect that a running process has become "stable").
#[derive(Debug, Default, Clone, Copy)]
struct BackoffState {
    consecutive_failures: u32,
    last_spawn_at: Option<Instant>,
}

/// Tracks one supervised process per agent id, restarting it on crash with
/// exponential backoff (see `backoff.rs`) so a persistently-crashing
/// backend doesn't spin-loop respawn attempts.
pub struct Supervisor {
    specs: HashMap<String, SpawnSpec>,
    running: HashMap<String, SharedBackendProcess>,
    /// Independent clone of each running process's `writer` handle,
    /// captured at spawn time (see `BackendProcess::writer_handle`'s doc
    /// comment) so [`Self::cancel_writer`] can hand it out without ever
    /// touching `running`'s own per-process lock -- the entire point,
    /// see that method's doc comment. Kept in lockstep with `running`:
    /// inserted alongside every fresh spawn, removed on `stop`. A stale
    /// entry surviving past a crash the caller hasn't yet noticed just
    /// means a write into a dead process's closed stdin, which
    /// `FramedWriter::write_value` surfaces as a normal I/O error, not a
    /// hang or a panic.
    write_handles: HashMap<String, Arc<Mutex<FramedWriter>>>,
    attempts: HashMap<String, BackoffState>,
    /// How long a process must stay alive before its consecutive-failure
    /// count resets. Defaults to `backoff::STABLE_AFTER`; overridable via
    /// [`Supervisor::set_stable_after`], primarily so tests can exercise the
    /// reset path without waiting out the real default.
    stable_after: Duration,
}

/// Hard ceiling on how long [`Supervisor::stop`] will wait to acquire the
/// to-be-killed process's own per-process lock before giving up on doing
/// the kill inline and detaching that wait into a background task instead.
///
/// **Why this exists, not hypothetical.** `stop` is called from two
/// `acpx-core::Router` call sites that themselves run while their own
/// caller is holding the single global router mutex for the whole call:
/// `acpx-server`'s periodic lifecycle-reaper tick (`reap_unreferenced_
/// backends`, gated behind `connector_idle_shutdown_ttl`) and
/// `dispatch_proxied_shared`'s `session/close` handling (`stop_if_
/// session_scoped`, gated behind `ACPX_SESSION_PROCESS_ISOLATION=1`).
/// If the agent id being stopped currently has a stuck in-flight request
/// (`read_matching_response` blocked forever on a backend that stopped
/// responding -- the exact, previously-live incident `acpx-core::
/// router::REAP_BACKEND_CALL_TIMEOUT`'s doc comment describes), that
/// request's task is holding this same process's lock for as long as
/// it's stuck, so an unbounded `handle.lock().await` here would wedge
/// the caller's global router lock right along with it -- the identical
/// failure mode, one call site further down. Both gating features are
/// off by default in this deployment, so this closes a real but not
/// (yet) independently observed-live path, the same way the
/// `REAP_BACKEND_CALL_TIMEOUT` fix closed the one that was.
const STOP_LOCK_TIMEOUT: Duration = Duration::from_secs(15);

impl Supervisor {
    pub fn new() -> Self {
        Self {
            specs: HashMap::new(),
            running: HashMap::new(),
            write_handles: HashMap::new(),
            attempts: HashMap::new(),
            stable_after: backoff::STABLE_AFTER,
        }
    }

    pub fn register(&mut self, agent_id: impl Into<String>, spec: SpawnSpec) {
        self.specs.insert(agent_id.into(), spec);
    }

    /// Look up the currently-registered `SpawnSpec` for `agent_id`, if
    /// any. Lets a caller (namely `acpx-core::router`'s Phase 3 profile
    /// resolution) reuse an already-registered spec as a base -- e.g. a
    /// profile whose `agent_id` names something an operator (or a test)
    /// registered directly via [`Self::register`] rather than something
    /// resolved fresh from the ACP registry -- instead of mandating a
    /// registry lookup on every `session/new`.
    pub fn spec(&self, agent_id: &str) -> Option<&SpawnSpec> {
        self.specs.get(agent_id)
    }

    /// Override the "stayed alive long enough to reset backoff" threshold.
    /// Defaults to `backoff::STABLE_AFTER`.
    pub fn set_stable_after(&mut self, stable_after: Duration) {
        self.stable_after = stable_after;
    }

    /// Report whether `agent_id`'s process is currently running, has
    /// exited, or was never started. Non-blocking.
    ///
    /// Uses [`Mutex::try_lock`] rather than `.await`ing the per-process
    /// lock, so this stays a synchronous, non-blocking call even though
    /// the process handle is now shared: if some other caller currently
    /// holds the lock (mid request/response I/O), that's itself proof the
    /// process is alive and in use, so a contended lock is reported as
    /// `Running` rather than blocking here to find out for certain.
    pub fn status(&mut self, agent_id: &str) -> ProcessStatus {
        match self.running.get(agent_id) {
            None => ProcessStatus::NotStarted,
            Some(handle) => match handle.try_lock() {
                Ok(mut proc) => match proc.try_exit_status() {
                    Some(exit) => ProcessStatus::Exited { code: exit.code() },
                    None => ProcessStatus::Running,
                },
                Err(_) => ProcessStatus::Running,
            },
        }
    }

    /// Return the OS PID for a currently supervised backend.
    pub async fn process_id(&self, agent_id: &str) -> Option<u32> {
        let handle = self.running.get(agent_id)?.clone();
        let pid = handle.lock().await.id();
        pid
    }

    /// Ensure the named agent's backend process is running, spawning it if
    /// necessary (or if the previously-spawned process has exited).
    ///
    /// If the process has been crashing repeatedly, respawn attempts are
    /// throttled with exponential backoff (`backoff::next_delay`): calling
    /// this again while still inside the backoff window returns
    /// `SupervisorError::Backoff` instead of respawning immediately. A
    /// process that stays alive past `backoff::STABLE_AFTER` resets the
    /// consecutive-failure count back to zero.
    pub async fn ensure_running(
        &mut self,
        agent_id: &str,
    ) -> Result<SharedBackendProcess, SupervisorError> {
        if !self.specs.contains_key(agent_id) {
            return Err(SupervisorError::UnknownAgent(agent_id.to_string()));
        }
        let now = Instant::now();

        // Check liveness first and drop the borrow immediately (as an owned
        // bool) rather than holding a lock guard across the branches below.
        // **Must be `try_lock`, never `handle.lock().await`**: every real
        // caller of `ensure_running` (`dispatch_proxied_shared`,
        // `dispatch_session_new_shared` in acpx-core/src/router.rs) calls
        // it from inside a block that still holds *this router's own*
        // outer `Arc<Mutex<Router>>` guard -- so if this awaited the
        // per-process lock and lost the race to a concurrent in-flight
        // `session/prompt` (which legitimately holds that same lock for
        // its *entire* turn on the legacy, non-`process_reader_demux`
        // path), the wait here would transitively hold the *router-wide*
        // mutex hostage for that whole turn -- freezing every other
        // client on this daemon (a different agent's `session/prompt`,
        // `/health`, everything funnels through
        // `dispatch_shared_for_tenant`'s `router.lock().await` first).
        // Reproduced live: two sessions sharing one agent with demux off,
        // one WS-connected session's `session/new` blocked, and `/health`
        // on an unrelated connection went fully unresponsive for the same
        // window (`process_reader_demux_cancel_and_live_updates_test.rs`'s
        // `demux_off_a_second_sessions_launch_and_live_updates_stall_
        // behind_first_sessions_turn` pins the fixed, no-longer-server-
        // wide behavior). `try_lock`'s `Err` case (lock currently held by
        // someone else) is treated as "still running" -- correct, since a
        // process a concurrent caller is actively doing I/O against right
        // now cannot have silently exited out from under it; if it did,
        // that caller's own read/write would itself error and this
        // process gets removed from `running` and respawned on its own
        // next call, same as any other backend I/O error path.
        let exited = match self.running.get(agent_id) {
            Some(handle) => match handle.try_lock() {
                Ok(mut guard) => Some(guard.has_exited()),
                Err(_) => Some(false),
            },
            None => None,
        };

        match exited {
            Some(false) => {
                // Still running -- opportunistically reset backoff once the
                // process has proven itself stable.
                if let Some(state) = self.attempts.get_mut(agent_id) {
                    if let Some(last_spawn) = state.last_spawn_at {
                        if now.duration_since(last_spawn) >= self.stable_after {
                            state.consecutive_failures = 0;
                        }
                    }
                }
                return Ok(Arc::clone(
                    self.running
                        .get(agent_id)
                        .expect("checked Some(false) above"),
                ));
            }
            Some(true) => {
                self.running.remove(agent_id);
                let state = self.attempts.entry(agent_id.to_string()).or_default();
                // If the crashed process had already run past the
                // stability threshold, treat this as an isolated crash
                // rather than inflating the backoff for what was otherwise
                // a healthy run.
                let survived_before_crash = state
                    .last_spawn_at
                    .map(|last| now.duration_since(last) >= self.stable_after)
                    .unwrap_or(false);
                state.consecutive_failures = if survived_before_crash {
                    0
                } else {
                    state.consecutive_failures.saturating_add(1)
                };
            }
            None => {}
        }

        let state = self.attempts.entry(agent_id.to_string()).or_default();
        if state.consecutive_failures > 0 {
            let delay = backoff::next_delay(state.consecutive_failures);
            if let Some(last_spawn) = state.last_spawn_at {
                let ready_at = last_spawn + delay;
                if now < ready_at {
                    return Err(SupervisorError::Backoff {
                        agent_id: agent_id.to_string(),
                        retry_after: ready_at - now,
                    });
                }
            }
        }

        let spec = self
            .specs
            .get(agent_id)
            .expect("checked contains_key above");
        state.last_spawn_at = Some(now);
        match BackendProcess::spawn(spec).await {
            Ok(proc) => {
                // Captured *before* `proc` is ever wrapped in its own
                // `Arc<Mutex<BackendProcess>>` and handed out -- so this
                // clone is unconditionally cheap and uncontended, never a
                // point where this could itself block on a lock some
                // other caller already holds. See `write_handles`'s and
                // `BackendProcess::writer`'s doc comments.
                self.write_handles
                    .insert(agent_id.to_string(), proc.writer_handle());
                let handle: SharedBackendProcess = Arc::new(Mutex::new(proc));
                self.running
                    .insert(agent_id.to_string(), Arc::clone(&handle));
                Ok(handle)
            }
            Err(e) => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                Err(SupervisorError::Process(e))
            }
        }
    }

    pub async fn stop(&mut self, agent_id: &str) -> Result<(), SupervisorError> {
        if let Some(handle) = self.running.remove(agent_id) {
            self.write_handles.remove(agent_id);
            // Waits for any in-flight request against this agent to finish
            // before killing it, rather than yanking the process out from
            // under a concurrent caller mid-I/O -- but only up to
            // `STOP_LOCK_TIMEOUT`: see that constant's doc comment for why
            // waiting unboundedly here is a real deadlock risk for any
            // caller holding a broader lock (e.g. the global router mutex)
            // around this call, not just a theoretical concern.
            match tokio::time::timeout(STOP_LOCK_TIMEOUT, handle.clone().lock_owned()).await {
                Ok(mut proc) => {
                    proc.kill().await.map_err(ProcessError::Spawn)?;
                }
                Err(_) => {
                    tracing::warn!(
                        %agent_id,
                        timeout_secs = STOP_LOCK_TIMEOUT.as_secs(),
                        "stop: backend process lock is still held by a stuck in-flight \
                         request; detaching the kill into a background task instead of \
                         blocking this call (and whatever lock its caller holds) \
                         indefinitely -- the process will still be killed once that \
                         stuck request eventually releases the lock"
                    );
                    tokio::spawn(async move {
                        let mut proc = handle.lock().await;
                        if let Err(error) = proc.kill().await {
                            tracing::warn!(
                                %error,
                                "detached background kill (after a stop() lock timeout) \
                                 also failed"
                            );
                        }
                    });
                }
            }
        }
        // An intentional stop isn't a crash -- clear backoff bookkeeping so
        // a subsequent `ensure_running` spawns immediately.
        self.attempts.remove(agent_id);
        Ok(())
    }

    /// Stop every running process whose supervisor key starts with
    /// `prefix`. This lets profile deletion clean up both shared and
    /// tenant-qualified profile process keys.
    pub async fn stop_prefix(&mut self, prefix: &str) -> Result<(), SupervisorError> {
        let keys: Vec<String> = self
            .running
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect();
        for key in keys {
            self.stop(&key).await?;
        }
        Ok(())
    }

    /// Real ACP `session/cancel` support's key primitive: an independent
    /// clone of `agent_id`'s currently-running process's writer handle,
    /// obtainable *without* ever touching that process's own per-process
    /// lock (`SharedBackendProcess`'s `Arc<Mutex<BackendProcess>>`) --
    /// see `write_handles`'s and `BackendProcess::writer`'s doc comments
    /// for why that matters: a `session/prompt` call already in flight
    /// against this exact process holds that per-process lock for its
    /// entire duration, so anything routed through it (the pre-phase-7
    /// behavior) can't ever deliver a cancel notification until *after*
    /// the very call it was meant to interrupt has already finished --
    /// at which point cancelling is moot. `None` if `agent_id` has never
    /// been spawned (or was `stop`ped) -- a caller with nothing to
    /// cancel, not an error in itself.
    pub fn cancel_writer(&self, agent_id: &str) -> Option<Arc<Mutex<FramedWriter>>> {
        self.write_handles.get(agent_id).cloned()
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
