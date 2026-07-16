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
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::error::PersistenceError;
use super::sessions::{RecoveryMetadata, RecoveryStatus, RecoveryStatusCounts, SessionRecord};
use super::transcripts::{Direction, TranscriptRecord};
use crate::custom_agents::CustomAgent;

#[derive(Clone)]
pub struct PersistenceStore {
    conn: Arc<Mutex<Connection>>,
    /// Counts written through this store. Comparing this against SQLite's
    /// actual count lets a reconnect distinguish normal ACPX persistence
    /// from an out-of-band transcript mutation.
    known_transcript_counts: Arc<Mutex<HashMap<String, usize>>>,
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
        migrate_sessions_columns(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            known_transcript_counts: Arc::new(Mutex::new(HashMap::new())),
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
        self.record_session_with_recovery(
            gateway_session_id,
            agent_id,
            backend_session_id,
            profile_name,
            created_at,
            tenant_id,
            RecoveryMetadata::default(),
        )
        .await
    }

    /// Record a newly-created session with durable recovery metadata.
    ///
    /// [`Self::record_session`] remains the compatibility API for callers
    /// that do not yet provide recovery data.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_session_with_recovery(
        &self,
        gateway_session_id: impl Into<String>,
        agent_id: impl Into<String>,
        backend_session_id: impl Into<String>,
        profile_name: Option<String>,
        created_at: impl Into<String>,
        tenant_id: impl Into<String>,
        recovery: RecoveryMetadata,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let agent_id = agent_id.into();
        let backend_session_id = backend_session_id.into();
        let created_at = created_at.into();
        let tenant_id = tenant_id.into();
        let recovery_params_json = recovery
            .recovery_params
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let created_at_unix_nanos = recovery
            .created_at_unix_nanos
            .unwrap_or_else(unix_time_nanos);
        let last_activity_at_unix_nanos = recovery
            .last_activity_at_unix_nanos
            .unwrap_or(created_at_unix_nanos);
        let bridge_config_options_json = recovery
            .bridge_config_options
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO sessions \
                 (gateway_session_id, agent_id, backend_session_id, profile_name, created_at, tenant_id, \
                  cwd, recovery_params_json, status, recovery_method, last_recovery_error, pinned, \
                  created_at_unix_nanos, last_activity_at_unix_nanos, bridge_session_id, \
                  bridge_model_alias, bridge_config_options_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    gateway_session_id,
                    agent_id,
                    backend_session_id,
                    profile_name,
                    created_at,
                    tenant_id,
                    recovery.cwd,
                    recovery_params_json,
                    recovery.status,
                    recovery.recovery_method,
                    recovery.last_recovery_error,
                    recovery.pinned as i64,
                    created_at_unix_nanos,
                    last_activity_at_unix_nanos,
                    recovery.bridge_session_id,
                    recovery.bridge_model_alias,
                    bridge_config_options_json
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
                "UPDATE sessions SET closed_at = ?1, status = 'closed' WHERE gateway_session_id = ?2",
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
                        created_at, closed_at, tenant_id, cwd, recovery_params_json, status, \
                        recovery_method, last_recovery_error, pinned, created_at_unix_nanos, \
                        last_activity_at_unix_nanos, bridge_session_id, bridge_model_alias, \
                        bridge_config_options_json \
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
                        created_at, closed_at, tenant_id, cwd, recovery_params_json, status, \
                        recovery_method, last_recovery_error, pinned, created_at_unix_nanos, \
                        last_activity_at_unix_nanos, bridge_session_id, bridge_model_alias, \
                        bridge_config_options_json \
                 FROM sessions ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], row_to_session_record)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await
    }

    /// List sessions that are still open and have an explicit recovery
    /// mechanism. These rows are candidates for startup recovery.
    pub async fn list_recoverable_sessions(&self) -> Result<Vec<SessionRecord>, PersistenceError> {
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn.prepare(
                "SELECT gateway_session_id, agent_id, backend_session_id, profile_name, \
                        created_at, closed_at, tenant_id, cwd, recovery_params_json, status, \
                        recovery_method, last_recovery_error, pinned, created_at_unix_nanos, \
                        last_activity_at_unix_nanos, bridge_session_id, bridge_model_alias, \
                        bridge_config_options_json \
                 FROM sessions \
                 WHERE closed_at IS NULL \
                   AND status != 'closed' \
                   AND recovery_method IN ('load', 'resume') \
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], row_to_session_record)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await
    }

    /// Persist the current startup-recovery result for a session.
    ///
    /// Passing `None` clears any previously stored recovery error.
    pub async fn update_recovery_status(
        &self,
        gateway_session_id: impl Into<String>,
        status: RecoveryStatus,
        last_recovery_error: Option<String>,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let last_recovery_error = last_recovery_error.map(bound_recovery_error);
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let rows = conn.execute(
                "UPDATE sessions \
                 SET status = ?1, last_recovery_error = ?2 \
                 WHERE gateway_session_id = ?3",
                params![status, last_recovery_error, gateway_session_id],
            )?;
            if rows == 0 {
                return Err(PersistenceError::SessionNotFound(gateway_session_id));
            }
            Ok(())
        })
        .await
    }

    /// Return a secret-free aggregate of durable recovery state for daemon
    /// readiness and health checks. Individual session identities and error
    /// strings remain in SQLite-only operator records.
    pub async fn recovery_status_counts(&self) -> Result<RecoveryStatusCounts, PersistenceError> {
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM sessions GROUP BY status")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, RecoveryStatus>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut counts = RecoveryStatusCounts::default();
            for row in rows {
                let (status, count) = row?;
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                match status {
                    RecoveryStatus::Active => counts.active = count,
                    RecoveryStatus::Restoring => counts.restoring = count,
                    RecoveryStatus::Restored => counts.restored = count,
                    RecoveryStatus::RecoveryFailed => counts.recovery_failed = count,
                    RecoveryStatus::Closed => counts.closed = count,
                }
            }
            Ok(counts)
        })
        .await
    }

    /// Record completed backend work without changing the session's explicit
    /// retention mode.
    pub async fn update_session_activity(
        &self,
        gateway_session_id: impl Into<String>,
        last_activity_at_unix_nanos: i64,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let rows = conn.execute(
                "UPDATE sessions SET last_activity_at_unix_nanos = ?1 WHERE gateway_session_id = ?2",
                params![last_activity_at_unix_nanos, gateway_session_id],
            )?;
            if rows == 0 {
                return Err(PersistenceError::SessionNotFound(gateway_session_id));
            }
            Ok(())
        })
        .await
    }

    /// Persist an explicit pin/unpin operation together with the activity
    /// refresh it induces.
    pub async fn update_session_pinned(
        &self,
        gateway_session_id: impl Into<String>,
        pinned: bool,
        last_activity_at_unix_nanos: i64,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let rows = conn.execute(
                "UPDATE sessions \
                 SET pinned = ?1, last_activity_at_unix_nanos = ?2 \
                 WHERE gateway_session_id = ?3",
                params![
                    pinned as i64,
                    last_activity_at_unix_nanos,
                    gateway_session_id
                ],
            )?;
            if rows == 0 {
                return Err(PersistenceError::SessionNotFound(gateway_session_id));
            }
            Ok(())
        })
        .await
    }

    pub async fn update_bridge_binding(
        &self,
        gateway_session_id: impl Into<String>,
        bridge_session_id: String,
        bridge_model_alias: String,
        bridge_config_options: Value,
    ) -> Result<(), PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let bridge_config_options = serde_json::to_string(&bridge_config_options)?;
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let rows = conn.execute(
                "UPDATE sessions SET bridge_session_id = ?1, bridge_model_alias = ?2, \
                 bridge_config_options_json = ?3 WHERE gateway_session_id = ?4",
                params![
                    bridge_session_id,
                    bridge_model_alias,
                    bridge_config_options,
                    gateway_session_id
                ],
            )?;
            if rows == 0 {
                return Err(PersistenceError::SessionNotFound(gateway_session_id));
            }
            Ok(())
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
        let insert_session_id = gateway_session_id.clone();
        let result = run_blocking(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO transcripts \
                 (gateway_session_id, direction, payload, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    insert_session_id,
                    direction.as_str(),
                    payload_text,
                    recorded_at
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await;
        if result.is_ok() {
            if let Some(count) = self
                .known_transcript_counts
                .lock()
                .map_err(|_| PersistenceError::Poisoned)?
                .get_mut(&gateway_session_id)
            {
                *count += 1;
            }
        }
        result
    }

    /// Return whether durable transcript state changed outside this store
    /// since it was last observed. The first observation establishes a
    /// baseline; successful [`Self::append_transcript`] calls advance it.
    pub async fn transcript_state_changed(
        &self,
        gateway_session_id: impl Into<String>,
    ) -> Result<bool, PersistenceError> {
        let gateway_session_id = gateway_session_id.into();
        let query_id = gateway_session_id.clone();
        let conn = self.conn.clone();
        let actual = run_blocking(move || {
            let conn = lock(&conn)?;
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM transcripts WHERE gateway_session_id = ?1",
                params![query_id],
                |row| row.get(0),
            )?;
            Ok(usize::try_from(count).expect("SQLite transcript count is non-negative"))
        })
        .await?;
        let mut known = self
            .known_transcript_counts
            .lock()
            .map_err(|_| PersistenceError::Poisoned)?;
        match known.get_mut(&gateway_session_id) {
            Some(expected) if *expected == actual => Ok(false),
            Some(expected) => {
                *expected = actual;
                Ok(true)
            }
            None => {
                known.insert(gateway_session_id, actual);
                Ok(false)
            }
        }
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

    pub(crate) async fn set_agent_enabled(
        &self,
        agent_id: impl Into<String>,
        enabled: bool,
    ) -> Result<(), PersistenceError> {
        let agent_id = agent_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            conn.execute(
                "INSERT INTO agent_enablement (agent_id, enabled) VALUES (?1, ?2) \
                 ON CONFLICT(agent_id) DO UPDATE SET enabled = excluded.enabled",
                params![agent_id, enabled as i64],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn agent_enabled(
        &self,
        agent_id: impl Into<String>,
    ) -> Result<Option<bool>, PersistenceError> {
        let agent_id = agent_id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut statement =
                conn.prepare("SELECT enabled FROM agent_enablement WHERE agent_id = ?1")?;
            let mut rows = statement.query(params![agent_id])?;
            match rows.next()? {
                Some(row) => match row.get::<_, i64>(0)? {
                    0 => Ok(Some(false)),
                    1 => Ok(Some(true)),
                    value => Err(PersistenceError::InvalidAgentEnablement(value)),
                },
                None => Ok(None),
            }
        })
        .await
    }

    pub(crate) async fn create_custom_agent(
        &self,
        agent: CustomAgent,
    ) -> Result<(), PersistenceError> {
        let args_json = serde_json::to_string(&agent.args)?;
        let env_json = serde_json::to_string(&agent.env)?;
        let id = agent.id.clone();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            match conn.execute(
                "INSERT INTO custom_agents (id, name, command, args_json, env_json, cwd) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    agent.id,
                    agent.name,
                    agent.command,
                    args_json,
                    env_json,
                    agent.cwd
                ],
            ) {
                Ok(_) => Ok(()),
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    Err(PersistenceError::CustomAgentAlreadyExists(id))
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    pub async fn get_custom_agent(
        &self,
        id: impl Into<String>,
    ) -> Result<Option<CustomAgent>, PersistenceError> {
        let id = id.into();
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut statement = conn.prepare(
                "SELECT id, name, command, args_json, env_json, cwd \
                 FROM custom_agents WHERE id = ?1",
            )?;
            let mut rows = statement.query(params![id])?;
            Ok(rows.next()?.map(row_to_custom_agent).transpose()?)
        })
        .await
    }

    pub async fn list_custom_agents(&self) -> Result<Vec<CustomAgent>, PersistenceError> {
        let conn = self.conn.clone();
        run_blocking(move || {
            let conn = lock(&conn)?;
            let mut statement = conn.prepare(
                "SELECT id, name, command, args_json, env_json, cwd \
                 FROM custom_agents ORDER BY id ASC",
            )?;
            let agents = statement
                .query_map([], row_to_custom_agent)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(PersistenceError::from)?;
            Ok(agents)
        })
        .await
    }

    pub(crate) async fn delete_custom_agent(
        &self,
        id: impl Into<String>,
    ) -> Result<(), PersistenceError> {
        let id = id.into();
        let query_id = id.clone();
        let conn = self.conn.clone();
        run_blocking(move || {
            let mut conn = lock(&conn)?;
            let transaction = conn.transaction()?;
            if transaction.execute("DELETE FROM custom_agents WHERE id = ?1", params![query_id])?
                == 0
            {
                return Err(PersistenceError::CustomAgentNotFound(id));
            }
            transaction.execute(
                "DELETE FROM agent_enablement WHERE agent_id = ?1",
                params![id],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }
}

fn row_to_custom_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<CustomAgent> {
    let args_json: String = row.get(3)?;
    let env_json: String = row.get(4)?;
    Ok(CustomAgent {
        id: row.get(0)?,
        name: row.get(1)?,
        command: row.get(2)?,
        args: serde_json::from_str(&args_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        env: serde_json::from_str(&env_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        cwd: row.get(5)?,
    })
}

fn row_to_session_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let recovery_params_json = row.get::<_, Option<String>>(8)?;
    let recovery_params = recovery_params_json
        .map(|text| {
            serde_json::from_str(&text).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })
        })
        .transpose()?;
    let bridge_config_options = row
        .get::<_, Option<String>>(17)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    17,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })
        })
        .transpose()?;
    Ok(SessionRecord {
        gateway_session_id: row.get(0)?,
        agent_id: row.get(1)?,
        backend_session_id: row.get(2)?,
        profile_name: row.get(3)?,
        created_at: row.get(4)?,
        closed_at: row.get(5)?,
        tenant_id: row.get(6)?,
        cwd: row.get(7)?,
        recovery_params,
        status: row.get(9)?,
        recovery_method: row.get(10)?,
        last_recovery_error: row.get(11)?,
        pinned: row.get::<_, i64>(12)? != 0,
        created_at_unix_nanos: row.get(13)?,
        last_activity_at_unix_nanos: row.get(14)?,
        bridge_session_id: row.get(15)?,
        bridge_model_alias: row.get(16)?,
        bridge_config_options,
    })
}

/// Idempotently add columns introduced after the first `sessions` schema.
///
/// `CREATE TABLE IF NOT EXISTS` never changes an existing table, and SQLite
/// has no `ADD COLUMN IF NOT EXISTS`, so upgrades inspect `PRAGMA table_info`
/// before applying each additive migration.
fn migrate_sessions_columns(conn: &Connection) -> Result<(), PersistenceError> {
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
    let column_names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for (name, definition) in [
        ("tenant_id", "TEXT NOT NULL DEFAULT 'default'"),
        ("cwd", "TEXT"),
        ("recovery_params_json", "TEXT"),
        ("status", "TEXT NOT NULL DEFAULT 'active'"),
        ("recovery_method", "TEXT NOT NULL DEFAULT 'none'"),
        ("last_recovery_error", "TEXT"),
        ("pinned", "INTEGER NOT NULL DEFAULT 0"),
        ("created_at_unix_nanos", "INTEGER"),
        ("last_activity_at_unix_nanos", "INTEGER"),
        ("bridge_session_id", "TEXT"),
        ("bridge_model_alias", "TEXT"),
        ("bridge_config_options_json", "TEXT"),
    ] {
        if !column_names.iter().any(|column| column == name) {
            conn.execute(
                &format!("ALTER TABLE sessions ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn unix_time_nanos() -> i64 {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

const MAX_RECOVERY_ERROR_BYTES: usize = 512;

fn bound_recovery_error(error: String) -> String {
    let flattened = error.replace(['\n', '\r'], " ");
    if flattened.len() <= MAX_RECOVERY_ERROR_BYTES {
        return flattened;
    }
    let mut end = MAX_RECOVERY_ERROR_BYTES;
    while !flattened.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &flattened[..end])
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn transcript_state_detects_an_out_of_band_database_write() {
        let store = PersistenceStore::open_in_memory().expect("in-memory store");
        store
            .record_session(
                "session-1",
                "agent-1",
                "backend-1",
                None,
                "2026-07-16T00:00:00Z",
                "default",
            )
            .await
            .expect("parent session");
        assert!(!store
            .transcript_state_changed("session-1")
            .await
            .expect("baseline"));
        store
            .append_transcript(
                "session-1",
                Direction::ClientToAgent,
                json!({"method": "session/prompt"}),
                "2026-07-16T00:00:00Z",
            )
            .await
            .expect("normal ACPX write");
        assert!(!store
            .transcript_state_changed("session-1")
            .await
            .expect("normal write stays current"));

        // This models an external session-file/import/database mutation:
        // it reaches SQLite but never advances PersistenceStore's known
        // count, so a later resume must invalidate its old epoch.
        lock(&store.conn)
            .expect("sqlite lock")
            .execute(
                "INSERT INTO transcripts \
                 (gateway_session_id, direction, payload, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    "session-1",
                    "agent_to_client",
                    r#"{"method":"session/update"}"#,
                    "2026-07-16T00:00:01Z"
                ],
            )
            .expect("out-of-band insert");
        assert!(store
            .transcript_state_changed("session-1")
            .await
            .expect("external write is detected"));
        assert!(!store
            .transcript_state_changed("session-1")
            .await
            .expect("new baseline is acknowledged once"));
    }
}
