use std::path::Path;

use crate::error::SkillError;

/// Removes whatever is at `target` (symlink, file, or real directory tree)
/// so a fresh link/copy can be created in its place. No-op if nothing is
/// there. Shared by both the unix and Windows link-creation paths.
pub(crate) fn remove_existing(target: &Path) -> Result<(), SkillError> {
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
