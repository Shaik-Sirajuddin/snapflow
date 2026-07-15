//! Startup config-file provisioning: providers/keys/profiles/MCP servers.
//!
//! Closes the gap `COVERAGE.md`'s Phase 3 section flags as "Not yet built
//! in Phase 3": before this module, `Router::register_provider`/
//! `Router::store_key` were programmatic-only seams exercised solely by
//! tests -- a real deployment had no way to provision a provider/profile
//! without writing Rust. Setting `ACPX_CONFIG_FILE` to a JSON file now
//! lets an operator declare providers, central MCP servers, and profiles
//! that get created at startup, before either transport starts accepting
//! requests.
//!
//! Deliberately reuses `Router::dispatch` for `mcp_servers/create` and
//! `profiles/create` (rather than adding new non-JSON-RPC `Router`
//! methods) so provisioning-file profiles/servers go through exactly the
//! same validation (`ProfileStoreError::AlreadyExists`, unknown-provider
//! checks happen later at resolve time, etc.) a client's own
//! `profiles/create` call would -- one code path, not two.
//!
//! Secrets: a profile entry may set `secret` (the raw value, inline in
//! the file -- discouraged, since the file itself is then a secret) or
//! `secret_env` (the *name* of an environment variable to read the real
//! value from at load time -- preferred, since the file can then be
//! committed/templated while the actual value comes from whatever secret
//! manager populates the process's environment). Setting both is a
//! startup error, not a silent "one wins" choice. This does not add
//! encryption at rest for `Keystore` itself (still explicitly open, see
//! `05-open-risks.md`) -- it only keeps the config *file* free of secrets
//! when `secret_env` is used, which is the more common real deployment
//! shape (env injected by systemd/k8s/etc.) than a plaintext secret
//! sitting in a checked-in file.

use acpx_core::router::Router;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningFile {
    #[serde(default)]
    pub providers: Vec<serde_json::Value>,
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
    #[serde(default)]
    pub profiles: Vec<ProfileEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    pub name: String,
    pub agent_id: String,
    #[serde(default)]
    pub provider: Option<String>,
    /// Raw secret value, inline in the file. Mutually exclusive with
    /// `secret_env` -- see module doc comment.
    #[serde(default)]
    pub secret: Option<String>,
    /// Name of an env var to read the secret from at load time. Preferred
    /// over `secret` -- see module doc comment.
    #[serde(default)]
    pub secret_env: Option<String>,
    #[serde(default)]
    pub launch_overrides: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProvisioningError {
    #[error("failed to read provisioning file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse provisioning file {path} as JSON: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("profile {0}: \"secret\" and \"secret_env\" are mutually exclusive, set at most one")]
    BothSecretFields(String),
    #[error("profile {profile}: secret_env references unset env var {var}")]
    MissingSecretEnv { profile: String, var: String },
    #[error("provider entry {index}: {source}")]
    InvalidProvider {
        index: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("mcp_servers/create for {name}: {source}")]
    McpServer {
        name: String,
        #[source]
        source: acpx_core::router::RouterError,
    },
    #[error("profiles/create for {name}: {source}")]
    Profile {
        name: String,
        #[source]
        source: acpx_core::router::RouterError,
    },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProvisioningSummary {
    pub providers: usize,
    pub mcp_servers: usize,
    pub profiles: usize,
}

/// Read + parse a provisioning file from disk. Split from [`apply`] so
/// callers/tests can construct a [`ProvisioningFile`] in-memory (e.g. from
/// a `serde_json::json!` literal) without touching the filesystem.
pub fn load(path: &Path) -> Result<ProvisioningFile, ProvisioningError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ProvisioningError::Read {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| ProvisioningError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Apply a parsed provisioning file to `router`: register every provider,
/// create every central MCP server, then create every profile (in that
/// order, since a profile may reference either by name). Fails fast on
/// the first error rather than partially applying and continuing --
/// a broken deployment config should refuse to start with an unclear
/// partial state, not silently skip the bad entry.
pub async fn apply(
    router: &mut Router,
    file: ProvisioningFile,
) -> Result<ProvisioningSummary, ProvisioningError> {
    let mut summary = ProvisioningSummary::default();

    for (index, raw) in file.providers.into_iter().enumerate() {
        let provider: acpx_core::provider::ProviderConfig = serde_json::from_value(raw)
            .map_err(|source| ProvisioningError::InvalidProvider { index, source })?;
        router.register_provider(provider);
        summary.providers += 1;
    }

    for entry in file.mcp_servers.into_iter() {
        let name = entry
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("<unnamed>")
            .to_string();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "mcp_servers/create",
            "params": entry,
        });
        router
            .dispatch(request)
            .await
            .map_err(|source| ProvisioningError::McpServer {
                name: name.clone(),
                source,
            })?;
        summary.mcp_servers += 1;
    }

    for profile in file.profiles.into_iter() {
        if profile.secret.is_some() && profile.secret_env.is_some() {
            return Err(ProvisioningError::BothSecretFields(profile.name));
        }
        let secret = match profile.secret_env {
            Some(var) => {
                Some(
                    std::env::var(&var).map_err(|_| ProvisioningError::MissingSecretEnv {
                        profile: profile.name.clone(),
                        var,
                    })?,
                )
            }
            None => profile.secret,
        };
        let name = profile.name.clone();
        let mut params = serde_json::json!({
            "name": profile.name,
            "agent_id": profile.agent_id,
            "provider": profile.provider,
            "launch_overrides": profile.launch_overrides,
            "mcp_servers": profile.mcp_servers,
        });
        if let Some(secret) = secret {
            params["secret"] = serde_json::Value::String(secret);
        }
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "profiles/create",
            "params": params,
        });
        router
            .dispatch(request)
            .await
            .map_err(|source| ProvisioningError::Profile {
                name: name.clone(),
                source,
            })?;
        summary.profiles += 1;
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpx_conductor::SpawnSpec;

    /// Same stand-in trick as `acpx-core/tests/profile_resolution_test.rs`:
    /// a real `sh` subprocess that speaks just enough ACP-shaped JSON-RPC
    /// (matching the incoming request's `id`) to satisfy both the
    /// `initialize` handshake and `session/new`'s `result.sessionId`
    /// requirement.
    const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc","protocolVersion":1}}\n' "$id"
done
"#;

    fn router_with_stand_in_agent(agent_id: &str) -> Router {
        let mut router = Router::new(agent_id.to_string());
        router.register_agent(
            agent_id.to_string(),
            SpawnSpec::new(
                "sh".to_string(),
                vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
            ),
        );
        router
    }

    #[tokio::test]
    async fn applies_providers_mcp_servers_and_profiles_in_order() {
        let mut router = router_with_stand_in_agent("stand-in");
        let file: ProvisioningFile = serde_json::from_value(serde_json::json!({
            "providers": [
                {"name": "anthropic-default", "kind": "anthropic", "base_url": null}
            ],
            "mcp_servers": [
                {"name": "fs", "command": "npx", "args": ["-y", "server-filesystem"]}
            ],
            "profiles": [
                {
                    "name": "work",
                    "agent_id": "stand-in",
                    "provider": "anthropic-default",
                    "mcp_servers": ["fs"]
                }
            ]
        }))
        .unwrap();

        let summary = apply(&mut router, file).await.unwrap();
        assert_eq!(
            summary,
            ProvisioningSummary {
                providers: 1,
                mcp_servers: 1,
                profiles: 1,
            }
        );

        // The profile is actually usable end to end, not just recorded --
        // session/new with `_acpx.profile: "work"` should resolve through
        // to the registered stand-in backend.
        let session_new = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {"mcpServers": [], "_acpx": {"profile": "work"}},
        });
        let response = router.dispatch(session_new).await.unwrap();
        assert!(response.get("result").is_some(), "{response:?}");
    }

    #[tokio::test]
    async fn secret_env_reads_from_the_named_env_var_not_the_file() {
        // SAFETY: single-threaded within this test's own env var, tokio's
        // multi-thread test runner still runs each #[tokio::test] body on
        // one task at a time; no other test in this file touches this
        // specific var name.
        std::env::set_var("ACPX_TEST_PROVISIONING_SECRET", "sk-from-env");
        let mut router = router_with_stand_in_agent("stand-in");
        let file: ProvisioningFile = serde_json::from_value(serde_json::json!({
            "profiles": [
                {
                    "name": "work",
                    "agent_id": "stand-in",
                    "secret_env": "ACPX_TEST_PROVISIONING_SECRET"
                }
            ]
        }))
        .unwrap();

        apply(&mut router, file).await.unwrap();
        std::env::remove_var("ACPX_TEST_PROVISIONING_SECRET");

        // profiles/list never echoes the resolved secret itself (only an
        // opaque KeyRef), so assert indirectly: the profile has a
        // key_ref set at all (proving `secret_env` was actually resolved
        // and stored, not silently dropped).
        let list =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "profiles/list", "params": {}});
        let response = router.dispatch(list).await.unwrap();
        let profiles = response["result"]["profiles"].as_array().unwrap();
        // `profiles/list` also includes auto-seeded profiles
        // (`ensure_default_profiles_seeded`, see
        // `acpx-core/tests/default_profile_seeding_test.rs`) alongside
        // the provisioned "work" one, so find it by name rather than
        // asserting the list's exact length.
        let work = profiles
            .iter()
            .find(|p| p["name"] == "work")
            .expect("provisioned \"work\" profile listed");
        assert!(work.get("key_ref").is_some_and(|v| !v.is_null()));
    }

    #[tokio::test]
    async fn both_secret_fields_set_is_a_startup_error() {
        let mut router = router_with_stand_in_agent("stand-in");
        let file: ProvisioningFile = serde_json::from_value(serde_json::json!({
            "profiles": [
                {
                    "name": "work",
                    "agent_id": "stand-in",
                    "secret": "inline",
                    "secret_env": "SOME_VAR"
                }
            ]
        }))
        .unwrap();

        let err = apply(&mut router, file).await.unwrap_err();
        assert!(matches!(err, ProvisioningError::BothSecretFields(name) if name == "work"));
    }

    #[tokio::test]
    async fn missing_secret_env_var_is_a_clear_startup_error_not_a_silent_skip() {
        let mut router = router_with_stand_in_agent("stand-in");
        let file: ProvisioningFile = serde_json::from_value(serde_json::json!({
            "profiles": [
                {
                    "name": "work",
                    "agent_id": "stand-in",
                    "secret_env": "ACPX_TEST_DEFINITELY_UNSET_VAR"
                }
            ]
        }))
        .unwrap();

        let err = apply(&mut router, file).await.unwrap_err();
        assert!(matches!(
            err,
            ProvisioningError::MissingSecretEnv { profile, var }
                if profile == "work" && var == "ACPX_TEST_DEFINITELY_UNSET_VAR"
        ));
    }

    #[tokio::test]
    async fn unknown_provider_reference_surfaces_at_apply_time_is_deferred_to_resolve() {
        // profiles/create itself doesn't validate `provider` against the
        // ProviderStore (that check happens at session/new resolve time,
        // see router.rs's resolve_profile) -- assert that documented
        // behavior explicitly here so a future change to make create-time
        // eager is a deliberate decision, not an accidental regression
        // this test silently stops covering.
        let mut router = router_with_stand_in_agent("stand-in");
        let file: ProvisioningFile = serde_json::from_value(serde_json::json!({
            "profiles": [
                {
                    "name": "work",
                    "agent_id": "stand-in",
                    "provider": "never-registered"
                }
            ]
        }))
        .unwrap();

        let summary = apply(&mut router, file).await.unwrap();
        assert_eq!(summary.profiles, 1);
    }

    #[test]
    fn load_reads_and_parses_a_real_file() {
        let dir =
            std::env::temp_dir().join(format!("acpx-provisioning-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"providers": [], "mcp_servers": [], "profiles": []}"#,
        )
        .unwrap();

        let file = load(&path).unwrap();
        assert_eq!(file.providers.len(), 0);
        assert_eq!(file.profiles.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
