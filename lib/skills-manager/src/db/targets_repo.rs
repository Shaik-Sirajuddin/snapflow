use rusqlite::{params, Connection};
use std::path::PathBuf;

use crate::error::SkillError;
use crate::types::{SkillTargetRecord, SyncMode, TargetStatus};

/// Pure persistence over the `skill_targets` table.
pub(crate) struct TargetsRepo;

impl TargetsRepo {
    pub(crate) fn upsert(
        conn: &Connection,
        id: &str,
        skill_id: &str,
        vendor_id: &str,
        target_path: &str,
        mode: SyncMode,
        status: TargetStatus,
        now: i64,
    ) -> Result<(), SkillError> {
        conn.execute(
            "INSERT INTO skill_targets (id, skill_id, vendor_id, target_path, mode, status, last_synced_at, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)
             ON CONFLICT(vendor_id, skill_id, target_path) DO UPDATE SET mode = excluded.mode",
            params![id, skill_id, vendor_id, target_path, mode.as_str(), status.as_str()],
        )?;
        let _ = now;
        Ok(())
    }

    pub(crate) fn list_for_vendor(
        conn: &Connection,
        vendor_id: &str,
    ) -> Result<Vec<SkillTargetRecord>, SkillError> {
        let mut stmt = conn.prepare(
            "SELECT id, skill_id, vendor_id, target_path, mode, status, last_synced_at, last_error
             FROM skill_targets WHERE vendor_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![vendor_id], Self::row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    #[allow(dead_code)]
    pub(crate) fn list_for_skill(
        conn: &Connection,
        skill_id: &str,
    ) -> Result<Vec<SkillTargetRecord>, SkillError> {
        let mut stmt = conn.prepare(
            "SELECT id, skill_id, vendor_id, target_path, mode, status, last_synced_at, last_error
             FROM skill_targets WHERE skill_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![skill_id], Self::row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn update_status(
        conn: &Connection,
        target_id: &str,
        status: TargetStatus,
        last_synced_at: Option<i64>,
        last_error: Option<&str>,
    ) -> Result<(), SkillError> {
        conn.execute(
            "UPDATE skill_targets SET status = ?2, last_synced_at = ?3, last_error = ?4 WHERE id = ?1",
            params![target_id, status.as_str(), last_synced_at, last_error],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn delete(conn: &Connection, target_id: &str) -> Result<(), SkillError> {
        conn.execute("DELETE FROM skill_targets WHERE id = ?1", params![target_id])?;
        Ok(())
    }

    pub(crate) fn delete_for_skill_and_vendor(
        conn: &Connection,
        skill_id: &str,
        vendor_id: &str,
    ) -> Result<(), SkillError> {
        conn.execute(
            "DELETE FROM skill_targets WHERE skill_id = ?1 AND vendor_id = ?2",
            params![skill_id, vendor_id],
        )?;
        Ok(())
    }

    pub(crate) fn update_mode(
        conn: &Connection,
        target_id: &str,
        mode: SyncMode,
    ) -> Result<(), SkillError> {
        conn.execute(
            "UPDATE skill_targets SET mode = ?2 WHERE id = ?1",
            params![target_id, mode.as_str()],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn delete_for_vendor(conn: &Connection, vendor_id: &str) -> Result<(), SkillError> {
        conn.execute(
            "DELETE FROM skill_targets WHERE vendor_id = ?1",
            params![vendor_id],
        )?;
        Ok(())
    }

    fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<SkillTargetRecord> {
        let mode_str: String = row.get(4)?;
        let status_str: String = row.get(5)?;
        Ok(SkillTargetRecord {
            id: row.get(0)?,
            skill_id: row.get(1)?,
            vendor_id: row.get(2)?,
            target_path: PathBuf::from(row.get::<_, String>(3)?),
            mode: SyncMode::from_str(&mode_str).unwrap_or(SyncMode::Symlink),
            status: TargetStatus::from_str(&status_str).unwrap_or(TargetStatus::Missing),
            last_synced_at: row.get(6)?,
            last_error: row.get(7)?,
        })
    }
}
