use sha2::{Digest, Sha256};
use std::path::Path;

use crate::error::SkillError;

/// sha256 over every regular file under `dir` (SKILL.md + any supporting
/// files), in a deterministic (sorted relative-path) order so the same
/// content always hashes the same regardless of directory-walk ordering.
/// This hash is a skill's real identity -- see README.md#schema.
pub(crate) fn hash_dir(dir: &Path) -> Result<String, SkillError> {
    let mut relative_paths: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(dir)
                .ok()
                .map(|rel| rel.to_path_buf())
        })
        .collect();
    relative_paths.sort();

    let mut hasher = Sha256::new();
    for rel in relative_paths {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        let contents = std::fs::read(dir.join(&rel)).map_err(|e| SkillError::io(dir.join(&rel), e))?;
        hasher.update(&contents);
        hasher.update(b"\0");
    }
    Ok(format!("{:x}", hasher.finalize()))
}
