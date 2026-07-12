//! Remote ACP adapter registry client.
//!
//! Phase 0 stub -- real implementation (fetch + parse
//! `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`,
//! `registry.fallback.json` bundled fallback, install-step resolution) lands
//! in Phase 4. See
//! `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md` step 18-19.

pub mod index;
pub mod install;

pub use index::{Agent, Distribution, Registry};
