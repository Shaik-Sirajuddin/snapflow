//! Canonical project-local storage roots.
//!
//! This module is intentionally the only place that derives a Snapflow store
//! directory from a project identity. Keeping this conversion centralized
//! prevents the ACP cwd, MCP `--project-dir`, and persistence layers from
//! drifting apart.

use crate::model::ProjectIdentity;
use std::path::{Path, PathBuf};

/// Normalize an existing saved path so symlink and relative-path spellings
/// cannot fork one project's store. Nonexistent paths remain unchanged for
/// the host's subsequent save/open flow.
pub fn normalize_mlt_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn project_store_dir(identity: &ProjectIdentity, staging_root: &Path) -> Option<PathBuf> {
    match identity {
        ProjectIdentity::None => None,
        ProjectIdentity::Untitled(id) => Some(staging_root.join(".snapflow-staging").join(id)),
        ProjectIdentity::Saved(mlt_path) => {
            let normalized = normalize_mlt_path(Path::new(mlt_path));
            let path = normalized.as_path();
            let folder = path.parent()?;
            let stem = path.file_stem()?;
            Some(folder.join(".snapflow").join(stem))
        }
    }
}

/// Physical SQLite location for one project's durable panel state.
/// `None` deliberately keeps the legacy/global location so old rows remain
/// readable during migration and genuinely projectless state has somewhere
/// stable to live. Every real identity gets its own database file.
pub fn panel_state_path(identity: &ProjectIdentity, staging_root: &Path) -> PathBuf {
    project_store_dir(identity, staging_root)
        .unwrap_or_else(|| staging_root.to_path_buf())
        .join("panel-state.sqlite3")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_projects_use_parent_folder_and_file_stem() {
        assert_eq!(
            project_store_dir(
                &ProjectIdentity::Saved("/work/cut/project.mlt".into()),
                Path::new("/global"),
            ),
            Some(PathBuf::from("/work/cut/.snapflow/project")),
        );
    }

    #[test]
    fn raw_mlt_path_is_never_the_store_or_acp_cwd() {
        let mlt = "/work/cut/project.mlt";
        let store = project_store_dir(&ProjectIdentity::Saved(mlt.into()), Path::new("/global"))
            .expect("saved identity must resolve a store");
        assert_ne!(store, PathBuf::from(mlt));
        assert!(!store.as_os_str().to_string_lossy().ends_with(".mlt"));
    }

    #[test]
    fn untitled_projects_are_not_global_none() {
        assert_eq!(
            project_store_dir(
                &ProjectIdentity::Untitled("u-1".into()),
                Path::new("/global"),
            ),
            Some(PathBuf::from("/global/.snapflow-staging/u-1")),
        );
        assert_eq!(
            project_store_dir(&ProjectIdentity::None, Path::new("/global")),
            None
        );
    }

    #[test]
    fn each_real_identity_has_a_distinct_physical_state_database() {
        let root = Path::new("/global");
        assert_eq!(
            panel_state_path(&ProjectIdentity::Untitled("u-1".into()), root),
            PathBuf::from("/global/.snapflow-staging/u-1/panel-state.sqlite3")
        );
        assert_ne!(
            panel_state_path(&ProjectIdentity::Untitled("u-1".into()), root),
            panel_state_path(&ProjectIdentity::Untitled("u-2".into()), root)
        );
        assert_ne!(
            panel_state_path(&ProjectIdentity::Saved("/work/a/project.mlt".into()), root),
            panel_state_path(&ProjectIdentity::Saved("/work/b/project.mlt".into()), root)
        );
    }

    #[test]
    fn physical_stores_do_not_share_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let a = crate::state_store::PanelStateStore::open(panel_state_path(
            &ProjectIdentity::Untitled("a".into()),
            temp.path(),
        ))
        .unwrap();
        let b = crate::state_store::PanelStateStore::open(panel_state_path(
            &ProjectIdentity::Untitled("b".into()),
            temp.path(),
        ))
        .unwrap();
        a.save_defaults(&crate::state_store::PanelDefaults {
            profile_name: Some("project-a".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            a.defaults().unwrap().profile_name.as_deref(),
            Some("project-a")
        );
        assert_eq!(b.defaults().unwrap().profile_name, None);
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlinked_mlt_paths_share_the_canonical_store() {
        let temp = tempfile::tempdir().unwrap();
        let real_dir = temp.path().join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("project.mlt");
        std::fs::write(&real, b"<mlt/>").unwrap();
        let link = temp.path().join("project-link.mlt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            project_store_dir(
                &ProjectIdentity::Saved(real.to_string_lossy().into_owned()),
                temp.path(),
            ),
            project_store_dir(
                &ProjectIdentity::Saved(link.to_string_lossy().into_owned()),
                temp.path(),
            )
        );
    }
}
