//! N supervised backend processes keyed by agent name, with
//! restart-on-crash + backoff. Phase 1 only needs a single hardcoded
//! process (see `acpx-server`'s Phase 1 passthrough); this generalizes in
//! Phase 2 step 5.

use crate::process::{BackendProcess, ProcessError, SpawnSpec};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("no spawn spec registered for agent {0}")]
    UnknownAgent(String),
}

/// Tracks one supervised process per agent id. Phase 1: no restart/backoff
/// logic yet, just ensure-running + lookup. Phase 2 adds crash detection
/// with backoff per `04-phased-plan.md` step 5.
pub struct Supervisor {
    specs: HashMap<String, SpawnSpec>,
    running: HashMap<String, BackendProcess>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            specs: HashMap::new(),
            running: HashMap::new(),
        }
    }

    pub fn register(&mut self, agent_id: impl Into<String>, spec: SpawnSpec) {
        self.specs.insert(agent_id.into(), spec);
    }

    /// Ensure the named agent's backend process is running, spawning it if
    /// necessary (or if the previously-spawned process has exited).
    pub async fn ensure_running(
        &mut self,
        agent_id: &str,
    ) -> Result<&mut BackendProcess, SupervisorError> {
        let needs_spawn = match self.running.get_mut(agent_id) {
            Some(proc) => proc.has_exited(),
            None => true,
        };
        if needs_spawn {
            let spec = self
                .specs
                .get(agent_id)
                .ok_or_else(|| SupervisorError::UnknownAgent(agent_id.to_string()))?;
            let proc = BackendProcess::spawn(spec).await?;
            self.running.insert(agent_id.to_string(), proc);
        }
        Ok(self.running.get_mut(agent_id).expect("just inserted"))
    }

    pub async fn stop(&mut self, agent_id: &str) -> Result<(), SupervisorError> {
        if let Some(mut proc) = self.running.remove(agent_id) {
            proc.kill().await.map_err(ProcessError::Spawn)?;
        }
        Ok(())
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
