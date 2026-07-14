//! Shared ACP wire types for acpx.
//!
//! `acpx-proto` re-exports the official `agent-client-protocol` crate's wire
//! types as the single source of truth for "raw ACP" (see
//! `memory/acpx/gen/plans/acp-gateway-daemon/03-crate-and-folder-layout.md`),
//! and adds only the acpx-specific sibling extension field (`_acpx`) used by
//! `session/new`'s profile-selection side channel -- never redefining the
//! raw ACP shape itself.

pub mod agent;
pub mod gateway;
pub mod jsonrpc;
pub mod methods;
pub mod openapi;
pub mod openrpc;
pub mod schema;
pub mod session;

/// Re-export of the official ACP SDK crate, so downstream crates can depend
/// on `acpx_proto::acp` instead of taking a second direct dependency that
/// could drift to a different version.
pub use agent_client_protocol as acp;
