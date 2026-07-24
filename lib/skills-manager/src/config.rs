use std::path::{Path, PathBuf};

/// Where this SkillManager handle's sqlite db and canonical skill copies
/// live. `db_path` should be the SAME file across every vendor_id (custom
/// agent format) that wants cross-format collision detection -- see
/// README.md#schema in the plan doc. `central_store_dir` is where
/// `register_skill` copies new skill directories into for THIS handle.
#[derive(Debug, Clone)]
pub enum SkillManagerConfig {
    Default {
        app_data_dir: PathBuf,
    },
    AtPath {
        db_path: PathBuf,
        central_store_dir: PathBuf,
    },
}

impl SkillManagerConfig {
    pub(crate) fn resolve(&self) -> (PathBuf, PathBuf) {
        match self {
            SkillManagerConfig::Default { app_data_dir } => (
                app_data_dir.join("skills.db"),
                app_data_dir.join("store"),
            ),
            SkillManagerConfig::AtPath {
                db_path,
                central_store_dir,
            } => (db_path.clone(), central_store_dir.clone()),
        }
    }

    /// `.snapflow/` storage convention (see README.md#storage-layout):
    /// - db always at `~/.snapflow/skills/skills.db` -- one shared db
    ///   regardless of project, so cross-vendor_id collision detection
    ///   works by default.
    /// - central store lives in a `.manager-store` subdirectory *inside*
    ///   the project's or the global `.snapflow/skills/` directory --
    ///   deliberately NOT that directory itself. A caller (panel-rust)
    ///   typically also uses that same parent directory as its own
    ///   human-readable, name-keyed skill directory (what its UI scans
    ///   and displays); this crate's canonical copies are keyed by
    ///   opaque skill id, not name, so nesting them in a dot-prefixed
    ///   subdirectory keeps the two from colliding or from the crate's
    ///   internal copies showing up as spurious entries in a caller's own
    ///   directory listing.
    ///
    /// `project_root` is caller-supplied -- this crate never guesses a
    /// project root itself, matching `skills_state.rs`'s existing
    /// "caller passes directories in" discipline.
    pub fn default_snapflow_dirs(project_root: Option<&Path>) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let global_snapflow_skills = home.join(".snapflow").join("skills");
        let db_path = global_snapflow_skills.join("skills.db");
        let central_store_dir = match project_root {
            Some(root) => root
                .join(".snapflow")
                .join("skills")
                .join(".manager-store"),
            None => global_snapflow_skills.join(".manager-store"),
        };
        SkillManagerConfig::AtPath {
            db_path,
            central_store_dir,
        }
    }
}
