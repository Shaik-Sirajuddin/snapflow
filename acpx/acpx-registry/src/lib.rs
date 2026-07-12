//! Remote ACP adapter registry client.
//!
//! Fetches and parses the official
//! `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`
//! (falling back to a bundled `registry.fallback.json` snapshot when
//! unreachable, see [`index::fetch_registry_or_fallback`]) and resolves the
//! `agents/install` step for a given agent's preferred distribution method
//! (see [`install::install`]). See
//! `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md` step 18-19.

pub mod index;
pub mod install;

pub use index::{
    fallback_registry, fetch_registry, fetch_registry_or_fallback, Agent, BinaryDist, Distribution,
    NpxDist, Registry, RegistryError, REGISTRY_URL,
};
pub use install::{host_platform_key, install, install_into, InstallError, InstallOutcome};
