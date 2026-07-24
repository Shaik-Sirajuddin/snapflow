//! Ported from xingkongliang/skills-manager (MIT License,
//! Copyright (c) 2026 Tianliang Zhang), src-tauri/src/core/sync_engine.rs's
//! Windows branch (`symlink_dir` -> junction -> copy fallback chain) -- see
//! README.md#provenance in the plan doc for what's ported vs. fresh.
//!
//! NOTE: written against documented `std`/`junction` crate APIs but not
//! compiled/run on Windows in this session (dev box is Linux) -- flag for
//! extra scrutiny in the rust_audit_review phase and verify with real
//! Windows CI before relying on it.

#![cfg(windows)]

use std::path::Path;

use crate::error::SkillError;
use crate::types::SyncMode;

/// Tries `std::os::windows::fs::symlink_dir` (needs Developer Mode or admin
/// on most Windows configs), falls back to a directory junction (no special
/// privileges needed), falls back to a full copy if both fail. Returns the
/// SyncMode that was ACTUALLY used, which may differ from what was
/// requested -- the caller must persist this back to `skill_targets.mode`.
pub(crate) fn create_or_repair_link(source: &Path, target: &Path) -> Result<SyncMode, SkillError> {
    super::symlink_common::remove_existing(target)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SkillError::io(parent, e))?;
    }

    if std::os::windows::fs::symlink_dir(source, target).is_ok() {
        return Ok(SyncMode::Symlink);
    }

    // std treats a directory junction as a symlink for our purposes
    // (`is_symlink()` / `read_link()` both work on it) -- reported back as
    // SyncMode::Symlink, matching upstream's documented behavior.
    if junction::create(source, target).is_ok() {
        return Ok(SyncMode::Symlink);
    }

    super::copy::copy_dir_recursive(source, target)?;
    Ok(SyncMode::Copy)
}

pub(crate) fn symlink_points_to(target: &Path, source: &Path) -> bool {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::read_link(target)
            .map(|resolved| resolved == source)
            .unwrap_or(false),
        _ => false,
    }
}
