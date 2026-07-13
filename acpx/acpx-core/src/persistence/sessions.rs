//! Session metadata persistence -- mirrors the `sessions` table and the
//! session concept from [`crate::session_registry::SessionRegistry`], but
//! this is the durable, on-disk record rather than the hot-path in-memory
//! index; the two are populated independently (see [`crate::persistence`]
//! module docs on the async write path).

/// One row of the `sessions` table. `created_at`/`closed_at` are opaque
/// caller-supplied timestamp strings (the router owns timestamp formatting,
/// e.g. RFC3339) -- persistence itself stays free of a time-formatting
/// dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub gateway_session_id: String,
    pub agent_id: String,
    pub backend_session_id: String,
    pub profile_name: Option<String>,
    pub created_at: String,
    pub closed_at: Option<String>,
    /// **Phase C (`acpx-tenant-isolation`).** The tenant that owns this
    /// row, mirroring `crate::session_registry::TenantId`'s `String`
    /// payload (kept as a plain `String` here, not the newtype itself, so
    /// this crate's persistence module stays free of a dependency on
    /// `session_registry` -- `router.rs` is what converts between the
    /// two). Rows written before this field existed are backfilled to
    /// `"default"` by `store.rs`'s migration, matching every other
    /// tenant-unaware caller's implicit tenant.
    pub tenant_id: String,
}
