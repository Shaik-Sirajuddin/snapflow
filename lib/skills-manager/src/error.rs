use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("skill source directory has no SKILL.md: {0}")]
    MissingSkillMd(PathBuf),

    #[error("skill not found: {0}")]
    SkillNotFound(String),

    #[error("skill owner not found: skill {skill_id} / vendor {vendor_id}")]
    OwnerNotFound { skill_id: String, vendor_id: String },
}

impl SkillError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
