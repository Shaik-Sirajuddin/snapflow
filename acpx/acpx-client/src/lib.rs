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
pub mod raw;
