//! The acpx gateway's brain: session registry, method-classification
//! router, profile/provider config, central MCP server registry, and
//! transcript persistence. See
//! `memory/acpx/gen/plans/acp-gateway-daemon/02-architecture.md`.

pub mod admin;
pub mod agent_relay;
pub mod agent_state;
pub mod bridge_sessions;
pub mod custom_agents;
pub mod detect;
pub mod interaction;
pub mod keystore;
pub mod launch;
pub mod lifecycle;
pub mod mcp_servers;
pub mod notify;
pub mod persistence;
pub mod profile;
pub mod provider;
pub mod router;
pub mod session_registry;

pub use admin::{AdminError, AdminOps};
pub use agent_relay::AgentRequestHub;
pub use agent_state::AgentEnablement;
pub use bridge_sessions::{
    BindingClaim, BridgeSession, BridgeSessionError, BridgeSessionId, BridgeSessionState,
    BridgeSessionStore,
};
pub use custom_agents::{CustomAgent, CustomAgentStore, CustomAgentStoreError};
pub use interaction::{
    InteractionBinding, InteractionError, InteractionHub, DEFAULT_INTERACTION_TIMEOUT,
    INTERACTION_QUEUE_CAPACITY,
};
pub use lifecycle::LifecycleConfig;
pub use notify::{NotificationHub, ResumeCursor, StreamResumeState, SubscribeError};
pub use persistence::{
    Direction, PersistenceError, PersistenceStore, RecoveryStatusCounts, SessionRecord,
    TranscriptRecord,
};
pub use router::{
    recover_open_sessions_shared, LifecycleReapReport, MethodClass, Router, StartupRecoveryPolicy,
    StartupRecoveryReport,
};
pub use session_registry::{BackendSessionId, SessionRegistry, TenantId};
