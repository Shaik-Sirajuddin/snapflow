//! Durable enabled/disabled state for registry and custom agents.

use crate::persistence::{PersistenceError, PersistenceStore};

#[derive(Clone)]
pub struct AgentEnablement {
    persistence: PersistenceStore,
}

impl AgentEnablement {
    pub fn new(persistence: PersistenceStore) -> Self {
        Self { persistence }
    }

    /// Agents default to enabled until an administrator persists an override.
    pub async fn is_enabled(&self, agent_id: &str) -> Result<bool, PersistenceError> {
        Ok(self
            .persistence
            .agent_enabled(agent_id)
            .await?
            .unwrap_or(true))
    }

    pub(crate) async fn set_enabled(
        &self,
        agent_id: impl Into<String>,
        enabled: bool,
    ) -> Result<(), PersistenceError> {
        self.persistence.set_agent_enabled(agent_id, enabled).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults_to_enabled_and_persists_overrides_across_reopen() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = directory.path().join("acpx.sqlite");

        {
            let store = PersistenceStore::open(&database).expect("open database");
            let enablement = AgentEnablement::new(store);
            assert!(enablement
                .is_enabled("registry-claude")
                .await
                .expect("default enablement"));

            enablement
                .set_enabled("registry-claude", false)
                .await
                .expect("disable agent");
            assert!(!enablement
                .is_enabled("registry-claude")
                .await
                .expect("disabled state"));
        }

        let store = PersistenceStore::open(&database).expect("reopen database");
        let enablement = AgentEnablement::new(store);
        assert!(!enablement
            .is_enabled("registry-claude")
            .await
            .expect("persisted disabled state"));

        enablement
            .set_enabled("registry-claude", true)
            .await
            .expect("enable agent");
        assert!(enablement
            .is_enabled("registry-claude")
            .await
            .expect("persisted enabled state"));
    }
}
