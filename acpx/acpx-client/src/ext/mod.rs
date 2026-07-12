//! Additive acpx extensions on top of raw ACP -- profile
//! selection/listing, registry queries, and the gateway-native
//! `agents/*`/aggregated `session/list` surfaces that have no plain-ACP
//! equivalent to fall back to. Every function here is a thin typed
//! wrapper around one `raw::GatewayClient::call` -- none of them touch
//! `raw`'s own behavior, so a caller that only ever uses `raw` directly
//! still gets an unmodified ACP client. Phase 5 step 21.

pub mod profiles;
pub mod registry;
pub mod sessions;
