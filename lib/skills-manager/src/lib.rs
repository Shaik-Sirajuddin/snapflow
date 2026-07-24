//! Vendor(custom-agent-format)-scoped, SQLite-backed skill manager.
//!
//! `vendor_id` throughout this crate is a custom-agent-format id (e.g.
//! "codex-acp", "claude-acp"), not a second embedding application --
//! panel-rust is this crate's sole caller. See the plan doc at
//! `memory/acpx/gen/plans/acpx-skills/README.md` for full design rationale.
//!
//! Everything below `lib.rs` is `pub(crate)` (except `agent_registry`,
//! which is deliberately public -- see its own module doc) -- callers
//! only see this public surface.

pub mod agent_registry;
mod config;
mod content;
mod db;
mod error;
mod formats;
mod manager;
mod sync;
mod types;

pub use config::SkillManagerConfig;
pub use error::SkillError;
pub use manager::SkillManager;
pub use types::{
    RegisterOutcome, SkillRecord, SkillTargetRecord, SkillTargetStatus, SyncMode, SyncResult,
    TargetStatus, UpdateOutcome,
};

/// The `name` `register_skill` would assign a skill registered from
/// `source_dir` -- exposed so a caller that only has a directory path
/// (not a `skill_id`, e.g. panel-rust's skill-editor save path) can
/// resolve which existing `SkillRecord` (from `list_skills`) it
/// corresponds to, using the exact same identity logic `register_skill`
/// itself uses, rather than reimplementing the frontmatter/basename
/// fallback separately and risking the two disagreeing.
pub fn skill_name_for_dir(source_dir: &std::path::Path) -> String {
    content::resolve_skill_name(source_dir)
}
