//! Custom-agent-format adapter layer. `sync/` decides *where* a skill
//! needs to land; this layer decides *what shape* it lands in for a given
//! `vendor_id` (== custom-agent-format id). See README.md#abstraction-layers.

mod passthrough;

use std::path::Path;

use crate::error::SkillError;
use crate::types::SyncMode;

pub(crate) trait AgentSkillFormat {
    fn materialize(
        &self,
        canonical_skill: &Path,
        target_path: &Path,
        mode: SyncMode,
    ) -> Result<SyncMode, SkillError>;
}

/// Every `vendor_id` uses `Passthrough` at launch -- this function is the
/// one place a future per-format dispatch (e.g. by matching on `vendor_id`)
/// would be added, without `sync/`'s drift/collision/symlink logic needing
/// to know or care that other formats exist.
pub(crate) fn format_for_vendor(_vendor_id: &str) -> impl AgentSkillFormat {
    passthrough::Passthrough
}
