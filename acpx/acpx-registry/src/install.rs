//! Install-step execution. Phase 0/1 stub -- real npx/binary install paths
//! land in Phase 4 (`04-phased-plan.md` step 19).

use crate::index::Agent;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("agent {0} declares no supported distribution method")]
    NoDistribution(String),
    #[error("not yet implemented")]
    NotImplemented,
}

/// Resolve and run the install step for one agent's preferred distribution
/// method. Stubbed until Phase 4.
pub async fn install(agent: &Agent) -> Result<(), InstallError> {
    agent
        .distribution
        .preferred_method()
        .ok_or_else(|| InstallError::NoDistribution(agent.id.clone()))?;
    Err(InstallError::NotImplemented)
}
