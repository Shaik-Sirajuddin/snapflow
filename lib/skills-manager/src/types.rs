use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkillRecord {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub content_hash: String,
    pub central_path: PathBuf,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Brand new skill, first owner.
    Registered { skill_id: String },
    /// Same vendor_id already owns this exact (name, content_hash) skill. No-op.
    AlreadyOwned { skill_id: String },
    /// A different vendor_id already registered identical content; this
    /// vendor_id was just added as an additional owner. No new skills row.
    AdoptedExisting { skill_id: String },
    /// Same name, different content_hash than an existing skill. Both kept
    /// as separate rows -- caller decides what to do next.
    NameCollision {
        existing_skill_id: String,
        new_skill_id: String,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// source_dir's content differed from what was stored; central_path
    /// was overwritten in place (same skill_id, new content_hash).
    Updated { new_content_hash: String },
    /// source_dir's content_hash matched what was already stored -- no
    /// filesystem or db write happened.
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Symlink,
    Copy,
}

impl SyncMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncMode::Symlink => "symlink",
            SyncMode::Copy => "copy",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "symlink" => Some(SyncMode::Symlink),
            "copy" => Some(SyncMode::Copy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    Linked,
    Drifted,
    Missing,
    Error,
}

impl TargetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetStatus::Linked => "linked",
            TargetStatus::Drifted => "drifted",
            TargetStatus::Missing => "missing",
            TargetStatus::Error => "error",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "linked" => Some(TargetStatus::Linked),
            "drifted" => Some(TargetStatus::Drifted),
            "missing" => Some(TargetStatus::Missing),
            "error" => Some(TargetStatus::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillTargetRecord {
    pub id: String,
    pub skill_id: String,
    pub vendor_id: String,
    pub target_path: PathBuf,
    pub mode: SyncMode,
    pub status: TargetStatus,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub target_id: String,
    pub skill_id: String,
    pub target_path: PathBuf,
    pub status: TargetStatus,
    pub error: Option<String>,
}

pub type SkillTargetStatus = SkillTargetRecord;
