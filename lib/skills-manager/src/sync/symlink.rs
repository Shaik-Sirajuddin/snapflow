//! Ported from xingkongliang/skills-manager (MIT License,
//! Copyright (c) 2026 Tianliang Zhang), src-tauri/src/core/sync_engine.rs
//! (`createSymlink`-equivalent + `symlink_points_to`) -- see
//! README.md#provenance in the plan doc for what's ported vs. fresh.
//! Unix implementation; see symlink_windows.rs for the junction/copy path.

#![cfg(unix)]

use std::path::Path;

use crate::error::SkillError;

/// Creates a symlink at `target` -> `source`, repairing it in place if
/// something else is already there. Idempotent: a target already
/// symlinked to `source` is left untouched.
pub(crate) fn create_or_repair_symlink(source: &Path, target: &Path) -> Result<(), SkillError> {
    if symlink_points_to(target, source) {
        return Ok(());
    }
    super::symlink_common::remove_existing(target)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SkillError::io(parent, e))?;
    }
    std::os::unix::fs::symlink(source, target).map_err(|e| SkillError::io(target, e))
}

pub(crate) fn symlink_points_to(target: &Path, source: &Path) -> bool {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::read_link(target)
            .map(|resolved| resolved == source)
            .unwrap_or(false),
        _ => false,
    }
}

pub(crate) fn remove_link_or_dir(target: &Path) -> Result<(), SkillError> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            std::fs::remove_file(target).map_err(|e| SkillError::io(target, e))
        }
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(target).map_err(|e| SkillError::io(target, e))
        }
        _ => Ok(()),
    }
}
