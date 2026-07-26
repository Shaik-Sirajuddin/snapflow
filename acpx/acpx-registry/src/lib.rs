//! Remote ACP adapter registry client.
//!
//! Fetches and parses the official
//! `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`
//! (falling back to a bundled `registry.fallback.json` snapshot when
//! unreachable, see [`index::fetch_registry_or_fallback`]) and resolves the
//! `agents/install` step for a given agent's preferred distribution method
//! (see [`install::install`]). See
//! `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md` step 18-19.

pub mod capabilities;
pub mod capability_cache;
pub mod index;
pub mod install;

pub use capabilities::{AdapterCapabilities, ConfigOption, SelectOption};
pub use capability_cache::{CapabilityCache, CapabilityCacheKey};
pub use index::{
    fallback_registry, fetch_registry, fetch_registry_or_fallback, Agent, BinaryDist, Distribution,
    NpxDist, Registry, RegistryError, REGISTRY_URL,
};
pub use install::{
    default_adapters_dir, host_platform_key, install, install_into, is_package_ready,
    ready_marker_path, write_ready_marker, InstallError, InstallOutcome, READY_MARKER_NAME,
};
