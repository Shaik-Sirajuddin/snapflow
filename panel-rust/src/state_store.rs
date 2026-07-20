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
}

#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    #[error("SQLite panel-state error: {0}")]
    Sql(#[from] rusqlite::Error),
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
            ",
        )?;
        Self::add_column_if_missing(&connection, "display_name", "TEXT")?;
        Self::add_column_if_missing(&connection, "provider", "TEXT")?;
        Self::add_defaults_column_if_missing(&connection, "selected_thread_id", "TEXT")?;
        connection.execute_batch("PRAGMA user_version = 3;")?;
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
        let connection = self.connection.lock().expect("panel state mutex poisoned");
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

    pub fn save_defaults(&self, defaults: &PanelDefaults) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().expect("panel state mutex poisoned");
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
        let connection = self.connection.lock().expect("panel state mutex poisoned");
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
        let connection = self.connection.lock().expect("panel state mutex poisoned");
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
        let connection = self.connection.lock().expect("panel state mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT thread_id, display_name, provider, session_id,
                    profile_name, permission_profile, background_session
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
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Persists a thread's local identity once an ACPX session is bound.
    /// Session/profile immutability is enforced by `bind_session`; this method
    /// only adds the panel-specific display name and provider needed on the
    /// next host launch.
    pub fn save_thread_record(&self, record: &ThreadRecord) -> Result<(), StateStoreError> {
        self.bind_session(
            &record.thread_id,
            &record.session_id,
            record.profile_name.as_deref(),
            record.permission_profile.as_deref(),
        )?;
        let connection = self.connection.lock().expect("panel state mutex poisoned");
        connection.execute(
            "UPDATE thread_settings
             SET display_name = ?2, provider = ?3
             WHERE thread_id = ?1",
            params![record.thread_id, record.display_name, record.provider],
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
        let connection = self.connection.lock().expect("panel state mutex poisoned");
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

        let connection = self.connection.lock().expect("panel state mutex poisoned");
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

    pub fn set_background_override(
        &self,
        thread_id: &str,
        background_session: Option<bool>,
    ) -> Result<(), StateStoreError> {
        let connection = self.connection.lock().expect("panel state mutex poisoned");
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let second = ThreadRecord {
            thread_id: "filters".to_owned(),
            display_name: "Refactor filters".to_owned(),
            provider: "claude".to_owned(),
            session_id: "session-2".to_owned(),
            profile_name: None,
            permission_profile: Some("confirm".to_owned()),
            background_session: Some(true),
        };
        store.save_thread_record(&first).unwrap();
        store
            .set_background_override(&second.thread_id, second.background_session)
            .unwrap();
        store.save_thread_record(&second).unwrap();

        assert_eq!(store.thread_records().unwrap(), vec![first, second]);
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
}
