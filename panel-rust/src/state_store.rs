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
}

impl Default for PanelDefaults {
    fn default() -> Self {
        Self {
            profile_name: None,
            permission_profile: None,
            background_session: false,
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
                background_session INTEGER NOT NULL CHECK (background_session IN (0, 1))
            );
            CREATE TABLE IF NOT EXISTS thread_settings (
                thread_id TEXT PRIMARY KEY NOT NULL,
                session_id TEXT,
                profile_name TEXT,
                permission_profile TEXT,
                background_session INTEGER CHECK (background_session IN (0, 1))
            );
            PRAGMA user_version = 1;
            ",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self, StateStoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    pub fn defaults(&self) -> Result<PanelDefaults, StateStoreError> {
        let connection = self.connection.lock().expect("panel state mutex poisoned");
        connection
            .query_row(
                "SELECT profile_name, permission_profile, background_session
                 FROM panel_defaults WHERE id = 1",
                [],
                |row| {
                    Ok(PanelDefaults {
                        profile_name: row.get(0)?,
                        permission_profile: row.get(1)?,
                        background_session: row.get::<_, i64>(2)? != 0,
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
            "INSERT INTO panel_defaults (id, profile_name, permission_profile, background_session)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 profile_name = excluded.profile_name,
                 permission_profile = excluded.permission_profile,
                 background_session = excluded.background_session",
            params![
                defaults.profile_name,
                defaults.permission_profile,
                i64::from(defaults.background_session),
            ],
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
        };
        store.save_defaults(&defaults).unwrap();
        store
            .set_background_override("thread-a", Some(false))
            .unwrap();

        assert_eq!(store.defaults().unwrap(), defaults);
        assert!(!store.effective_background_session("thread-a").unwrap());
        assert!(store.effective_background_session("thread-b").unwrap());
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
}
