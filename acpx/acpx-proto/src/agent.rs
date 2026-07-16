//! `agent/*` gateway-native payload types (`agents/list`, `agents/install`,
//! `agents/status` -- see `02-architecture.md`'s method-classification
//! table). These have no raw-ACP equivalent; they only exist between an
//! acpx-aware client and `acpx-server`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Registry entry known, nothing fetched/verified yet.
    NotInstalled,
    /// Runtime present, adapter installed/resolvable (e.g. `node`+`npm` on
    /// `PATH` for an npx-distributed entry, or a fetched binary present).
    Installed,
    /// Installed, but no native session/credentials detected yet (distinct
    /// from `Installed` per `05-open-risks.md`'s "native session by
    /// default" note -- a third status, not a boolean).
    InstalledNoSession,
    /// The distribution method's runtime dependency is missing (e.g. no
    /// `node`/`npm` for an npx-only entry).
    RuntimeMissing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    Registry,
    Custom,
}

impl Default for AgentSource {
    fn default() -> Self {
        Self::Registry
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentListEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub status: AgentStatus,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub source: AgentSource,
}

fn default_enabled() -> bool {
    true
}
