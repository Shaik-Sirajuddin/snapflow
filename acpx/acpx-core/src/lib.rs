//! The acpx gateway's brain: session registry, method-classification
//! router, profile/provider config, central MCP server registry, and
//! transcript persistence. See
//! `memory/acpx/gen/plans/acp-gateway-daemon/02-architecture.md`.

pub mod agent_relay;
pub mod detect;
pub mod keystore;
pub mod launch;
pub mod mcp_servers;
pub mod notify;
pub mod persistence;
pub mod profile;
pub mod provider;
pub mod router;
pub mod session_registry;

pub use agent_relay::AgentRequestHub;
pub use notify::NotificationHub;
pub use persistence::{
    Direction, PersistenceError, PersistenceStore, SessionRecord, TranscriptRecord,
};
pub use router::{MethodClass, Router};
pub use session_registry::{BackendSessionId, SessionRegistry, TenantId};
