//! Rust client SDK for consumers of the acpx gateway. Phase 5.
//!
//! `raw` is the JSON-RPC-over-HTTP transport to one gateway instance
//! (`GatewayClient`) -- unmodified ACP wire shape, no acpx-specific
//! interpretation; `ext` layers acpx-specific typed extensions (profiles,
//! registry queries + client-initiated install, aggregated session/list)
//! strictly on top, never editing `raw`'s own behavior. See
//! `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md` (Phase 5,
//! steps 20-22) and `raw.rs`'s doc comment for the one documented
//! deviation from the plan's literal wording (HTTP transport vs. the
//! official SDK's subprocess-stdio-oriented `Client` trait).

pub mod ext;
pub mod gateway;
pub mod raw;
pub mod ws;

pub use gateway::{AgentRequest, Gateway, TransportMode};

/// Re-export of the single-source-of-truth default acpx bind address (and
/// its URL/port helpers) from `acpx-proto`, so panel-rust -- which only
/// depends on this SDK crate -- can reach the same value acpx-server uses
/// for its own `ACPX_HTTP_BIND` default.
pub use acpx_proto::{default_acpx_http_port, default_acpx_http_url, DEFAULT_ACPX_HTTP_ADDR};
