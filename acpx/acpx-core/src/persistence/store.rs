//! [`PersistenceStore`]: the async-friendly handle callers use to read and
//! write `sessions`/`transcripts` rows.
//!
//! Concurrency shape: `rusqlite::Connection` is a synchronous, `Send`-but-
//! not-`Sync` handle, so the store wraps one in `Arc<Mutex<Connection>>`.
//! Every public method:
//! 1. Converts its (borrowed/generic) arguments into owned data up front.
//! 2. Clones the `Arc` (cheap) and moves the owned data + clone into a
//!    `tokio::task::spawn_blocking` closure.
//! 3. Locks the `Mutex` only inside that blocking closure, for the
//!    duration of one query.
//!
//! That means `PersistenceStore` is `Clone` (each clone shares the same
//! underlying connection via the `Arc`) and every async method is safe to
//! call from a `tokio::spawn`-ed fire-and-forget task on the routing hot
//! path: the caller's `.await` point never blocks the async runtime on
//! sqlite file I/O, and concurrent callers serialize on the `Mutex` inside
//! the blocking thread pool rather than on an async runtime worker thread.
//! A single connection is sufficient here (not a connection pool) because
//! sqlite only ever allows one writer at a time regardless -- a pool would
//! just move the contention from our `Mutex` to sqlite's own file lock.

use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::error::PersistenceError;
use super::sessions::SessionRecord;
use super::transcripts::{Direction, TranscriptRecord};

#[derive(Clone)]
pub struct PersistenceStore {
    conn: Arc<Mutex<Connection>>,
}

impl PersistenceStore {
    /// Open (creating if absent) a sqlite database file at `path` and
    /// ensure the schema exists.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory database, for tests -- same schema-application path as
    /// [`Self::open`], nothing sqlite-specific is skipped.
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, PersistenceError> {
        conn.execute_batch(super::SCHEMA_SQL)?;
        migrate_tenant_id_column(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record a newly-created session. Fails if `gateway_session_id` is
    /// already present (the `sessions` table primary key) -- callers should
    /// only invoke this once per session, right after
    /// [`crate::SessionRegistry::register`].
    pub async fn record_session(
        &self,
        gateway_session_id: impl Into<String>,
        agent_id: impl Into<String>,
        backend_session_id: impl Into<String>,
        profile_name: Option<String>,
        created_at: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let agent_id = agent_id.into();
        let backend_session_id = backend_session_id.into();
        let created_at = created_at.into();
        let tenant_id = tenant_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO sessions \
                 (gateway_session_id, agent_id, backend_session_id, profile_name, created_at, tenant_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    gateway_session_id,
                    agent_id,
                    backend_session_id,
                    profile_name,
                    created_at,
                    tenant_id
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Mark a session closed. Errors with [`PersistenceError::SessionNotFound`]
    /// if no row with that `gateway_session_id` exists.
    pub async fn close_session(
        &self,
        gateway_session_id: impl Into<String>,
        closed_at: impl Into<String>,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let closed_at = closed_at.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let rows = conn.execute(
                "UPDATE sessions SET closed_at = ?1 WHERE gateway_session_id = ?2",
                params![closed_at, gateway_session_id],
            )?;
            if rows == 0 {
                return Err(PersistenceError::SessionNotFound(gateway_session_id));
            }
            Ok(())
        })
        .await
    }

    /// Fetch one session's metadata row, if it exists.
    pub async fn get_session(
        &self,
        gateway_session_id: impl Into<String>,
    ) -> Result<Option<SessionRecord>, PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn.prepare(
                "SELECT gateway_session_id, agent_id, backend_session_id, profile_name, \
                        created_at, closed_at, tenant_id \
                 FROM sessions WHERE gateway_session_id = ?1",
            )?;
            let mut rows = stmt.query_map(params![gateway_session_id], row_to_session_record)?;
            match rows.next() {
                Some(row) => Ok(Some(row?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// List every session's metadata row, oldest first. Kept minimal
    /// (no pagination/filtering) -- Phase 2's `session/list` is served from
    /// the in-memory [`crate::SessionRegistry`], not this; this is for
    /// completeness/tests/future persistence-backed reporting.
    pub async fn list_sessions(&self) -> Result<Vec<SessionRecord>, PersistenceError> {
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn.prepare(
                "SELECT gateway_session_id, agent_id, backend_session_id, profile_name, \
                        created_at, closed_at, tenant_id \
                 FROM sessions ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], row_to_session_record)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await
    }

    /// Append one transcript record for `gateway_session_id`. Returns the
    /// assigned row id. Cheap to call fire-and-forget via `tokio::spawn` --
    /// see module docs.
    pub async fn append_transcript(
        &self,
        gateway_session_id: impl Into<String>,
        direction: Direction,
        payload: serde_json::Value,
        recorded_at: impl Into<String>,
    ) -> Result<i64, PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let recorded_at = recorded_at.into();
        let payload_text = serde_json::to_string(&payload)?;
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO transcripts \
                 (gateway_session_id, direction, payload, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    gateway_session_id,
                    direction.as_str(),
                    payload_text,
                    recorded_at
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// Fetch all transcript records for a session, oldest first -- future
    /// replay/debugging read path.
    pub async fn list_transcripts(
        &self,
        gateway_session_id: impl Into<String>,
    ) -> Result<Vec<TranscriptRecord>, PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn.prepare(
                "SELECT id, gateway_session_id, direction, payload, recorded_at \
                 FROM transcripts WHERE gateway_session_id = ?1 ORDER BY id ASC",
            )?;
            let raw = stmt
                .query_map(params![gateway_session_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, rusqlite::Error>>()?;
            let mut out = Vec::with_capacity(raw.len());
            for (id, gateway_session_id, direction_text, payload_text, recorded_at) in raw {
                out.push(TranscriptRecord {
                    id: Some(id),
                    gateway_session_id,
                    direction: Direction::try_from(direction_text.as_str())?,
                    payload: serde_json::from_str(&payload_text)?,
                    recorded_at,
                });
            }
            Ok(out)
        })
        .await
    }
}

fn row_to_session_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        gateway_session_id: row.get(0)?,
        agent_id: row.get(1)?,
        backend_session_id: row.get(2)?,
        profile_name: row.get(3)?,
        created_at: row.get(4)?,
        closed_at: row.get(5)?,
        tenant_id: row.get(6)?,
    })
}

/// **Phase C (`acpx-tenant-isolation`).** Idempotent upgrade path for
/// databases created before `tenant_id` existed: `CREATE TABLE IF NOT
/// EXISTS` (in `SCHEMA_SQL`, applied unconditionally above) never touches
/// an already-existing `sessions` table, so a pre-existing on-disk
/// database would otherwise be missing the column entirely and every
/// query above would fail with "no such column: tenant_id". Sqlite has no
/// `ADD COLUMN IF NOT EXISTS`, so this checks `PRAGMA table_info` first
/// (same pattern used everywhere else idempotent schema evolution is
/// needed in this codebase) and only runs `ALTER TABLE` when the column
/// is genuinely absent. Existing rows backfill to `'default'` via the
/// column's own `DEFAULT` clause -- exactly the tenant every pre-tenant-
/// isolation session implicitly belonged to, since `TenantId::default_tenant`
/// is what every caller used before `X-Acpx-Tenant` existed.
fn migrate_tenant_id_column(conn: &Connection) -> Result<(), PersistenceError> {
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
    let has_tenant_id = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == "tenant_id");
    drop(stmt);
    if !has_tenant_id {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default'",
            [],
        )?;
    }
    Ok(())
}

fn lock(
    conn: &Arc<Mutex<Connection>>,
) -> Result<std::sync::MutexGuard<'_, Connection>, PersistenceError> {
    conn.lock().map_err(|_| PersistenceError::Poisoned)
}

/// Flatten `spawn_blocking`'s `Result<Result<T, E>, JoinError>` into
/// `Result<T, E>`, converting a panicked/cancelled blocking task into a
/// [`PersistenceError::TaskJoin`].
async fn run_blocking<F, T>(f: F) -> Result<T, PersistenceError>
where
    F: FnOnce() -> Result<T, PersistenceError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| PersistenceError::TaskJoin(e.to_string()))?
}
