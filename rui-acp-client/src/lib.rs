//! `rui-acp-client`: the chat panel's ACP session layer.
//!
//! Per `memory/rui/gen/plans/chat-panel-acp-rust-sdk.md` Decision 4, this
//! crate is the *only* place that depends on `agent-client-protocol`
//! (the official ACP Rust SDK) directly. `panel-rust` (the Slint UI crate)
//! depends on this crate's public API only -- [`SessionClient`],
//! [`ChatMessage`]/[`MessageKind`], [`AgentEvent`] -- and never sees wire
//! types or jsonl file formats.
//!
//! Scope boundary (unchanged from the plan): per-thread static agent
//! binding, not `acpx`-style dynamic routing. Each thread is bound to one
//! agent connection for its lifetime.

mod jsonl;
mod session_client;

pub use jsonl::{CacheError, CachedThread, JsonlStore, ThreadTrailer};
pub use session_client::{
    spawn_thread, AgentEvent, AgentRequestEvent, ChatMessage, ConfigOptionInfo,
    ConfigOptionValue, MessageKind, SessionClient, SessionClientError, SessionModeInfo,
    SessionModesEvent, TerminalOutputEvent, ThreadHandle, ThreadId,
};

// Re-exported so callers can construct transports (`AcpAgent`, `Channel`,
// `Stdio`) without adding a second direct dependency on
// `agent-client-protocol` just for that.
pub use agent_client_protocol::{AcpAgent, Channel, ConnectTo};
