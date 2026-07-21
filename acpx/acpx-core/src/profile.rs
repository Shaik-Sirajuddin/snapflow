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
    /// Where this profile came from -- see [`ProfileSource`]'s own doc
    /// comment. Consulted by the `/acp` bridge's model-discovery seed
    /// (`acpx-server`'s `refresh_models_with_config`) to decide whether
    /// this profile's `agent_id` is eligible to be probed/exposed as a
    /// selectable model: only [`ProfileSource::Provisioned`] profiles
    /// are, so installing a new CLI agent on the host never silently
    /// widens what a strict-ACP client like Zed can select without an
    /// operator explicitly provisioning it (`ACPX_CONFIG_FILE` or
    /// `profiles/create`). `#[serde(default)]` (defaulting to
    /// `Provisioned`) so every pre-existing persisted/provisioned
    /// `Profile` JSON that predates this field parses unchanged.
    #[serde(default)]
    pub source: ProfileSource,
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
    /// How to answer a real ACP `session/request_permission` request from
    /// this profile's backend -- see [`PermissionPolicy`]'s own doc
    /// comment. `#[serde(default)]` so every pre-existing profile JSON
    /// (persisted or provisioned) that predates this field parses
    /// unchanged, defaulting to the conservative `AutoReject`.
    pub permission_policy: PermissionPolicy,
    /// Whether this profile's backend is allowed real `fs/read_text_file`/
    /// `fs/write_text_file` access to acpx's own host filesystem.
    /// Default `false` (declared `false`/`false` in the `initialize`
    /// handshake's `clientCapabilities.fs`, and any attempt anyway gets a
    /// capability-not-enabled error rather than performing I/O) -- a
    /// backend process being able to read/write arbitrary paths on
    /// whatever host is running acpx is a real, meaningfully dangerous
    /// default to ship opt-out rather than opt-in, unlike
    /// `permission_policy` (which only ever picks among options the
    /// backend itself offered). See `router::read_matching_response`'s
    /// `fs/read_text_file`/`fs/write_text_file` handling.
    #[serde(default)]
    pub allow_fs_access: bool,
    /// Whether this profile's backend is allowed to run real
    /// `terminal/*` commands (`create`/`output`/`wait_for_exit`/`kill`/
    /// `release`) on acpx's own host -- arbitrary command execution, an
    /// even more direct risk than `allow_fs_access`'s read/write. Same
    /// opt-in-not-opt-out default (`false`) and same reasoning. See
    /// `router::read_matching_response`'s `terminal/*` handling.
    #[serde(default)]
    pub allow_terminal_access: bool,
    /// Pre-configured ACP `authenticate` method id to use for this
    /// profile's backend, if that backend's `initialize` response
    /// advertises a non-empty `authMethods` list (e.g. `"api-key"`,
    /// `"oauth-personal"` -- whatever id the backend itself offered;
    /// acpx doesn't invent or validate one, it just passes it through
    /// verbatim as `authenticate`'s `params.methodId` per the real ACP
    /// schema). `None` (the default) means "this profile has nothing
    /// pre-configured" -- a backend that requires auth then gets a
    /// clear, immediate `RouterError::BackendRequiresAuthentication`
    /// (naming every method id the backend offered) instead of acpx
    /// either hanging, guessing a method id, or silently proceeding to
    /// `session/new` and letting the backend's own rejection surface as
    /// an opaque downstream error. There is deliberately no live,
    /// interactive "ask the user which auth method and for what
    /// credential" flow here -- same honest limitation as
    /// `permission_policy`/`allow_fs_access`/`allow_terminal_access`,
    /// see `router::ensure_backend_initialized`'s doc comment.
    #[serde(default)]
    pub auth_method_id: Option<String>,
}

/// Distinguishes a profile an operator/client explicitly asked for from
/// one `Router::ensure_default_profiles_seeded` auto-fills so every
/// installed registry agent has *some* name to bind to. Both are equally
/// valid for ACPX's own native protocol (that auto-fill exists precisely
/// so `_acpx.profile` never requires setup for the common case) -- this
/// distinction only matters to consumers, like the `/acp` bridge, that
/// need a deliberately curated subset rather than "every CLI this host
/// happens to have installed."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    /// Created via `profiles/create`/`profiles/update` (including
    /// `ACPX_CONFIG_FILE` startup provisioning, which goes through the
    /// same call).
    #[default]
    Provisioned,
    /// Auto-filled by `ensure_default_profiles_seeded` for an installed
    /// agent nobody has explicitly named a profile for yet.
    AutoSeeded,
}

/// How `crate::router` answers a backend's `session/request_permission`
/// request on this profile's behalf. ACP's own spec explicitly sanctions
/// this ("Clients MAY automatically allow or reject permission requests
/// according to user settings" -- agentclientprotocol.com/protocol/
/// tool-calls#requesting-permission): acpx has no live, out-of-band
/// channel back to whichever client opened the session by the time a
/// backend asks mid-turn (see `04-phased-plan.md`'s open-risks and
/// `COVERAGE.md`'s ACP-compatibility-hardening notes for why), so an
/// explicit, profile-scoped auto-decision policy is the honest
/// alternative to either silently deadlocking the backend (the pre-fix
/// behavior: the request was misclassified as a notification and simply
/// never answered) or guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    /// Auto-select an option whose `kind` starts with `allow_`
    /// (`allow_once`/`allow_always`); if the backend offered no such
    /// option, fall back to its first offered option (matching the
    /// reference Go SDK's own fallback) rather than replying `cancelled`
    /// -- this policy is an explicit opt-in to acpx deciding "yes" on the
    /// client's behalf, so failing to find an `allow_*`-labeled option
    /// specifically shouldn't itself block the operation.
    AutoAllow,
    /// Default: auto-select an option whose `kind` starts with `reject_`
    /// (`reject_once`/`reject_always`); if the backend offered no such
    /// option, reply with ACP's own `cancelled` outcome rather than
    /// guessing an option that might actually be an `allow_*` one.
    #[default]
    AutoReject,
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
            source: ProfileSource::Provisioned,
            provider: Some("openai-default".to_string()),
            key_ref: None,
            launch_overrides: HashMap::new(),
            mcp_servers: vec![],
            permission_policy: Default::default(),
            allow_fs_access: false,
            allow_terminal_access: false,
            auth_method_id: None,
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
