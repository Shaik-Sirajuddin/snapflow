//! Per-agent filesystem-skill-discovery knowledge -- see
//! README.md#agent-skill-convention-registry in the plan doc
//! (memory/acpx/gen/plans/acpx-skills/) for the full design rationale.
//!
//! This cannot be auto-detected: ACP's own schema has no capability
//! field for "this agent reads skills from a filesystem convention," and
//! acpx-registry's `agents/list` entries carry no such field either --
//! it's implementation-specific engine behavior, not a negotiable
//! protocol fact. The only real way to know is a live per-agent test
//! (see panel-rust/tests/skills_manager_live_discovery_e2e_test.rs for
//! the one that proved `codex-acp`). This module holds the *results* of
//! that verification, not a detection mechanism.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryStatus {
    /// A live test actually proved this vendor_id discovers skills via
    /// filesystem with no MCP present. The only status that permits MCP
    /// removal for a vendor_id.
    LiveVerified,
    /// A plausible directory convention is known (from the agent's own
    /// docs, dogfooding, or research) but NOT live-tested against a
    /// non-interactive ACP session. MCP delivery must stay in place.
    Unverified,
    /// No known filesystem skill convention for this vendor_id at all --
    /// MCP is not a fallback here, it's the only path there is.
    NoNativeConvention,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentSkillConvention {
    pub vendor_id: &'static str,
    /// Alternate/short-form strings the same agent is known by in a
    /// caller's own transitional provider-id scheme (e.g. panel-rust's
    /// "codex" alongside the real registry id "codex-acp").
    pub aliases: &'static [&'static str],
    pub status: DiscoveryStatus,
    /// Relative to project_root when syncing project-scoped skills.
    pub project_relative_dir: Option<&'static str>,
    /// Relative to $HOME when syncing global skills.
    pub global_relative_dir: Option<&'static str>,
}

/// Seeded from every agent this repo actually knows about
/// (acpx/acpx-registry/registry.fallback.json's three entries), plus a
/// generic fallback for arbitrary future custom-agent-format ids.
const REGISTRY: &[AgentSkillConvention] = &[
    AgentSkillConvention {
        vendor_id: "codex-acp",
        aliases: &["codex"],
        status: DiscoveryStatus::LiveVerified,
        project_relative_dir: Some(".codex/skills"),
        global_relative_dir: Some(".codex/skills"),
    },
    AgentSkillConvention {
        vendor_id: "claude-acp",
        aliases: &["claude"],
        status: DiscoveryStatus::Unverified,
        project_relative_dir: Some(".claude/skills"),
        global_relative_dir: Some(".claude/skills"),
    },
    AgentSkillConvention {
        vendor_id: "gemini",
        aliases: &[],
        status: DiscoveryStatus::Unverified,
        project_relative_dir: None,
        global_relative_dir: None,
    },
];

pub fn lookup(vendor_id: &str) -> Option<&'static AgentSkillConvention> {
    REGISTRY
        .iter()
        .find(|entry| entry.vendor_id == vendor_id || entry.aliases.contains(&vendor_id))
}

/// Resolves the directory `vendor_id` would natively discover skills in,
/// if this registry has one on record. `None` means either the agent is
/// unknown to this registry, or it's known to have no native filesystem
/// convention (`DiscoveryStatus::NoNativeConvention`) -- callers should
/// treat both cases identically: nothing to sync to.
pub fn native_target_dir(vendor_id: &str, project_root: Option<&Path>) -> Option<PathBuf> {
    let entry = lookup(vendor_id)?;
    match project_root {
        Some(root) => entry.project_relative_dir.map(|rel| root.join(rel)),
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            entry.global_relative_dir.map(|rel| home.join(rel))
        }
    }
}

/// Whether MCP-free, filesystem-only skill delivery is safe for
/// `vendor_id` -- true only for `DiscoveryStatus::LiveVerified` entries.
/// An unknown vendor_id (not in this registry at all) is `false`, same
/// as `Unverified`/`NoNativeConvention` -- absence of knowledge is never
/// treated as permission to drop the MCP safety net.
pub fn is_live_verified(vendor_id: &str) -> bool {
    matches!(
        lookup(vendor_id).map(|entry| entry.status),
        Some(DiscoveryStatus::LiveVerified)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_resolves_both_canonical_id_and_alias_to_the_same_entry() {
        let by_canonical = lookup("codex-acp").expect("codex-acp should be seeded");
        let by_alias = lookup("codex").expect("codex alias should resolve");
        assert_eq!(by_canonical.vendor_id, by_alias.vendor_id);
        assert_eq!(by_canonical.vendor_id, "codex-acp");
    }

    #[test]
    fn unknown_vendor_id_resolves_to_nothing() {
        assert!(lookup("some-agent-nobody-has-heard-of").is_none());
    }

    #[test]
    fn native_target_dir_present_for_codex_and_claude_absent_for_gemini() {
        let project_root = Path::new("/tmp/example-project");
        assert_eq!(
            native_target_dir("codex-acp", Some(project_root)),
            Some(project_root.join(".codex/skills"))
        );
        assert_eq!(
            native_target_dir("claude-acp", Some(project_root)),
            Some(project_root.join(".claude/skills"))
        );
        assert_eq!(
            native_target_dir("gemini", Some(project_root)),
            None,
            "gemini has no researched directory convention -- must be None, not a guess"
        );
        assert_eq!(
            native_target_dir("totally-unknown-agent", Some(project_root)),
            None
        );
    }

    #[test]
    fn is_live_verified_is_true_only_for_codex_never_for_claude_or_unknowns() {
        assert!(is_live_verified("codex-acp"));
        assert!(is_live_verified("codex"));
        assert!(
            !is_live_verified("claude-acp"),
            "claude-acp has a directory guess but was never live-tested -- must not be treated as verified"
        );
        assert!(!is_live_verified("claude"));
        assert!(!is_live_verified("gemini"));
        assert!(!is_live_verified("totally-unknown-agent"));
    }
}
