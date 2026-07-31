//! Durable panel-local settings, deliberately separate from ACPX sessions.
//!
//! Only safe UI defaults and per-thread presentation policy live here. ACPX
//! credentials, launch overrides, terminal environments, and raw prompt data
//! must never be persisted in this database.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelDefaults {
    pub profile_name: Option<String>,
    pub permission_profile: Option<String>,
    pub background_session: bool,
    pub selected_thread_id: Option<String>,
}

impl Default for PanelDefaults {
    fn default() -> Self {
        Self {
            profile_name: None,
            permission_profile: None,
            background_session: false,
            selected_thread_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSettings {
    pub thread_id: String,
    pub session_id: Option<String>,
    pub profile_name: Option<String>,
    pub permission_profile: Option<String>,
    pub background_session: Option<bool>,
}

/// Panel-local cache of server-derived snapflowd session state. This is a
/// restart-safe presentation cache; the daemon remains authoritative and the
/// WebSocket client refreshes it whenever a live connection is available.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDerivedState {
    pub session_id: String,
    pub acp_session_id: Option<String>,
    pub project_id: Option<String>,
    pub project_path: Option<String>,
    pub connection_status: String,
    pub revision: u64,
}

/// The durable identity needed to restore a panel thread before its transcript
/// cache and ACPX session are reconciled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRecord {
    pub thread_id: String,
    pub display_name: String,
    pub provider: String,
    pub session_id: String,
    pub profile_name: Option<String>,
    pub permission_profile: Option<String>,
    pub background_session: Option<bool>,
    /// PISO-3 (project-isolation-mlt-binding plan): the MLT project *file
    /// path* this thread's session was opened/resumed against, mirroring
    /// `ThreadSlot::project_path`'s own doc comment -- captured once at
    /// session-open time and never updated afterward. `None` for a thread
    /// created before this column existed, or one created with no MLT
    /// project open at all; both are treated identically as "unscoped",
    /// shown regardless of which project is active (see
    /// `models::retain_items_for_project`). Legacy rows never gain a value
    /// retroactively -- there is no way to know after the fact which
    /// project a pre-migration thread belonged to.
    ///
    /// Stored as a PATH, not a synthesized project id, deliberately: every
    /// existing consumer (`AgentBridge::session_cwd_override`,
    /// `PanelModel::active_project_path`, `cwd_for_session`,
    /// `retain_items_for_project`) already compares raw MLT project file
    /// paths, so a path needs no new lookup table and stays comparable
    /// with zero translation at every call site. The known cost: a
    /// Save-As or on-disk rename changes the path out from under an
    /// already-recorded row, and this phase does not detect or reconcile
    /// that -- the row simply keeps its old path (same "stranded" outcome
    /// `ThreadSlot::project_path`'s capture-once design already accepts
    /// for the live in-memory value; this durable copy inherits the same
    /// limitation rather than introducing a new one). A future phase
    /// (PISO-1 propagates Save-As/rename from the host) can add
    /// rename-aware rebinding on top of this column without a schema
    /// change.
    pub project_path: Option<String>,
}

/// A durable candidate for `acpx_client::pool::ProjectSessionPool`'s idle
/// key (`project_dir` + `agent_id` + `provider_profile`). `pool_status`
/// and `leased_thread_id` mirror the plan's diagnostic/recovery hint
/// fields -- SQL is never the runtime source of truth for lease ownership,
/// only for which session ids are worth trying to `session/resume` after a
/// restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolBindingRecord {
    pub project_dir: String,
    pub agent_id: String,
    pub provider_profile: String,
    pub session_id: String,
    pub desired_config_options: Option<String>,
    pub pool_status: Option<String>,
    pub leased_thread_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    #[error("SQLite panel-state error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("panel-state cache dir error: {0}")]
    CacheDir(#[from] std::io::Error),
    #[error("thread {thread_id:?} is already bound to session {existing_session_id:?}")]
    SessionBindingConflict {
        thread_id: String,
        existing_session_id: String,
    },
    #[error("thread {thread_id:?} has immutable profile settings after session binding")]
    BoundSettingsConflict { thread_id: String },
}

pub struct PanelStateStore {
    connection: Mutex<Connection>,
}

impl PanelStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateStoreError> {
        let path = path.as_ref();
        // The cache dir may not exist yet: this store opens BEFORE the
        // jsonl transcript store (which is what used to create
        // rui-thread-cache/ as a side effect), and every per-project
        // daemon launch runs under a freshly created sandboxed $HOME
        // (snapshotd procmgr's qtHomeDir), so on that path the very
        // first open always failed with "unable to open database file"
        // and the panel silently ran without settings/thread
        // persistence for the whole session -- found live on the
        // video-generation-e2e-harness VNC demo launch.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, StateStoreError> {
        connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS panel_defaults (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                profile_name TEXT,
                permission_profile TEXT,
                background_session INTEGER NOT NULL CHECK (background_session IN (0, 1)),
                selected_thread_id TEXT
            );
            CREATE TABLE IF NOT EXISTS thread_settings (
                thread_id TEXT PRIMARY KEY NOT NULL,
                session_id TEXT,
                profile_name TEXT,
                permission_profile TEXT,
                background_session INTEGER CHECK (background_session IN (0, 1)),
                display_name TEXT,
                provider TEXT
            );
            CREATE TABLE IF NOT EXISTS session_derived_state (
                session_id TEXT PRIMARY KEY NOT NULL,
                acp_session_id TEXT,
                project_id TEXT,
                project_path TEXT,
                connection_status TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0
            );
            ",
        )?;
        Self::add_column_if_missing(&connection, "display_name", "TEXT")?;
        Self::add_column_if_missing(&connection, "provider", "TEXT")?;
        Self::add_defaults_column_if_missing(&connection, "selected_thread_id", "TEXT")?;
        // PISO-3: durable thread<->project association. An existing
        // database from before this column existed migrates in place via
        // `add_column_if_missing` -- every pre-existing row simply reads
        // back with `project_path = NULL` (see `ThreadRecord::project_path`'s
        // doc comment for why that is the correct, permanent state for
        // those rows rather than a value to backfill).
        Self::add_column_if_missing(&connection, "project_path", "TEXT")?;
        // acpx-client-session-lease-pool plan: durable project/agent/
        // provider -> session binding for the client-side lease pool.
        // `pool_status`/`leased_thread_id` are diagnostic/recovery hints
        // only -- the in-process `ProjectSessionPool` is authoritative
        // while panel-rust is running; on restart these rows are the
        // candidates the pool validates with `session/resume` before
        // reuse. Distinct from `thread_settings` (per-thread presentation
        // state): a pool binding is keyed by project+agent+provider, never
        // by thread_id, matching the plan's idle-pool key.
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS project_session_pool_bindings (
                project_dir TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                provider_profile TEXT NOT NULL,
                session_id TEXT NOT NULL,
                desired_config_options TEXT,
                pool_status TEXT,
                leased_thread_id TEXT,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (project_dir, agent_id, provider_profile, session_id)
            );
            ",
        )?;
        connection.execute_batch("PRAGMA user_version = 5;")?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn add_column_if_missing(
        connection: &Connection,
        column: &str,
        definition: &str,
    ) -> Result<(), StateStoreError> {
        let exists = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('thread_settings') WHERE name = ?1
             )",
            [column],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            connection.execute(
                &format!("ALTER TABLE thread_settings ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
        Ok(())
    }

    fn add_defaults_column_if_missing(
        connection: &Connection,
        column: &str,
        definition: &str,
    ) -> Result<(), StateStoreError> {
        let exists = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('panel_defaults') WHERE name = ?1
             )",
            [column],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            connection.execute(
                &format!("ALTER TABLE panel_defaults ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self, StateStoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    pub fn defaults(&self) -> Result<PanelDefaults, StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection
            .query_row(
                "SELECT profile_name, permission_profile, background_session, selected_thread_id
                 FROM panel_defaults WHERE id = 1",
                [],
                |row| {
                    Ok(PanelDefaults {
                        profile_name: row.get(0)?,
                        permission_profile: row.get(1)?,
                        background_session: row.get::<_, i64>(2)? != 0,
                        selected_thread_id: row.get(3)?,
                    })
                },
            )
            .optional()
            .map(|stored| stored.unwrap_or_default())
            .map_err(Into::into)
    }

    pub fn save_session_derived_state(
        &self,
        state: &SessionDerivedState,
    ) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO session_derived_state
                (session_id, acp_session_id, project_id, project_path, connection_status, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                acp_session_id = excluded.acp_session_id,
                project_id = excluded.project_id,
                project_path = excluded.project_path,
                connection_status = excluded.connection_status,
                revision = excluded.revision
             WHERE excluded.revision >= session_derived_state.revision",
            params![
                state.session_id,
                state.acp_session_id,
                state.project_id,
                state.project_path,
                state.connection_status,
                state.revision as i64,
            ],
        )?;
        Ok(())
    }

    pub fn session_derived_state(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionDerivedState>, StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection
            .query_row(
                "SELECT session_id, acp_session_id, project_id, project_path, connection_status, revision
                 FROM session_derived_state WHERE session_id = ?1",
                [session_id],
                |row| Ok(SessionDerivedState {
                    session_id: row.get(0)?, acp_session_id: row.get(1)?, project_id: row.get(2)?,
                    project_path: row.get(3)?, connection_status: row.get(4)?, revision: row.get::<_, i64>(5)? as u64,
                }),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn all_session_derived_states(&self) -> Result<Vec<SessionDerivedState>, StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let mut statement = connection.prepare(
            "SELECT session_id, acp_session_id, project_id, project_path, connection_status, revision
             FROM session_derived_state ORDER BY session_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionDerivedState {
                session_id: row.get(0)?,
                acp_session_id: row.get(1)?,
                project_id: row.get(2)?,
                project_path: row.get(3)?,
                connection_status: row.get(4)?,
                revision: row.get::<_, i64>(5)? as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn save_defaults(&self, defaults: &PanelDefaults) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO panel_defaults
                (id, profile_name, permission_profile, background_session, selected_thread_id)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                profile_name = excluded.profile_name,
                permission_profile = excluded.permission_profile,
                background_session = excluded.background_session,
                selected_thread_id = excluded.selected_thread_id",
            params![
                defaults.profile_name,
                defaults.permission_profile,
                i64::from(defaults.background_session),
                defaults.selected_thread_id,
            ],
        )?;
        Ok(())
    }

    /// Persists the active panel thread independently of settings-sheet
    /// edits, so selecting a thread is durable even when the sheet is never
    /// opened or saved in that host session.
    pub fn set_selected_thread_id(
        &self,
        selected_thread_id: Option<&str>,
    ) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO panel_defaults
                (id, profile_name, permission_profile, background_session, selected_thread_id)
             VALUES (1, NULL, NULL, 0, ?1)
             ON CONFLICT(id) DO UPDATE SET
                selected_thread_id = excluded.selected_thread_id",
            [selected_thread_id],
        )?;
        Ok(())
    }

    pub fn thread_settings(
        &self,
        thread_id: &str,
    ) -> Result<Option<ThreadSettings>, StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection
            .query_row(
                "SELECT thread_id, session_id, profile_name, permission_profile, background_session
                 FROM thread_settings WHERE thread_id = ?1",
                [thread_id],
                |row| {
                    Ok(ThreadSettings {
                        thread_id: row.get(0)?,
                        session_id: row.get(1)?,
                        profile_name: row.get(2)?,
                        permission_profile: row.get(3)?,
                        background_session: row.get::<_, Option<i64>>(4)?.map(|value| value != 0),
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Returns restoreable thread records in stable insertion order. Legacy
    /// rows without a display name/provider are intentionally skipped: they
    /// remain available through `thread_settings`, but do not provide enough
    /// information to safely reconstruct a live panel thread.
    pub fn thread_records(&self) -> Result<Vec<ThreadRecord>, StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let mut statement = connection.prepare(
            "SELECT thread_id, display_name, provider, session_id,
                    profile_name, permission_profile, background_session, project_path
             FROM thread_settings
             WHERE display_name IS NOT NULL
               AND provider IS NOT NULL
               AND session_id IS NOT NULL
             ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ThreadRecord {
                thread_id: row.get(0)?,
                display_name: row.get(1)?,
                provider: row.get(2)?,
                session_id: row.get(3)?,
                profile_name: row.get(4)?,
                permission_profile: row.get(5)?,
                background_session: row.get::<_, Option<i64>>(6)?.map(|value| value != 0),
                project_path: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Persists a thread's local identity once an ACPX session is bound.
    /// Session/profile immutability is enforced by `bind_session`; this method
    /// only adds the panel-specific display name and provider needed on the
    /// next host launch.
    pub fn save_thread_record(&self, record: &ThreadRecord) -> Result<(), StateStoreError> {
        match self.bind_session(
            &record.thread_id,
            &record.session_id,
            record.profile_name.as_deref(),
            record.permission_profile.as_deref(),
        ) {
            // `thread_already_bound_resume_errors` (consolidation plan
            // phase 9): a DIFFERENT session id landing on a thread that
            // already has one is the sanctioned supersede path -- the
            // attachment flow only ever opens a fresh session after the
            // persisted one failed to resume (dead gateway session,
            // relaunch races), so erroring here surfaced a per-thread
            // "already bound" card on every such relaunch. Rebind the
            // record to the live session instead; the thread's jsonl
            // transcript is keyed by thread_id and is unaffected. The
            // profile-immutability rule (BoundSettingsConflict) still
            // holds -- only the session id is superseded.
            Err(StateStoreError::SessionBindingConflict { .. }) => {
                let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
                connection.execute(
                    "UPDATE thread_settings SET session_id = ?2 WHERE thread_id = ?1",
                    params![record.thread_id, record.session_id],
                )?;
            }
            other => other?,
        }
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "UPDATE thread_settings
             SET display_name = ?2, provider = ?3, project_path = ?4
             WHERE thread_id = ?1",
            params![
                record.thread_id,
                record.display_name,
                record.provider,
                record.project_path,
            ],
        )?;
        Ok(())
    }

    /// Updates only the local display name. The stable thread id and ACP
    /// session binding remain untouched, so renaming never creates a session.
    pub fn update_thread_display_name(
        &self,
        thread_id: &str,
        display_name: &str,
    ) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "UPDATE thread_settings SET display_name = ?2 WHERE thread_id = ?1",
            params![thread_id, display_name],
        )?;
        Ok(())
    }

    /// Profile and permission bindings become immutable once `session/new`
    /// succeeds. Changing either must create a new thread/session instead of
    /// silently migrating a populated transcript.
    pub fn bind_session(
        &self,
        thread_id: &str,
        session_id: &str,
        profile_name: Option<&str>,
        permission_profile: Option<&str>,
    ) -> Result<(), StateStoreError> {
        if let Some(existing) = self.thread_settings(thread_id)? {
            if let Some(existing_session_id) = existing.session_id {
                if existing_session_id != session_id {
                    return Err(StateStoreError::SessionBindingConflict {
                        thread_id: thread_id.to_owned(),
                        existing_session_id,
                    });
                }
                if existing.profile_name.as_deref() != profile_name
                    || existing.permission_profile.as_deref() != permission_profile
                {
                    return Err(StateStoreError::BoundSettingsConflict {
                        thread_id: thread_id.to_owned(),
                    });
                }
                return Ok(());
            }
        }

        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO thread_settings
                (thread_id, session_id, profile_name, permission_profile, background_session)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(thread_id) DO UPDATE SET
                session_id = excluded.session_id,
                profile_name = excluded.profile_name,
                permission_profile = excluded.permission_profile",
            params![thread_id, session_id, profile_name, permission_profile],
        )?;
        Ok(())
    }

    /// Upserts one durable project/agent/provider -> session candidate for
    /// the client-side lease pool. `desired_config_options` is caller-
    /// serialized (e.g. JSON) opaque text; this store does not interpret
    /// it. Keyed by `(project_dir, agent_id, provider_profile, session_id)`
    /// so a provider that has cycled through multiple session ids over
    /// time (e.g. after invalidation) keeps its prior rows as history
    /// rather than clobbering them -- callers wanting "the" binding should
    /// use [`Self::pool_bindings_for_project`] and prefer the most
    /// recently `updated_at`.
    pub fn save_pool_binding(&self, binding: &PoolBindingRecord) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO project_session_pool_bindings
                (project_dir, agent_id, provider_profile, session_id,
                 desired_config_options, pool_status, leased_thread_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(project_dir, agent_id, provider_profile, session_id) DO UPDATE SET
                desired_config_options = excluded.desired_config_options,
                pool_status = excluded.pool_status,
                leased_thread_id = excluded.leased_thread_id,
                updated_at = excluded.updated_at",
            params![
                binding.project_dir,
                binding.agent_id,
                binding.provider_profile,
                binding.session_id,
                binding.desired_config_options,
                binding.pool_status,
                binding.leased_thread_id,
                binding.updated_at,
            ],
        )?;
        Ok(())
    }

    /// All persisted pool-binding candidates for one project, newest
    /// `updated_at` first. Restart hydration reads this to find prior
    /// activated providers; each candidate must still be proven usable
    /// with `session/resume` before the in-memory pool treats it as idle.
    pub fn pool_bindings_for_project(
        &self,
        project_dir: &str,
    ) -> Result<Vec<PoolBindingRecord>, StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let mut statement = connection.prepare(
            "SELECT project_dir, agent_id, provider_profile, session_id,
                    desired_config_options, pool_status, leased_thread_id, updated_at
             FROM project_session_pool_bindings
             WHERE project_dir = ?1
             ORDER BY updated_at DESC",
        )?;
        let rows = statement
            .query_map(params![project_dir], |row| {
                Ok(PoolBindingRecord {
                    project_dir: row.get(0)?,
                    agent_id: row.get(1)?,
                    provider_profile: row.get(2)?,
                    session_id: row.get(3)?,
                    desired_config_options: row.get(4)?,
                    pool_status: row.get(5)?,
                    leased_thread_id: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Removes one binding row -- called when the pool invalidates a
    /// session (stale/closed/auth-failed), so a restart never re-offers a
    /// candidate already confirmed unusable.
    pub fn delete_pool_binding(
        &self,
        project_dir: &str,
        agent_id: &str,
        provider_profile: &str,
        session_id: &str,
    ) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "DELETE FROM project_session_pool_bindings
             WHERE project_dir = ?1 AND agent_id = ?2 AND provider_profile = ?3 AND session_id = ?4",
            params![project_dir, agent_id, provider_profile, session_id],
        )?;
        Ok(())
    }

    pub fn set_background_override(
        &self,
        thread_id: &str,
        background_session: Option<bool>,
    ) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO thread_settings
                (thread_id, session_id, profile_name, permission_profile, background_session)
             VALUES (?1, NULL, NULL, NULL, ?2)
             ON CONFLICT(thread_id) DO UPDATE SET
                background_session = excluded.background_session",
            params![thread_id, background_session.map(i64::from)],
        )?;
        Ok(())
    }

    pub fn effective_background_session(&self, thread_id: &str) -> Result<bool, StateStoreError> {
        Ok(self
            .thread_settings(thread_id)?
            .and_then(|settings| settings.background_session)
            .unwrap_or(self.defaults()?.background_session))
    }

    /// PISO-7 (project-isolation-mlt-binding plan): the durable half of a
    /// Save-As rebind -- rewrites every row whose `project_path` equals
    /// `old` to `new`. Rows recorded against a DIFFERENT project, or with
    /// no recorded project at all (`project_path IS NULL`, the SQL
    /// equality below never matches those either way), are untouched.
    ///
    /// This alone only takes effect on the NEXT restart: the live
    /// session's visible thread list reads `AgentBridge::thread_project_
    /// path`, which reads each `ThreadSlot`'s own in-memory copy, not
    /// sqlite -- `AgentBridge::rebind_project_path` is the matching live
    /// half, and both must be called together (see the `Effect::
    /// RenameProjectAssociation` handler, the only caller of either).
    ///
    /// Callers must never pass an empty `old`: that would mean "every
    /// legacy/never-scoped row", and an untitled project's first save is
    /// NOT a rename of anything (those threads were created unscoped on
    /// purpose and must stay that way) -- `update_host`'s `ProjectPath
    /// Renamed` handler guards this before it ever reaches here.
    pub fn rename_project_path(&self, old: &str, new: &str) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "UPDATE thread_settings SET project_path = ?2 WHERE project_path = ?1",
            params![old, new],
        )?;
        Ok(())
    }

    /// First-save migration for rows created while the project was Untitled.
    /// Those rows intentionally carry NULL project_path until the staging
    /// identity becomes a saved MLT identity; update only those rows so a
    /// different project's durable chats can never be rehomed accidentally.
    pub fn assign_unscoped_project_path(&self, new: &str) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "UPDATE thread_settings SET project_path = ?1 WHERE project_path IS NULL",
            params![new],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_binding_upsert_updates_status_without_duplicating_rows() {
        let store = PanelStateStore::in_memory().unwrap();
        let mut binding = PoolBindingRecord {
            project_dir: "/proj".to_string(),
            agent_id: "agent-1".to_string(),
            provider_profile: "codex/default".to_string(),
            session_id: "sess-1".to_string(),
            desired_config_options: Some("{}".to_string()),
            pool_status: Some("idle".to_string()),
            leased_thread_id: None,
            updated_at: "1".to_string(),
        };
        store.save_pool_binding(&binding).unwrap();
        binding.pool_status = Some("leased".to_string());
        binding.leased_thread_id = Some("thread-1".to_string());
        binding.updated_at = "2".to_string();
        store.save_pool_binding(&binding).unwrap();

        let rows = store.pool_bindings_for_project("/proj").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pool_status.as_deref(), Some("leased"));
        assert_eq!(rows[0].leased_thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn pool_bindings_are_isolated_per_project() {
        let store = PanelStateStore::in_memory().unwrap();
        let make = |project_dir: &str| PoolBindingRecord {
            project_dir: project_dir.to_string(),
            agent_id: "agent-1".to_string(),
            provider_profile: "codex/default".to_string(),
            session_id: "sess-1".to_string(),
            desired_config_options: None,
            pool_status: None,
            leased_thread_id: None,
            updated_at: "1".to_string(),
        };
        store.save_pool_binding(&make("/proj-a")).unwrap();
        store.save_pool_binding(&make("/proj-b")).unwrap();

        assert_eq!(store.pool_bindings_for_project("/proj-a").unwrap().len(), 1);
        assert_eq!(store.pool_bindings_for_project("/proj-c").unwrap().len(), 0);
    }

    #[test]
    fn delete_pool_binding_removes_only_the_matching_row() {
        let store = PanelStateStore::in_memory().unwrap();
        let binding = PoolBindingRecord {
            project_dir: "/proj".to_string(),
            agent_id: "agent-1".to_string(),
            provider_profile: "codex/default".to_string(),
            session_id: "sess-stale".to_string(),
            desired_config_options: None,
            pool_status: Some("invalid".to_string()),
            leased_thread_id: None,
            updated_at: "1".to_string(),
        };
        store.save_pool_binding(&binding).unwrap();
        store
            .delete_pool_binding("/proj", "agent-1", "codex/default", "sess-stale")
            .unwrap();
        assert!(store.pool_bindings_for_project("/proj").unwrap().is_empty());
    }

    /// mutex_poison_convention_unification: a panic in one caller while
    /// holding `connection`'s lock must not permanently wedge every future
    /// caller -- `.lock().unwrap_or_else(|e| e.into_inner())` self-heals
    /// instead of the old `.expect("... poisoned")`, which would panic
    /// again (forever) on the very next call.
    #[test]
    fn a_poisoned_connection_mutex_self_heals_instead_of_wedging_every_future_caller() {
        let store = std::sync::Arc::new(PanelStateStore::in_memory().unwrap());
        let poisoning = store.clone();
        let joined = std::thread::spawn(move || {
            let _connection = poisoning.connection.lock().unwrap();
            panic!("intentionally poison the mutex while holding the guard");
        })
        .join();
        assert!(joined.is_err(), "the spawned thread should have panicked");

        // Pre-fix (.expect("... poisoned")) this call would panic again,
        // forever, for the rest of the process's life.
        let defaults = store.defaults();
        assert!(
            defaults.is_ok(),
            "connection mutex should have self-healed, got {defaults:?}"
        );
    }

    /// SCNA-10: a real, deterministic mid-session write-failure trigger.
    /// The store's default rollback-journal mode needs to create/delete a
    /// `-journal` sibling file in the same directory on every write, even
    /// though the main .sqlite3 file itself stays writable -- so making
    /// *just the containing directory* read-only after the connection is
    /// already open reproduces a real "attempt to write a readonly
    /// database" failure on an already-open connection, without any
    /// test-only production hook. Restoring directory permissions heals it
    /// again with no code changes needed, same as the poison-mutex tests.
    #[test]
    fn a_mid_session_write_fails_when_the_state_dir_becomes_read_only_after_open() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("panel-state.sqlite3");
        let store = PanelStateStore::open(&db_path).unwrap();
        // Succeeds while the directory is still writable.
        store.save_defaults(&PanelDefaults::default()).unwrap();

        let original_mode = std::fs::metadata(dir.path()).unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = store.save_defaults(&PanelDefaults {
            profile_name: Some("codex".to_owned()),
            ..PanelDefaults::default()
        });
        assert!(
            matches!(result, Err(StateStoreError::Sql(_))),
            "write against a read-only state dir must surface as StateStoreError::Sql, got {result:?}"
        );

        // Restore so tempdir's own Drop cleanup can remove the directory.
        std::fs::set_permissions(dir.path(), original_mode).unwrap();
        assert!(
            store.save_defaults(&PanelDefaults::default()).is_ok(),
            "write must succeed again once the directory is writable"
        );
    }

    #[test]
    fn defaults_and_background_override_restore_without_transcript_data() {
        let store = PanelStateStore::in_memory().unwrap();
        let defaults = PanelDefaults {
            profile_name: Some("codex".to_owned()),
            permission_profile: Some("review".to_owned()),
            background_session: true,
            selected_thread_id: Some("thread-b".to_owned()),
        };
        store.save_defaults(&defaults).unwrap();
        store
            .set_background_override("thread-a", Some(false))
            .unwrap();

        assert_eq!(store.defaults().unwrap(), defaults);
        assert!(!store.effective_background_session("thread-a").unwrap());
        assert!(store.effective_background_session("thread-b").unwrap());
        assert_eq!(
            store.defaults().unwrap().selected_thread_id.as_deref(),
            Some("thread-b")
        );
    }

    #[test]
    fn session_derived_state_is_revision_ordered_and_restart_safe() {
        let store = PanelStateStore::in_memory().unwrap();
        let state = SessionDerivedState {
            session_id: "snap-1".to_owned(),
            acp_session_id: Some("acp-1".to_owned()),
            project_id: Some("project-1".to_owned()),
            project_path: Some("/projects/demo.mlt".to_owned()),
            connection_status: "connected".to_owned(),
            revision: 3,
        };
        store.save_session_derived_state(&state).unwrap();
        store
            .save_session_derived_state(&SessionDerivedState {
                project_path: Some("/projects/stale.mlt".to_owned()),
                revision: 2,
                ..state.clone()
            })
            .unwrap();
        assert_eq!(store.session_derived_state("snap-1").unwrap(), Some(state));
    }

    #[test]
    fn save_thread_record_supersedes_a_dead_session_binding() {
        // consolidation plan phase 9: relaunch after a failed resume
        // opens a fresh session; persisting it must rebind, not error.
        let store = PanelStateStore::in_memory().unwrap();
        let mut record = ThreadRecord {
            thread_id: "t".to_owned(),
            display_name: "T".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-1".to_owned(),
            profile_name: None,
            permission_profile: None,
            background_session: None,
            project_path: None,
        };
        store.save_thread_record(&record).unwrap();
        record.session_id = "session-2".to_owned();
        store.save_thread_record(&record).unwrap();
        let settings = store.thread_settings("t").unwrap().unwrap();
        assert_eq!(settings.session_id.as_deref(), Some("session-2"));
    }

    #[test]
    fn session_binding_cannot_migrate_profile_or_session() {
        let store = PanelStateStore::in_memory().unwrap();
        store
            .bind_session("thread-a", "session-1", Some("codex"), Some("review"))
            .unwrap();
        store
            .bind_session("thread-a", "session-1", Some("codex"), Some("review"))
            .unwrap();
        assert!(matches!(
            store.bind_session("thread-a", "session-2", Some("codex"), Some("review")),
            Err(StateStoreError::SessionBindingConflict { .. })
        ));
        assert!(matches!(
            store.bind_session("thread-a", "session-1", Some("claude"), Some("review")),
            Err(StateStoreError::BoundSettingsConflict { .. })
        ));
    }

    #[test]
    fn thread_records_restore_in_creation_order_with_their_binding() {
        let store = PanelStateStore::in_memory().unwrap();
        let first = ThreadRecord {
            thread_id: "timeline".to_owned(),
            display_name: "Fix timeline".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-1".to_owned(),
            profile_name: Some("review".to_owned()),
            permission_profile: None,
            background_session: None,
            project_path: Some("/projects/timeline.mlt".to_owned()),
        };
        let second = ThreadRecord {
            thread_id: "filters".to_owned(),
            display_name: "Refactor filters".to_owned(),
            provider: "claude".to_owned(),
            session_id: "session-2".to_owned(),
            profile_name: None,
            permission_profile: Some("confirm".to_owned()),
            background_session: Some(true),
            project_path: None,
        };
        store.save_thread_record(&first).unwrap();
        store
            .set_background_override(&second.thread_id, second.background_session)
            .unwrap();
        store.save_thread_record(&second).unwrap();

        assert_eq!(store.thread_records().unwrap(), vec![first, second]);
    }

    /// **`panel-rust-e2e-hardening`'s `default_thread_not_linked_to_real_agent`
    /// phase.** `update.rs`'s `ThreadMsg::New` and `settings_file.rs`'s
    /// `resolved_to_panel_defaults` both already guard against the literal
    /// "default" sentinel (a reserved acpx-server placeholder, never a
    /// real profile name) at their own point of use/write -- but a thread
    /// record persisted to `panel-state.sqlite3` *before* either fix
    /// landed (or written by any other path that predates them) still
    /// round-trips that literal string through this store untouched.
    /// `lib.rs`'s cold-start restoration reads `thread_records()` directly
    /// into `ThreadSpec::profile_name`/the permission-profile list with no
    /// guard at all -- this proves the actual fix (wrapping both fields in
    /// `settings_file::non_default_sentinel` at that read site) strips a
    /// real, sqlite-round-tripped sentinel while leaving a real profile
    /// name untouched, using the exact same composition lib.rs's cold
    /// start calls.
    #[test]
    fn a_persisted_default_sentinel_profile_is_stripped_on_restore_not_forwarded_to_a_real_session()
    {
        let store = PanelStateStore::in_memory().unwrap();
        let poisoned = ThreadRecord {
            thread_id: "legacy-thread".to_owned(),
            display_name: "Legacy Thread".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-legacy".to_owned(),
            profile_name: Some("default".to_owned()),
            permission_profile: Some("default".to_owned()),
            background_session: None,
            project_path: None,
        };
        let real = ThreadRecord {
            thread_id: "real-thread".to_owned(),
            display_name: "Real Thread".to_owned(),
            provider: "claude".to_owned(),
            session_id: "session-real".to_owned(),
            profile_name: Some("my-real-profile".to_owned()),
            permission_profile: Some("workspace".to_owned()),
            background_session: None,
            project_path: None,
        };
        store.save_thread_record(&poisoned).unwrap();
        store.save_thread_record(&real).unwrap();

        let restored = store.thread_records().unwrap();
        assert_eq!(restored, vec![poisoned, real]);

        // The actual fix: the same composition lib.rs's cold-start
        // restoration applies at the point it builds ThreadSpec/
        // initial_permission_profiles from these records.
        let sanitized: Vec<(Option<String>, Option<String>)> = restored
            .iter()
            .map(|record| {
                (
                    crate::settings_file::non_default_sentinel(record.profile_name.clone()),
                    crate::settings_file::non_default_sentinel(record.permission_profile.clone()),
                )
            })
            .collect();
        assert_eq!(
            sanitized[0],
            (None, None),
            "the literal \"default\" sentinel must never survive into a real session/new call"
        );
        assert_eq!(
            sanitized[1],
            (
                Some("my-real-profile".to_owned()),
                Some("workspace".to_owned())
            ),
            "a real, non-sentinel profile name must pass through unchanged"
        );
    }

    #[test]
    fn update_thread_display_name_preserves_durable_binding() {
        let store = PanelStateStore::in_memory().unwrap();
        let record = ThreadRecord {
            thread_id: "timeline".to_owned(),
            display_name: "Fix timeline".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-1".to_owned(),
            profile_name: Some("review".to_owned()),
            permission_profile: None,
            background_session: None,
            project_path: None,
        };
        store.save_thread_record(&record).unwrap();
        store
            .update_thread_display_name(&record.thread_id, "Repair timeline")
            .unwrap();

        assert_eq!(
            store.thread_records().unwrap(),
            vec![ThreadRecord {
                display_name: "Repair timeline".to_owned(),
                ..record
            }]
        );
    }

    /// PISO-7: the durable half of a Save-As rebind. Two threads on
    /// different projects plus one unscoped thread -- renaming A -> B
    /// must move only A's row, leave B's alone, and leave the unscoped
    /// row's NULL untouched (an untitled project's first save is not a
    /// rename of anything).
    #[test]
    fn rename_project_path_rewrites_only_matching_rows() {
        let store = PanelStateStore::in_memory().unwrap();
        let on_a = ThreadRecord {
            thread_id: "on-a".to_owned(),
            display_name: "On A".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-a".to_owned(),
            profile_name: None,
            permission_profile: None,
            background_session: None,
            project_path: Some("/projects/a/timeline.mlt".to_owned()),
        };
        let on_b = ThreadRecord {
            thread_id: "on-b".to_owned(),
            display_name: "On B".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-b".to_owned(),
            profile_name: None,
            permission_profile: None,
            background_session: None,
            project_path: Some("/projects/b/timeline.mlt".to_owned()),
        };
        let unscoped = ThreadRecord {
            thread_id: "unscoped".to_owned(),
            display_name: "Unscoped".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-u".to_owned(),
            profile_name: None,
            permission_profile: None,
            background_session: None,
            project_path: None,
        };
        store.save_thread_record(&on_a).unwrap();
        store.save_thread_record(&on_b).unwrap();
        store.save_thread_record(&unscoped).unwrap();

        store
            .rename_project_path(
                "/projects/a/timeline.mlt",
                "/projects/a-renamed/timeline.mlt",
            )
            .unwrap();

        let records = store.thread_records().unwrap();
        let by_id = |id: &str| records.iter().find(|r| r.thread_id == id).unwrap();
        assert_eq!(
            by_id("on-a").project_path.as_deref(),
            Some("/projects/a-renamed/timeline.mlt"),
            "the renamed project's thread must follow it"
        );
        assert_eq!(
            by_id("on-b").project_path.as_deref(),
            Some("/projects/b/timeline.mlt"),
            "an unrelated project's thread must never move"
        );
        assert_eq!(
            by_id("unscoped").project_path,
            None,
            "an unscoped thread must stay unscoped"
        );
    }

    /// PISO-7: an empty `old` must never match every legacy/unscoped row
    /// -- the caller-side contract (`update_host`'s `ProjectPathRenamed`
    /// handler must never issue the rename effect for an empty old path
    /// at all) is backed here by proving the SQL itself can't retro-bind
    /// unscoped rows even if that guard were somehow bypassed.
    #[test]
    fn rename_project_path_with_an_empty_old_path_touches_no_row() {
        let store = PanelStateStore::in_memory().unwrap();
        let unscoped = ThreadRecord {
            thread_id: "unscoped".to_owned(),
            display_name: "Unscoped".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-u".to_owned(),
            profile_name: None,
            permission_profile: None,
            background_session: None,
            project_path: None,
        };
        store.save_thread_record(&unscoped).unwrap();

        store
            .rename_project_path("", "/projects/untitled-saved-as.mlt")
            .unwrap();

        assert_eq!(
            store.thread_records().unwrap()[0].project_path,
            None,
            "an unscoped thread must never be retro-bound via an empty old path"
        );
    }

    #[test]
    fn migrates_existing_v1_database_without_losing_thread_settings() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE panel_defaults (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    profile_name TEXT,
                permission_profile TEXT,
                    background_session INTEGER NOT NULL CHECK (background_session IN (0, 1))
                );
                CREATE TABLE thread_settings (
                    thread_id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT,
                    profile_name TEXT,
                    permission_profile TEXT,
                    background_session INTEGER CHECK (background_session IN (0, 1))
                );
                INSERT INTO thread_settings
                    (thread_id, session_id, profile_name, permission_profile, background_session)
                VALUES ('legacy-thread', 'legacy-session', 'codex', 'review', 1);
                PRAGMA user_version = 1;
                ",
            )
            .unwrap();

        let store = PanelStateStore::from_connection(connection).unwrap();
        assert_eq!(
            store.thread_settings("legacy-thread").unwrap(),
            Some(ThreadSettings {
                thread_id: "legacy-thread".to_owned(),
                session_id: Some("legacy-session".to_owned()),
                profile_name: Some("codex".to_owned()),
                permission_profile: Some("review".to_owned()),
                background_session: Some(true),
            })
        );
        assert!(store.thread_records().unwrap().is_empty());
        assert_eq!(store.defaults().unwrap().selected_thread_id, None);
    }

    /// PISO-3: a database created by a build before the `project_path`
    /// column existed (v3 shape -- `display_name`/`provider` present,
    /// `project_path` absent) must open, migrate in place via
    /// `add_column_if_missing`, and keep every existing row -- not error,
    /// and not silently wipe the thread the user already had open.
    #[test]
    fn an_old_v3_database_without_project_path_migrates_and_keeps_its_row() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE panel_defaults (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    profile_name TEXT,
                    permission_profile TEXT,
                    background_session INTEGER NOT NULL CHECK (background_session IN (0, 1)),
                    selected_thread_id TEXT
                );
                CREATE TABLE thread_settings (
                    thread_id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT,
                    profile_name TEXT,
                    permission_profile TEXT,
                    background_session INTEGER CHECK (background_session IN (0, 1)),
                    display_name TEXT,
                    provider TEXT
                );
                INSERT INTO thread_settings
                    (thread_id, session_id, profile_name, permission_profile,
                     background_session, display_name, provider)
                VALUES ('pre-migration-thread', 'session-pre', 'codex', 'review',
                        1, 'Pre-migration thread', 'codex');
                PRAGMA user_version = 3;
                ",
            )
            .unwrap();

        let store = PanelStateStore::from_connection(connection).unwrap();

        // The row survives the migration, and -- being from before this
        // column existed -- reads back with `project_path: None`, which is
        // the documented "unscoped, visible everywhere" state, not an
        // error or a dropped row.
        assert_eq!(
            store.thread_records().unwrap(),
            vec![ThreadRecord {
                thread_id: "pre-migration-thread".to_owned(),
                display_name: "Pre-migration thread".to_owned(),
                provider: "codex".to_owned(),
                session_id: "session-pre".to_owned(),
                profile_name: Some("codex".to_owned()),
                permission_profile: Some("review".to_owned()),
                background_session: Some(true),
                project_path: None,
            }]
        );

        // A fresh write on the migrated table exercises the new column end
        // to end, proving the ALTER TABLE actually took (not just that the
        // old row happens to still be readable).
        store
            .save_thread_record(&ThreadRecord {
                thread_id: "pre-migration-thread".to_owned(),
                display_name: "Pre-migration thread".to_owned(),
                provider: "codex".to_owned(),
                session_id: "session-pre".to_owned(),
                profile_name: Some("codex".to_owned()),
                permission_profile: Some("review".to_owned()),
                background_session: Some(true),
                project_path: Some("/projects/pre-migration.mlt".to_owned()),
            })
            .unwrap();
        assert_eq!(
            store
                .thread_records()
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .project_path
                .as_deref(),
            Some("/projects/pre-migration.mlt")
        );
    }
}
