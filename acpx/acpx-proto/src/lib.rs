//! Shared ACP wire types for acpx.
//!
//! `acpx-proto` re-exports the official `agent-client-protocol` crate's wire
//! types as the single source of truth for "raw ACP" (see
//! `memory/acpx/gen/plans/acp-gateway-daemon/03-crate-and-folder-layout.md`),
//! and adds only the acpx-specific sibling extension field (`_acpx`) used by
//! `session/new`'s profile-selection side channel -- never redefining the
//! raw ACP shape itself.

pub mod admin;
pub mod agent;
pub mod gateway;
pub mod jsonrpc;
pub mod methods;
pub mod openapi;
pub mod openrpc;
pub mod schema;
pub mod session;
pub mod validate;

/// Single source of truth for the default acpx-server HTTP/WS bind address.
///
/// Both ends of the panel-rust <-> acpx-server link consume this one value
/// instead of repeating per-crate literals: acpx-server uses it as its
/// `ACPX_HTTP_BIND` default (`acpx-server/src/config.rs`), and panel-rust
/// (via acpx-client's re-export) uses it to build its default gateway URL
/// when no `RUI_ACPX_DEFAULT_URL` / `RUI_ACPX_<PROVIDER>_URL` override is
/// set (`panel-rust/src/agent_bridge.rs`). Change the default port here, in
/// one place, not in scattered 8790/8791 literals. Loopback only.
pub const DEFAULT_ACPX_HTTP_ADDR: &str = "127.0.0.1:8790";

/// The default gateway URL a client dials, derived from
/// [`DEFAULT_ACPX_HTTP_ADDR`] so the port lives in exactly one place.
pub fn default_acpx_http_url() -> String {
    format!("http://{DEFAULT_ACPX_HTTP_ADDR}")
}

/// The default HTTP/WS bind port, parsed from [`DEFAULT_ACPX_HTTP_ADDR`].
pub fn default_acpx_http_port() -> u16 {
    DEFAULT_ACPX_HTTP_ADDR
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("DEFAULT_ACPX_HTTP_ADDR must end in a valid port")
}

#[cfg(test)]
mod default_acpx_addr_tests {
    use super::*;

    #[test]
    fn default_addr_parses_and_helpers_agree() {
        // The one authority must be a valid socket addr, and both derived
        // helpers must stay consistent with it -- so a future edit to the
        // single literal can't silently desync the URL/port views of it.
        let addr: std::net::SocketAddr = DEFAULT_ACPX_HTTP_ADDR
            .parse()
            .expect("DEFAULT_ACPX_HTTP_ADDR must be a valid socket address");
        assert_eq!(default_acpx_http_port(), addr.port());
        assert_eq!(
            default_acpx_http_url(),
            format!("http://{DEFAULT_ACPX_HTTP_ADDR}")
        );
    }
}

/// Re-export of the official ACP SDK crate, so downstream crates can depend
/// on `acpx_proto::acp` instead of taking a second direct dependency that
/// could drift to a different version.
pub use agent_client_protocol as acp;
