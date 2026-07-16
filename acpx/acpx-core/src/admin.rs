//! Sole mutation facade for durable agent administration state.

use crate::{AgentEnablement, CustomAgent, CustomAgentStore, CustomAgentStoreError};
use std::collections::BTreeSet;

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("custom agent id {0} collides with a registry agent")]
    RegistryIdCollision(String),
    #[error("agent {0} is not a registry or custom agent")]
    UnknownAgent(String),
    #[error("custom agent {field} is invalid: {reason}")]
    InvalidCustomAgent {
        field: &'static str,
        reason: &'static str,
    },
    #[error(transparent)]
    CustomAgent(#[from] CustomAgentStoreError),
    #[error(transparent)]
    Persistence(#[from] crate::PersistenceError),
}

#[derive(Clone)]
pub struct AdminOps {
    enablement: AgentEnablement,
    custom_agents: CustomAgentStore,
    registry_agent_ids: BTreeSet<String>,
}

impl AdminOps {
    pub fn new(
        enablement: AgentEnablement,
        custom_agents: CustomAgentStore,
        registry_agent_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            enablement,
            custom_agents,
            registry_agent_ids: registry_agent_ids.into_iter().collect(),
        }
    }

    pub async fn set_enabled(
        &self,
        agent_id: impl Into<String>,
        enabled: bool,
    ) -> Result<(), AdminError> {
        let agent_id = agent_id.into();
        if !self.registry_agent_ids.contains(&agent_id)
            && self.custom_agents.get(&agent_id).await?.is_none()
        {
            return Err(AdminError::UnknownAgent(agent_id));
        }
        self.enablement.set_enabled(agent_id, enabled).await?;
        Ok(())
    }

    pub async fn create_custom_agent(&self, agent: CustomAgent) -> Result<(), AdminError> {
        validate_custom_agent(&agent)?;
        if self.registry_agent_ids.contains(&agent.id) {
            return Err(AdminError::RegistryIdCollision(agent.id));
        }
        self.custom_agents.create(agent).await?;
        Ok(())
    }

    pub async fn delete_custom_agent(&self, id: &str) -> Result<(), AdminError> {
        self.custom_agents.delete(id).await?;
        Ok(())
    }
}

fn validate_custom_agent(agent: &CustomAgent) -> Result<(), AdminError> {
    if agent.id.is_empty() {
        return Err(AdminError::InvalidCustomAgent {
            field: "id",
            reason: "must not be empty",
        });
    }
    if !agent
        .id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AdminError::InvalidCustomAgent {
            field: "id",
            reason: "must contain only ASCII letters, digits, '-', '_', or '.'",
        });
    }
    if agent.name.trim().is_empty() {
        return Err(AdminError::InvalidCustomAgent {
            field: "name",
            reason: "must not be blank",
        });
    }
    if agent.command.trim().is_empty() {
        return Err(AdminError::InvalidCustomAgent {
            field: "command",
            reason: "must not be blank",
        });
    }
    if agent
        .cwd
        .as_deref()
        .is_some_and(|cwd| cwd.trim().is_empty())
    {
        return Err(AdminError::InvalidCustomAgent {
            field: "cwd",
            reason: "must not be blank when set",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PersistenceStore;
    use std::collections::BTreeMap;

    fn test_agent(id: &str) -> CustomAgent {
        CustomAgent {
            id: id.to_owned(),
            name: "Test ACP Agent".to_owned(),
            command: "acp-test-agent".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn rejects_registry_collisions_without_mutating_custom_agents() {
        let persistence = PersistenceStore::open_in_memory().expect("in-memory database");
        let custom_agents = CustomAgentStore::new(persistence.clone());
        let admin = AdminOps::new(
            AgentEnablement::new(persistence),
            custom_agents.clone(),
            ["claude".to_owned()],
        );

        let error = admin
            .create_custom_agent(test_agent("claude"))
            .await
            .expect_err("registry id collision is rejected");
        assert!(matches!(
            error,
            AdminError::RegistryIdCollision(ref id) if id == "claude"
        ));
        assert_eq!(
            custom_agents.list().await.expect("list custom agents"),
            Vec::<CustomAgent>::new()
        );
    }

    #[tokio::test]
    async fn rejects_unknown_enablement_and_clears_deleted_custom_state() {
        let persistence = PersistenceStore::open_in_memory().expect("in-memory database");
        let enablement = AgentEnablement::new(persistence.clone());
        let custom_agents = CustomAgentStore::new(persistence);
        let admin = AdminOps::new(enablement.clone(), custom_agents, ["claude".to_owned()]);

        assert!(matches!(
            admin
                .set_enabled("does-not-exist", false)
                .await
                .expect_err("unknown agent is rejected"),
            AdminError::UnknownAgent(ref id) if id == "does-not-exist"
        ));

        admin
            .create_custom_agent(test_agent("reusable"))
            .await
            .expect("create custom agent");
        admin
            .set_enabled("reusable", false)
            .await
            .expect("disable custom agent");
        admin
            .delete_custom_agent("reusable")
            .await
            .expect("delete custom agent");
        admin
            .create_custom_agent(test_agent("reusable"))
            .await
            .expect("recreate custom agent");
        assert!(enablement
            .is_enabled("reusable")
            .await
            .expect("recreated custom agent defaults to enabled"));
    }

    #[test]
    fn rejects_malformed_custom_agent_definitions() {
        let mut agent = test_agent("valid-id");
        agent.id = "not valid".to_owned();
        assert!(matches!(
            validate_custom_agent(&agent),
            Err(AdminError::InvalidCustomAgent { field: "id", .. })
        ));

        agent = test_agent("valid-id");
        agent.command = "  ".to_owned();
        assert!(matches!(
            validate_custom_agent(&agent),
            Err(AdminError::InvalidCustomAgent {
                field: "command",
                ..
            })
        ));
    }
}
