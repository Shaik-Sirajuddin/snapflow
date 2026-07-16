//! Additive ACPX extensions. Most modules wrap raw JSON-RPC calls, while
//! `admin` is intentionally separate: the loopback-only `/admin/*` plane
//! is ordinary bearer-authenticated HTTP, not an ACP JSON-RPC surface.

pub mod admin;
pub mod profiles;
pub mod prompt;
pub mod registry;
pub mod sessions;
