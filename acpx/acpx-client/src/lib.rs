//! Rust client SDK for consumers of the acpx gateway.
//!
//! `raw` re-exports standard ACP client primitives unmodified; `ext`
//! layers acpx-specific extensions (profiles, registry, aggregated
//! session/list) on top. Phase 5 fills these in --
//! see `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md`.

pub mod ext;
pub mod raw;
