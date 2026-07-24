use std::path::Path;

use crate::error::SkillError;
use crate::sync::materialize;
use crate::types::SyncMode;

use super::AgentSkillFormat;

/// Default, and currently only, format adapter: symlinks/copies the
/// canonical `SKILL.md` directory to the target verbatim. Every `vendor_id`
/// uses this until a future custom-agent-format actually needs a different
/// on-disk shape -- see README.md#abstraction-layers ("Custom-agent-format
/// layer").
pub(crate) struct Passthrough;

impl AgentSkillFormat for Passthrough {
    fn materialize(
        &self,
        canonical_skill: &Path,
        target_path: &Path,
        mode: SyncMode,
    ) -> Result<SyncMode, SkillError> {
        materialize(canonical_skill, target_path, mode)
    }
}
