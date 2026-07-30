//! Middle / filesystem-projection layer. See README.md#abstraction-layers.
//! Never writes to sqlite directly -- reports results up to `manager.rs`,
//! which persists them via `db/`.

mod copy;
#[cfg(unix)]
mod symlink;
mod symlink_common;
#[cfg(windows)]
mod symlink_windows;

pub(crate) use copy::copy_dir_recursive;

use std::path::Path;

use crate::content;
use crate::error::SkillError;
use crate::types::SyncMode;

/// Makes `target_path` reflect `canonical_skill` per `mode`, self-healing
/// drift (recreate broken/missing symlinks, re-copy on hash change).
/// Idempotent: calling this repeatedly with nothing changed is a no-op.
/// Returns the SyncMode actually used -- on Windows this can silently
/// downgrade Symlink -> Copy if both symlink_dir and the junction fallback
/// fail (see symlink_windows.rs); callers must persist the returned mode
/// back to `skill_targets.mode`, not assume the requested mode stuck.
pub(crate) fn materialize(
    canonical_skill: &Path,
    target_path: &Path,
    mode: SyncMode,
) -> Result<SyncMode, SkillError> {
    match mode {
        SyncMode::Symlink => {
            #[cfg(unix)]
            {
                symlink::create_or_repair_symlink(canonical_skill, target_path)?;
                Ok(SyncMode::Symlink)
            }
            #[cfg(windows)]
            {
                symlink_windows::create_or_repair_link(canonical_skill, target_path)
            }
            #[cfg(not(any(unix, windows)))]
            {
                copy::copy_dir_recursive(canonical_skill, target_path)?;
                Ok(SyncMode::Copy)
            }
        }
        SyncMode::Copy => {
            if needs_copy_refresh(canonical_skill, target_path)? {
                symlink_remove_if_present(target_path)?;
                copy::copy_dir_recursive(canonical_skill, target_path)?;
            }
            Ok(SyncMode::Copy)
        }
    }
}

/// Copy-mode drift check: re-copy if the target is missing, or if its
/// content hash no longer matches the canonical source's. No extra schema
/// column needed for this -- both sides are re-hashed with the same
/// `content::hash_dir` used for a skill's identity.
fn needs_copy_refresh(canonical_skill: &Path, target_path: &Path) -> Result<bool, SkillError> {
    if !target_path.exists() {
        return Ok(true);
    }
    let canonical_hash = content::hash_dir(canonical_skill)?;
    let target_hash = content::hash_dir(target_path)?;
    Ok(canonical_hash != target_hash)
}

fn symlink_remove_if_present(target_path: &Path) -> Result<(), SkillError> {
    #[cfg(unix)]
    {
        symlink::remove_link_or_dir(target_path)
    }
    #[cfg(windows)]
    {
        symlink_common::remove_existing(target_path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        if target_path.exists() {
            std::fs::remove_dir_all(target_path).map_err(|e| SkillError::io(target_path, e))?;
        }
        Ok(())
    }
}

/// Whether `target_path` is currently a live, correctly-pointed symlink to
/// `canonical_skill` -- used by `status()` without touching the filesystem
/// beyond a single lstat/readlink.
#[allow(dead_code)]
pub(crate) fn is_linked_symlink(target_path: &Path, canonical_skill: &Path) -> bool {
    #[cfg(unix)]
    {
        symlink::symlink_points_to(target_path, canonical_skill)
    }
    #[cfg(windows)]
    {
        symlink_windows::symlink_points_to(target_path, canonical_skill)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Removes whatever sync artifact (symlink or copy) is at `target_path`.
/// Used for target teardown (e.g. a disabled agent's skills getting
/// unlinked) -- see README.md's reactive-sync "settings toggle" trigger.
pub(crate) fn remove_target_artifact(target_path: &Path) -> Result<(), SkillError> {
    symlink_remove_if_present(target_path)
}
