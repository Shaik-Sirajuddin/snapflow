mod owners_repo;
mod skills_repo;
mod targets_repo;

pub(crate) use owners_repo::OwnersRepo;
pub(crate) use skills_repo::SkillsRepo;
pub(crate) use targets_repo::TargetsRepo;

use rusqlite::Connection;
use std::path::Path;

use crate::error::SkillError;

const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Opens (creating parent dirs + the db file if needed) and initializes the
/// fixed schema. Idempotent: safe to call every time a SkillManager opens,
/// matches acpx-core/panel-rust's own "CREATE TABLE IF NOT EXISTS every
/// open, hand-rolled ALTER TABLE for later columns" convention -- no
/// migration framework.
pub(crate) fn open_and_init(db_path: &Path) -> Result<Connection, SkillError> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SkillError::io(parent, e))?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}
