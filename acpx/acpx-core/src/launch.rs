//! Provider -> spawn-env mapping for `session/new`'s managed (profile)
//! mode. Phase 3 steps 15-16.
//!
//! Maps a resolved [`crate::provider::ProviderConfig`] + secret (already
//! pulled out of [`crate::keystore::Keystore`] by the caller) into the env
//! vars a spawned backend process needs, per the config surfaces
//! researched for the two `npx`-distributed adapters this gateway spawns
//! today (see `01-research.md`/`05-open-risks.md` for what was previously
//! unverified here):
//! - `@agentclientprotocol/codex-acp`: `CODEX_API_KEY` (preferred over its
//!   own `OPENAI_API_KEY` fallback, so acpx always sets the
//!   gateway-specific one) for the key, `CODEX_CONFIG` (a JSON string
//!   merged into the Codex session config) carrying `{"openai_base_url":
//!   "<url>"}` for a custom/litellm endpoint. `ProviderKind::OpenAi` and
//!   `ProviderKind::LiteLlm` both route through this surface -- litellm is
//!   just an OpenAI-compatible endpoint from codex-acp's point of view.
//! - `@agentclientprotocol/claude-agent-acp`: `ANTHROPIC_API_KEY` for the
//!   key, `ANTHROPIC_BASE_URL` (plain string, not JSON-wrapped) for a
//!   custom endpoint -- both standard Anthropic-SDK env vars the adapter
//!   inherits unmodified.
//!
//! The resulting env map is handed to `acpx_conductor::SpawnSpec.env`
//! ("Env vars to set/override on top of the inherited ambient
//! environment"), so it composes with an already-working native
//! `codex login`/`claude login` -- a spawned managed-mode process still
//! inherits the rest of the ambient environment unless a var is
//! explicitly overridden here.

use crate::profile::Profile;
use crate::provider::{ProviderConfig, ProviderKind};
use std::collections::HashMap;

/// Derive the env vars implied by `provider` + an already-resolved secret.
/// Pure function of its inputs -- callers own key resolution (via
/// `Keystore::resolve`) and any resulting `KeystoreError` handling.
pub fn provider_env(
    provider: &ProviderConfig,
    resolved_key: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    match provider.kind {
        ProviderKind::OpenAi | ProviderKind::LiteLlm => {
            if let Some(key) = resolved_key {
                env.insert("CODEX_API_KEY".to_string(), key.to_string());
            }
            if let Some(base_url) = &provider.base_url {
                let config = serde_json::json!({ "openai_base_url": base_url });
                env.insert("CODEX_CONFIG".to_string(), config.to_string());
            }
        }
        ProviderKind::Anthropic => {
            if let Some(key) = resolved_key {
                env.insert("ANTHROPIC_API_KEY".to_string(), key.to_string());
            }
            if let Some(base_url) = &provider.base_url {
                env.insert("ANTHROPIC_BASE_URL".to_string(), base_url.clone());
            }
        }
    }
    env
}

/// Full env map for launching `profile`'s agent: `provider_env` (if a
/// provider was resolved) with `profile.launch_overrides` layered on top
/// -- an explicit override in the profile always wins over a
/// provider-derived default (same key name), per `profile.rs`'s doc
/// comment on that field.
pub fn build_launch_env(
    profile: &Profile,
    provider: Option<&ProviderConfig>,
    resolved_key: Option<&str>,
) -> HashMap<String, String> {
    let mut env = match provider {
        Some(provider) => provider_env(provider, resolved_key),
        None => HashMap::new(),
    };
    for (key, value) in &profile.launch_overrides {
        env.insert(key.clone(), value.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai(base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "openai-default".to_string(),
            kind: ProviderKind::OpenAi,
            base_url: base_url.map(str::to_string),
        }
    }

    fn litellm(base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "my-litellm".to_string(),
            kind: ProviderKind::LiteLlm,
            base_url: base_url.map(str::to_string),
        }
    }

    fn anthropic(base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "anthropic-default".to_string(),
            kind: ProviderKind::Anthropic,
            base_url: base_url.map(str::to_string),
        }
    }

    fn profile_with_overrides(overrides: HashMap<String, String>) -> Profile {
        Profile {
            name: "test-profile".to_string(),
            agent_id: "codex-acp".to_string(),
            source: crate::profile::ProfileSource::Provisioned,
            provider: None,
            key_ref: None,
            launch_overrides: overrides,
            mcp_servers: vec![],
            permission_policy: Default::default(),
            allow_fs_access: false,
            allow_terminal_access: false,
            auth_method_id: None,
        }
    }

    #[test]
    fn openai_with_key_and_base_url_sets_both_vars() {
        let env = provider_env(
            &openai(Some("https://litellm.example.com/v1")),
            Some("sk-abc"),
        );
        assert_eq!(env.get("CODEX_API_KEY").unwrap(), "sk-abc");
        let config: serde_json::Value =
            serde_json::from_str(env.get("CODEX_CONFIG").unwrap()).unwrap();
        assert_eq!(config["openai_base_url"], "https://litellm.example.com/v1");
    }

    #[test]
    fn openai_with_no_base_url_only_sets_api_key() {
        let env = provider_env(&openai(None), Some("sk-abc"));
        assert_eq!(env.get("CODEX_API_KEY").unwrap(), "sk-abc");
        assert!(!env.contains_key("CODEX_CONFIG"));
    }

    #[test]
    fn litellm_uses_the_same_codex_acp_surface_as_openai() {
        let env = provider_env(
            &litellm(Some("https://litellm.example.com/v1")),
            Some("sk-abc"),
        );
        assert_eq!(env.get("CODEX_API_KEY").unwrap(), "sk-abc");
        assert!(env.contains_key("CODEX_CONFIG"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn anthropic_with_key_and_base_url_sets_plain_string_base_url() {
        let env = provider_env(
            &anthropic(Some("https://api.example.com/anthropic")),
            Some("sk-ant-abc"),
        );
        assert_eq!(env.get("ANTHROPIC_API_KEY").unwrap(), "sk-ant-abc");
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").unwrap(),
            "https://api.example.com/anthropic"
        );
        assert!(!env.contains_key("CODEX_API_KEY"));
    }

    #[test]
    fn anthropic_with_no_key_omits_api_key_entry() {
        let env = provider_env(&anthropic(None), None);
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn profile_launch_overrides_win_over_derived_provider_env() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "CODEX_API_KEY".to_string(),
            "sk-profile-override".to_string(),
        );
        let profile = profile_with_overrides(overrides);
        let env = build_launch_env(&profile, Some(&openai(None)), Some("sk-resolved"));
        assert_eq!(env.get("CODEX_API_KEY").unwrap(), "sk-profile-override");
    }

    #[test]
    fn build_launch_env_with_no_provider_returns_only_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert("FOO".to_string(), "bar".to_string());
        let profile = profile_with_overrides(overrides.clone());
        let env = build_launch_env(&profile, None, None);
        assert_eq!(env, overrides);
    }
}
