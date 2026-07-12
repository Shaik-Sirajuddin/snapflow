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
