use rusqlite::{params, Connection, OptionalExtension};

use crate::error::SkillError;

/// Pure persistence over the `skill_owners` join table.
pub(crate) struct OwnersRepo;

impl OwnersRepo {
    pub(crate) fn exists(
        conn: &Connection,
        skill_id: &str,
        vendor_id: &str,
    ) -> Result<bool, SkillError> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM skill_owners WHERE skill_id = ?1 AND vendor_id = ?2",
                params![skill_id, vendor_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub(crate) fn insert(
        conn: &Connection,
        skill_id: &str,
        vendor_id: &str,
        now: i64,
    ) -> Result<(), SkillError> {
        conn.execute(
            "INSERT OR IGNORE INTO skill_owners (skill_id, vendor_id, registered_at) VALUES (?1, ?2, ?3)",
            params![skill_id, vendor_id, now],
        )?;
        Ok(())
    }

    pub(crate) fn remove(
        conn: &Connection,
        skill_id: &str,
        vendor_id: &str,
    ) -> Result<(), SkillError> {
        conn.execute(
            "DELETE FROM skill_owners WHERE skill_id = ?1 AND vendor_id = ?2",
            params![skill_id, vendor_id],
        )?;
        Ok(())
    }

    pub(crate) fn count_owners(conn: &Connection, skill_id: &str) -> Result<i64, SkillError> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM skill_owners WHERE skill_id = ?1",
            params![skill_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}
