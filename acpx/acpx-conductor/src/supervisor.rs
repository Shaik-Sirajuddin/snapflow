//! N supervised backend processes keyed by agent name, with
//! restart-on-crash + backoff, per `04-phased-plan.md` Phase 2 step 5.

use crate::backoff;
use crate::process::{BackendProcess, ProcessError, SpawnSpec};
use std::collections::HashMap;
use std::time::{Duration, Instant};

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
    running: HashMap<String, BackendProcess>,
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
            attempts: HashMap::new(),
            stable_after: backoff::STABLE_AFTER,
        }
    }

    pub fn register(&mut self, agent_id: impl Into<String>, spec: SpawnSpec) {
        self.specs.insert(agent_id.into(), spec);
    }

    /// Override the "stayed alive long enough to reset backoff" threshold.
    /// Defaults to `backoff::STABLE_AFTER`.
    pub fn set_stable_after(&mut self, stable_after: Duration) {
        self.stable_after = stable_after;
    }

    /// Report whether `agent_id`'s process is currently running, has
    /// exited, or was never started. Non-blocking.
    pub fn status(&mut self, agent_id: &str) -> ProcessStatus {
        match self.running.get_mut(agent_id) {
            None => ProcessStatus::NotStarted,
            Some(proc) => match proc.try_exit_status() {
                Some(exit) => ProcessStatus::Exited { code: exit.code() },
                None => ProcessStatus::Running,
            },
        }
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
    ) -> Result<&mut BackendProcess, SupervisorError> {
        if !self.specs.contains_key(agent_id) {
            return Err(SupervisorError::UnknownAgent(agent_id.to_string()));
        }
        let now = Instant::now();

        // Check liveness first and drop the borrow immediately (as an owned
        // bool) rather than holding it across the branches below -- keeps
        // this simple for the borrow checker instead of threading a `&mut
        // BackendProcess` through conditional early returns.
        let exited = self.running.get_mut(agent_id).map(|proc| proc.has_exited());

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
                return Ok(self
                    .running
                    .get_mut(agent_id)
                    .expect("checked Some(false) above"));
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
                self.running.insert(agent_id.to_string(), proc);
                Ok(self.running.get_mut(agent_id).expect("just inserted"))
            }
            Err(e) => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                Err(SupervisorError::Process(e))
            }
        }
    }

    pub async fn stop(&mut self, agent_id: &str) -> Result<(), SupervisorError> {
        if let Some(mut proc) = self.running.remove(agent_id) {
            proc.kill().await.map_err(ProcessError::Spawn)?;
        }
        // An intentional stop isn't a crash -- clear backoff bookkeeping so
        // a subsequent `ensure_running` spawns immediately.
        self.attempts.remove(agent_id);
        Ok(())
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
