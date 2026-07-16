//! Session metadata persistence -- mirrors the `sessions` table and the
//! session concept from [`crate::session_registry::SessionRegistry`], but
//! this is the durable, on-disk record rather than the hot-path in-memory
//! index; the two are populated independently (see [`crate::persistence`]
//! module docs on the async write path).

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde_json::Value;
use std::fmt;

/// Durable recovery lifecycle state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    Active,
    Restoring,
    Restored,
    RecoveryFailed,
    Closed,
}

impl RecoveryStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Restoring => "restoring",
            Self::Restored => "restored",
            Self::RecoveryFailed => "recovery_failed",
            Self::Closed => "closed",
        }
    }
}

impl fmt::Display for RecoveryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToSql for RecoveryStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RecoveryStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "active" => Ok(Self::Active),
            "restoring" => Ok(Self::Restoring),
            "restored" => Ok(Self::Restored),
            "recovery_failed" => Ok(Self::RecoveryFailed),
            "closed" => Ok(Self::Closed),
            value => Err(FromSqlError::Other(
                format!("unknown recovery status {value:?}").into(),
            )),
        }
    }
}

/// Backend mechanism to use when restoring a durable session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryMethod {
    Load,
    Resume,
    None,
}

impl RecoveryMethod {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Resume => "resume",
            Self::None => "none",
        }
    }
}

impl fmt::Display for RecoveryMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToSql for RecoveryMethod {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RecoveryMethod {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "load" => Ok(Self::Load),
            "resume" => Ok(Self::Resume),
            "none" => Ok(Self::None),
            value => Err(FromSqlError::Other(
                format!("unknown recovery method {value:?}").into(),
            )),
        }
    }
}

/// Optional metadata supplied when creating a session that can be restored.
///
/// A missing `recovery_params` is stored as SQL `NULL`, avoiding an unsafe
/// assumption about a backend's parameter shape.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryMetadata {
    pub cwd: Option<String>,
    pub recovery_params: Option<Value>,
    pub status: RecoveryStatus,
    pub recovery_method: RecoveryMethod,
    pub last_recovery_error: Option<String>,
    /// Durable wall-clock lifecycle values. They are optional so a database
    /// created before lifecycle continuity shipped can be migrated without
    /// making old rows immediately eligible for TTL reaping on restart.
    pub created_at_unix_nanos: Option<i64>,
    pub last_activity_at_unix_nanos: Option<i64>,
    pub pinned: bool,
}

impl Default for RecoveryMetadata {
    fn default() -> Self {
        Self {
            cwd: None,
            recovery_params: None,
            status: RecoveryStatus::Active,
            recovery_method: RecoveryMethod::None,
            last_recovery_error: None,
            created_at_unix_nanos: None,
            last_activity_at_unix_nanos: None,
            pinned: false,
        }
    }
}

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
    pub cwd: Option<String>,
    pub recovery_params: Option<Value>,
    pub status: RecoveryStatus,
    pub recovery_method: RecoveryMethod,
    pub last_recovery_error: Option<String>,
    /// Explicit lifecycle retention override. `false` is the safe migration
    /// default: old rows retain their existing TTL behavior.
    pub pinned: bool,
    /// Wall-clock creation time used to reconstruct a conservative monotonic
    /// lifetime after a daemon restart. `None` denotes a pre-lifecycle row.
    pub created_at_unix_nanos: Option<i64>,
    /// Wall-clock last activity used to reconstruct the idle deadline after
    /// a daemon restart. `None` denotes a pre-lifecycle row.
    pub last_activity_at_unix_nanos: Option<i64>,
}
