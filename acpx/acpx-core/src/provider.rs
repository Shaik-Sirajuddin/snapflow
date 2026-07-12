//! Provider config model: `openai` / `anthropic` / `litellm` endpoints.
//! Phase 3 step 12. Keys themselves live in [`crate::keystore`], referenced
//! from a [`crate::profile::Profile`] by [`crate::keystore::KeyRef`] rather
//! than embedded here -- a `ProviderConfig` only describes *where* to send
//! requests, never *what key* to send with them (that pairing happens at
//! profile-resolution time, see `crate::router`'s `session/new` handling).

use std::collections::HashMap;

/// Which backend API surface a provider speaks. Drives
/// [`crate::launch`]'s choice of which env vars to inject into a spawned
/// backend process -- `OpenAi`/`LiteLlm` both route through `codex-acp`'s
/// OpenAI-compatible config surface (`CODEX_API_KEY`/`CODEX_CONFIG`),
/// `Anthropic` routes through `claude-agent-acp`'s
/// `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` (see `01-research.md` and
/// `05-open-risks.md` for the unverified-until-now config surfaces this is
/// based on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    LiteLlm,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    /// Custom endpoint, e.g. a litellm proxy or an OpenAI-compatible
    /// gateway. `None` means "provider's own default endpoint" (real
    /// `api.openai.com`/`api.anthropic.com`) -- only `LiteLlm` and
    /// self-hosted/proxy `OpenAi`/`Anthropic` setups need this set.
    pub base_url: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProviderStoreError {
    #[error("provider {0} already exists")]
    AlreadyExists(String),
    #[error("no provider named {0}")]
    NotFound(String),
}

/// In-memory CRUD store for [`ProviderConfig`]s, keyed by `name`. Not
/// persisted to sqlite (unlike sessions/transcripts) -- provider/profile
/// config is gateway startup/runtime configuration, not session history;
/// see `05-open-risks.md`'s key-storage-mechanism note for why secrets
/// (this store's `base_url`s are not secret, but paired keys are) stay out
/// of the same persistence path as transcripts for now.
#[derive(Debug, Default)]
pub struct ProviderStore {
    providers: HashMap<String, ProviderConfig>,
}

impl ProviderStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, config: ProviderConfig) -> Result<(), ProviderStoreError> {
        if self.providers.contains_key(&config.name) {
            return Err(ProviderStoreError::AlreadyExists(config.name));
        }
        self.providers.insert(config.name.clone(), config);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    pub fn list(&self) -> impl Iterator<Item = &ProviderConfig> {
        self.providers.values()
    }

    pub fn update(&mut self, config: ProviderConfig) -> Result<(), ProviderStoreError> {
        if !self.providers.contains_key(&config.name) {
            return Err(ProviderStoreError::NotFound(config.name));
        }
        self.providers.insert(config.name.clone(), config);
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), ProviderStoreError> {
        self.providers
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| ProviderStoreError::NotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn litellm() -> ProviderConfig {
        ProviderConfig {
            name: "my-litellm".to_string(),
            kind: ProviderKind::LiteLlm,
            base_url: Some("https://litellm.example.com/v1".to_string()),
        }
    }

    #[test]
    fn create_then_get_round_trips() {
        let mut store = ProviderStore::new();
        store.create(litellm()).unwrap();
        assert_eq!(store.get("my-litellm").unwrap().kind, ProviderKind::LiteLlm);
    }

    #[test]
    fn create_twice_errors() {
        let mut store = ProviderStore::new();
        store.create(litellm()).unwrap();
        assert_eq!(
            store.create(litellm()),
            Err(ProviderStoreError::AlreadyExists("my-litellm".to_string()))
        );
    }

    #[test]
    fn update_missing_errors() {
        let mut store = ProviderStore::new();
        assert_eq!(
            store.update(litellm()),
            Err(ProviderStoreError::NotFound("my-litellm".to_string()))
        );
    }

    #[test]
    fn delete_then_get_returns_none() {
        let mut store = ProviderStore::new();
        store.create(litellm()).unwrap();
        store.delete("my-litellm").unwrap();
        assert!(store.get("my-litellm").is_none());
    }

    #[test]
    fn list_returns_every_provider() {
        let mut store = ProviderStore::new();
        store.create(litellm()).unwrap();
        store
            .create(ProviderConfig {
                name: "openai-default".to_string(),
                kind: ProviderKind::OpenAi,
                base_url: None,
            })
            .unwrap();
        assert_eq!(store.list().count(), 2);
    }
}
