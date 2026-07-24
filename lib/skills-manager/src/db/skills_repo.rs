use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

use crate::error::SkillError;
use crate::types::SkillRecord;

/// Pure persistence over the `skills` table. No business logic (no
/// collision decisions) lives here -- see README.md#abstraction-layers.
pub(crate) struct SkillsRepo;

impl SkillsRepo {
    pub(crate) fn find_by_name_and_hash(
        conn: &Connection,
        name: &str,
        content_hash: &str,
    ) -> Result<Option<SkillRecord>, SkillError> {
        conn.query_row(
            "SELECT id, name, description, content_hash, central_path, created_at, updated_at
             FROM skills WHERE name = ?1 AND content_hash = ?2",
            params![name, content_hash],
            Self::row_to_record,
        )
        .optional()
        .map_err(SkillError::from)
    }

    /// Any existing skill with this `name`, regardless of content_hash --
    /// used to detect a name collision (same name, different hash).
    pub(crate) fn find_by_name(
        conn: &Connection,
        name: &str,
    ) -> Result<Vec<SkillRecord>, SkillError> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, content_hash, central_path, created_at, updated_at
             FROM skills WHERE name = ?1",
        )?;
        let rows = stmt
            .query_map(params![name], Self::row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn find_by_id(
        conn: &Connection,
        skill_id: &str,
    ) -> Result<Option<SkillRecord>, SkillError> {
        conn.query_row(
            "SELECT id, name, description, content_hash, central_path, created_at, updated_at
             FROM skills WHERE id = ?1",
            params![skill_id],
            Self::row_to_record,
        )
        .optional()
        .map_err(SkillError::from)
    }

    pub(crate) fn list(
        conn: &Connection,
        vendor_id: Option<&str>,
    ) -> Result<Vec<SkillRecord>, SkillError> {
        match vendor_id {
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, name, description, content_hash, central_path, created_at, updated_at
                     FROM skills ORDER BY name",
                )?;
                let rows = stmt
                    .query_map([], Self::row_to_record)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
            Some(vendor_id) => {
                let mut stmt = conn.prepare(
                    "SELECT s.id, s.name, s.description, s.content_hash, s.central_path, s.created_at, s.updated_at
                     FROM skills s
                     JOIN skill_owners o ON o.skill_id = s.id
                     WHERE o.vendor_id = ?1
                     ORDER BY s.name",
                )?;
                let rows = stmt
                    .query_map(params![vendor_id], Self::row_to_record)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
        }
    }

    pub(crate) fn insert(
        conn: &Connection,
        id: &str,
        name: &str,
        description: Option<&str>,
        content_hash: &str,
        central_path: &str,
        now: i64,
    ) -> Result<(), SkillError> {
        conn.execute(
            "INSERT INTO skills (id, name, description, content_hash, central_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, name, description, content_hash, central_path, now],
        )?;
        Ok(())
    }

    pub(crate) fn update_content_hash(
        conn: &Connection,
        skill_id: &str,
        content_hash: &str,
        now: i64,
    ) -> Result<(), SkillError> {
        conn.execute(
            "UPDATE skills SET content_hash = ?2, updated_at = ?3 WHERE id = ?1",
            params![skill_id, content_hash, now],
        )?;
        Ok(())
    }

    pub(crate) fn delete(conn: &Connection, skill_id: &str) -> Result<(), SkillError> {
        conn.execute("DELETE FROM skills WHERE id = ?1", params![skill_id])?;
        Ok(())
    }

    fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<SkillRecord> {
        Ok(SkillRecord {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            content_hash: row.get(3)?,
            central_path: PathBuf::from(row.get::<_, String>(4)?),
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    }
}
