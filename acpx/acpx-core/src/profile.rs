//! Profile store: CRUD for {agent, provider, key-ref, launch overrides,
//! attached MCP servers}. Phase 3 step 14.
//!
//! A `Profile` is the thing `session/new`'s `_acpx.profile` names --
//! `crate::router::Router` resolves it to an agent id + provider config +
//! resolved key (via `crate::provider::ProviderStore` /
//! `crate::keystore::Keystore`) and a `SpawnSpec` (via `crate::launch`),
//! per `02-architecture.md`'s "managed mode" description. Omitting
//! `_acpx.profile` entirely stays native/unmanaged -- this store is never
//! consulted for that path, so its existence is a no-op for a client that
//! never opts in.

use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    /// Which registry-listed agent (e.g. `codex-acp`, `claude-agent-acp`)
    /// this profile launches.
    pub agent_id: String,
    /// Provider name, resolved against `ProviderStore` at spawn time.
    /// `None` means "launch the agent with no provider env overrides" --
    /// still a distinct, explicitly-requested process from native mode
    /// (e.g. useful for `launch_overrides`-only profiles), not the same as
    /// omitting `_acpx.profile` altogether.
    pub provider: Option<String>,
    /// Which stored key (via `crate::keystore::Keystore`) to resolve and
    /// inject alongside `provider`. `None` with `Some(provider)` set is
    /// valid (e.g. an agent already logged in natively but pointed at a
    /// custom `base_url`).
    pub key_ref: Option<crate::keystore::KeyRef>,
    /// Extra env vars layered on top of whatever `crate::launch` derives
    /// from `provider`/`key_ref` -- profile-specific escape hatch, applied
    /// last so a profile can always override the derived defaults.
    pub launch_overrides: HashMap<String, String>,
    /// Names of centrally-registered MCP servers (see
    /// `crate::mcp_servers`) to auto-attach at `session/new`, merged with
    /// whatever the client itself sent (client wins on name collision --
    /// see `crate::mcp_servers::merge_mcp_servers`).
    pub mcp_servers: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileStoreError {
    #[error("profile {0} already exists")]
    AlreadyExists(String),
    #[error("no profile named {0}")]
    NotFound(String),
}

/// In-memory CRUD store for [`Profile`]s, keyed by `name`. See
/// `crate::provider::ProviderStore`'s doc comment for why this isn't
/// sqlite-persisted (yet).
#[derive(Debug, Default)]
pub struct ProfileStore {
    profiles: HashMap<String, Profile>,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, profile: Profile) -> Result<(), ProfileStoreError> {
        if self.profiles.contains_key(&profile.name) {
            return Err(ProfileStoreError::AlreadyExists(profile.name));
        }
        self.profiles.insert(profile.name.clone(), profile);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn list(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }

    pub fn update(&mut self, profile: Profile) -> Result<(), ProfileStoreError> {
        if !self.profiles.contains_key(&profile.name) {
            return Err(ProfileStoreError::NotFound(profile.name));
        }
        self.profiles.insert(profile.name.clone(), profile);
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), ProfileStoreError> {
        self.profiles
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| ProfileStoreError::NotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        Profile {
            name: "work-openai".to_string(),
            agent_id: "codex-acp".to_string(),
            provider: Some("openai-default".to_string()),
            key_ref: None,
            launch_overrides: HashMap::new(),
            mcp_servers: vec![],
        }
    }

    #[test]
    fn create_then_get_round_trips() {
        let mut store = ProfileStore::new();
        store.create(sample()).unwrap();
        assert_eq!(store.get("work-openai").unwrap().agent_id, "codex-acp");
    }

    #[test]
    fn create_twice_errors() {
        let mut store = ProfileStore::new();
        store.create(sample()).unwrap();
        assert_eq!(
            store.create(sample()),
            Err(ProfileStoreError::AlreadyExists("work-openai".to_string()))
        );
    }

    #[test]
    fn update_missing_errors() {
        let mut store = ProfileStore::new();
        assert_eq!(
            store.update(sample()),
            Err(ProfileStoreError::NotFound("work-openai".to_string()))
        );
    }

    #[test]
    fn delete_then_get_returns_none() {
        let mut store = ProfileStore::new();
        store.create(sample()).unwrap();
        store.delete("work-openai").unwrap();
        assert!(store.get("work-openai").is_none());
    }

    #[test]
    fn list_returns_every_profile() {
        let mut store = ProfileStore::new();
        store.create(sample()).unwrap();
        store
            .create(Profile {
                name: "personal-anthropic".to_string(),
                agent_id: "claude-agent-acp".to_string(),
                ..sample()
            })
            .unwrap();
        assert_eq!(store.list().count(), 2);
    }
}
