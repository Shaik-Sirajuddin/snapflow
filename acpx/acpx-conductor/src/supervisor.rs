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
        // Locking here is a brief, uncontended (in the common case) check,
        // never held across an `.await` on backend I/O -- the only other
        // holder of this same per-process lock would be a concurrent
        // request already in flight against this exact agent, in which
        // case blocking briefly to confirm liveness is correct anyway
        // (see `status`'s doc comment for the non-blocking variant used
        // there instead).
        let exited = match self.running.get(agent_id) {
            Some(handle) => Some(handle.lock().await.has_exited()),
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
            // under a concurrent caller mid-I/O.
            let mut proc = handle.lock().await;
            proc.kill().await.map_err(ProcessError::Spawn)?;
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
