//! Ported from xingkongliang/skills-manager (MIT License,
//! Copyright (c) 2026 Tianliang Zhang), src-tauri/src/core/sync_engine.rs
//! `copy_dir_recursive` -- see README.md#provenance in the plan doc
//! (memory/acpx/gen/plans/acpx-skills/) for what's ported vs. fresh.
//!
//! Used both by `manager::register_skill` (initial copy of a source
//! directory into the central store) and by copy-mode `sync_all` (phase 3).

use std::path::Path;

use crate::error::SkillError;

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), SkillError> {
    std::fs::create_dir_all(dst).map_err(|e| SkillError::io(dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| SkillError::io(src, e))? {
        let entry = entry.map_err(|e| SkillError::io(src, e))?;
        let entry_path = entry.path();
        let dest_path = dst.join(entry.file_name());
        let file_type = entry.file_type().map_err(|e| SkillError::io(&entry_path, e))?;
        if file_type.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&entry_path, &dest_path).map_err(|e| SkillError::io(&entry_path, e))?;
        }
        // Symlinked entries inside a source skill dir are skipped, matching
        // the same defensive posture snapflowd_mcp.rs's list_skill_files
        // already takes (path-traversal / unexpected-symlink protection).
    }
    Ok(())
}
