//! Durable custom ACP-agent definitions owned by the admin plane.

use crate::persistence::{PersistenceError, PersistenceStore};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CustomAgent {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CustomAgentStoreError {
    #[error("custom agent {0} already exists")]
    AlreadyExists(String),
    #[error("custom agent {0} was not found")]
    NotFound(String),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

#[derive(Clone)]
pub struct CustomAgentStore {
    persistence: PersistenceStore,
}

impl CustomAgentStore {
    pub fn new(persistence: PersistenceStore) -> Self {
        Self { persistence }
    }

    pub(crate) async fn create(&self, agent: CustomAgent) -> Result<(), CustomAgentStoreError> {
        self.persistence
            .create_custom_agent(agent)
            .await
            .map_err(|error| match error {
                PersistenceError::CustomAgentAlreadyExists(id) => {
                    CustomAgentStoreError::AlreadyExists(id)
                }
                other => CustomAgentStoreError::Persistence(other),
            })
    }

    pub async fn list(&self) -> Result<Vec<CustomAgent>, CustomAgentStoreError> {
        Ok(self.persistence.list_custom_agents().await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<CustomAgent>, CustomAgentStoreError> {
        Ok(self.persistence.get_custom_agent(id).await?)
    }

    pub(crate) async fn delete(&self, id: &str) -> Result<(), CustomAgentStoreError> {
        self.persistence
            .delete_custom_agent(id)
            .await
            .map_err(|error| match error {
                PersistenceError::CustomAgentNotFound(id) => CustomAgentStoreError::NotFound(id),
                other => CustomAgentStoreError::Persistence(other),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent(id: &str) -> CustomAgent {
        CustomAgent {
            id: id.to_owned(),
            name: "Test ACP Agent".to_owned(),
            command: "acp-test-agent".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::from([("ACP_TEST".to_owned(), "1".to_owned())]),
            cwd: Some("/tmp/acpx-test".to_owned()),
        }
    }

    #[tokio::test]
    async fn creates_lists_gets_and_deletes_custom_agents() {
        let store = PersistenceStore::open_in_memory().expect("in-memory database");
        let agents = CustomAgentStore::new(store);
        let agent = test_agent("custom-test");

        agents.create(agent.clone()).await.expect("create agent");
        assert_eq!(
            agents.get("custom-test").await.expect("get agent"),
            Some(agent.clone())
        );
        assert_eq!(
            agents.list().await.expect("list agents"),
            vec![agent.clone()]
        );

        let duplicate = agents
            .create(agent)
            .await
            .expect_err("duplicate custom id is rejected");
        assert!(matches!(
            duplicate,
            CustomAgentStoreError::AlreadyExists(ref id) if id == "custom-test"
        ));

        agents.delete("custom-test").await.expect("delete agent");
        assert_eq!(
            agents.get("custom-test").await.expect("get deleted agent"),
            None
        );
        assert!(matches!(
            agents
                .delete("custom-test")
                .await
                .expect_err("deleting a missing agent fails"),
            CustomAgentStoreError::NotFound(ref id) if id == "custom-test"
        ));
    }

    #[tokio::test]
    async fn custom_agents_survive_store_reopen() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = directory.path().join("acpx.sqlite");
        let agent = test_agent("persisted-custom");

        {
            let store = PersistenceStore::open(&database).expect("open database");
            CustomAgentStore::new(store)
                .create(agent.clone())
                .await
                .expect("create agent");
        }

        let store = PersistenceStore::open(&database).expect("reopen database");
        assert_eq!(
            CustomAgentStore::new(store)
                .get("persisted-custom")
                .await
                .expect("read persisted agent"),
            Some(agent)
        );
    }
}
