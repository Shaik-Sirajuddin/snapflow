//! Registry index model, matching the official registry's schema:
//! `{ version, agents: [...], extensions: [...] }`. See
//! `memory/acpx/gen/plans/acp-gateway-daemon/01-research.md` for the schema
//! notes this was transcribed from.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    /// Semver-like string on the wire (e.g. `"1.0.0"`), not an integer --
    /// confirmed against the real registry.fallback.json snapshot.
    pub version: String,
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub extensions: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub distribution: Distribution,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<NpxDist>,
    #[serde(default)]
    pub uvx: Option<NpxDist>,
    /// Keyed by `<os>-<arch>`, e.g. `linux-x86_64`, `darwin-aarch64`.
    #[serde(default)]
    pub binary: Option<HashMap<String, BinaryDist>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpxDist {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryDist {
    pub archive: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Distribution {
    /// Preference order per `04-phased-plan.md` step 19: binary first (no
    /// extra runtime dependency), then npx/uvx.
    pub fn preferred_method(&self) -> Option<&'static str> {
        if self.binary.is_some() {
            Some("binary")
        } else if self.npx.is_some() {
            Some("npx")
        } else if self.uvx.is_some() {
            Some("uvx")
        } else {
            None
        }
    }
}

/// The official ACP registry's live endpoint. See `01-research.md`'s
/// registry schema notes.
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// Bundled schema-faithful mirror of the real Claude/Codex/Gemini entries,
/// used only when the live registry is unreachable. Kept in-crate (not a
/// path into the `memory` git submodule) so `include_str!` has a stable
/// target regardless of checkout layout.
const FALLBACK_JSON: &str = include_str!("../registry.fallback.json");

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("failed to reach registry at {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("registry endpoint {url} returned HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("failed to parse registry JSON from {url}: {source}")]
    Parse {
        url: String,
        #[source]
        source: serde_json::Error,
    },
}

/// GET the official registry and parse it as [`Registry`]. Requires network
/// access -- callers that need to stay hermetic (tests, offline builds)
/// should use [`fetch_registry_or_fallback`] or [`fallback_registry`]
/// instead.
pub async fn fetch_registry(client: &reqwest::Client) -> Result<Registry, RegistryError> {
    let response =
        client
            .get(REGISTRY_URL)
            .send()
            .await
            .map_err(|source| RegistryError::Request {
                url: REGISTRY_URL.to_string(),
                source,
            })?;

    let status = response.status();
    if !status.is_success() {
        return Err(RegistryError::Status {
            url: REGISTRY_URL.to_string(),
            status: status.as_u16(),
        });
    }

    let body = response
        .text()
        .await
        .map_err(|source| RegistryError::Request {
            url: REGISTRY_URL.to_string(),
            source,
        })?;

    serde_json::from_str(&body).map_err(|source| RegistryError::Parse {
        url: REGISTRY_URL.to_string(),
        source,
    })
}

/// Parse the bundled `registry.fallback.json` snapshot. Infallible in
/// practice -- the bundled file is schema-checked by
/// `tests/index_fixtures.rs` and this crate's own build -- but panics with a
/// clear message rather than silently returning an empty registry if it
/// ever drifts out of sync with [`Registry`]'s shape.
pub fn fallback_registry() -> Registry {
    serde_json::from_str(FALLBACK_JSON)
        .expect("bundled registry.fallback.json must parse as Registry -- keep it schema-synced")
}

/// Try the live registry first, falling back to the bundled snapshot on any
/// error (network failure, non-2xx status, or a parse error). Never fails --
/// this is the entry point most callers (e.g. `agents/list`) should use.
pub async fn fetch_registry_or_fallback(client: &reqwest::Client) -> Registry {
    match fetch_registry(client).await {
        Ok(registry) => registry,
        Err(err) => {
            tracing::warn!(
                error = %err,
                url = REGISTRY_URL,
                "failed to fetch live ACP registry, falling back to bundled registry.fallback.json"
            );
            fallback_registry()
        }
    }
}
