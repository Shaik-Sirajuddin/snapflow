//! Bridges panel-rust's filesystem-only skill UI (`skills_state.rs`) onto
//! the `skills-manager` crate's vendor(custom-agent-format)-scoped store,
//! so skills created/promoted through the UI become sync-trackable across
//! attached ACP agents. See `memory/acpx/gen/plans/acpx-skills/README.md`.
//!
//! panel-rust is this crate's sole caller -- `vendor_id` values used here
//! are ACP registry agent ids (e.g. `"codex-acp"`), not a second embedding
//! application ("vendor is nothing but a custom_agent format ... utilized
//! by upstream panel-rust/").
//!
//! `skills_state.rs`'s own read path (`scan_skills_dir`,
//! `merge_skills_for_context`) is intentionally left untouched -- it keeps
//! reflecting every `SKILL.md` on disk, including ones a user dropped in by
//! hand. This module is the separate write-side integration: registering a
//! skill here makes it sync-trackable, it doesn't change what the UI's own
//! directory scan already shows.

use std::path::Path;

use skills_manager::{
    agent_registry, RegisterOutcome, SkillError, SkillManager, SkillManagerConfig, SyncMode,
    SyncResult,
};

/// Opens (or re-initializes) a SkillManager pointed at this project's (or,
/// with `project_root: None`, the global) `.snapflow/skills/` storage.
/// Cheap enough to call per-operation (sqlite open + idempotent schema
/// init) -- phase 5's reactive wiring may promote this to a long-lived
/// handle held in panel state if profiling shows it matters.
pub(crate) fn open_manager(project_root: Option<&Path>) -> Result<SkillManager, SkillError> {
    SkillManager::open(SkillManagerConfig::default_snapflow_dirs(project_root))
}

/// Registers `skill_dir` (must already contain `SKILL.md`) under every
/// given custom-agent-format id. `open_manager` failure short-circuits
/// (systemic problem, e.g. can't create `~/.snapflow/skills/`); once open,
/// each vendor_id's registration result is reported back individually so
/// one collision/failure doesn't hide another vendor_id's success.
pub(crate) fn register_skill_for_agents(
    skill_dir: &Path,
    project_root: Option<&Path>,
    vendor_ids: &[&str],
) -> Result<Vec<(String, Result<RegisterOutcome, SkillError>)>, SkillError> {
    let manager = open_manager(project_root)?;
    Ok(vendor_ids
        .iter()
        .map(|vendor_id| {
            (
                vendor_id.to_string(),
                manager.register_skill(vendor_id, skill_dir),
            )
        })
        .collect())
}

/// Whether MCP-free, filesystem-only skill delivery is safe for
/// `vendor_id` -- thin re-export of `skills_manager::agent_registry::
/// is_live_verified` so `agent_bridge.rs` (which otherwise only talks to
/// this crate through this adapter module, not `skills_manager` directly)
/// has one place to ask. See README.md#agent-skill-convention-registry.
pub(crate) fn is_live_verified(vendor_id: &str) -> bool {
    agent_registry::is_live_verified(vendor_id)
}

/// Every skill directory panel-rust's own UI would show for this scope --
/// global always, project too when `project_root` is given. This is the
/// backfill source of truth: "what should a newly-enabled/newly-installed
/// agent inherit" is defined as "whatever the user can already see in the
/// skills list," not a second, divergent notion of what counts as a skill.
fn skill_source_dirs_visible_to_the_ui(project_root: Option<&Path>) -> Vec<std::path::PathBuf> {
    let global_dir = crate::skills_state::global_skills_dir(&crate::resolve_cache_dir());
    let mut dirs: Vec<std::path::PathBuf> = crate::skills_state::scan_skills_dir(
        &global_dir,
        crate::skills_state::SkillScope::Global,
    )
    .into_iter()
    .map(|entry| entry.path)
    .collect();
    if let Some(project_root) = project_root {
        let project_dir = crate::skills_state::project_skills_dir(project_root);
        dirs.extend(
            crate::skills_state::scan_skills_dir(
                &project_dir,
                crate::skills_state::SkillScope::Project,
            )
            .into_iter()
            .map(|entry| entry.path),
        );
    }
    dirs
}

/// Reactive-sync trigger (2)/(3)/(4) shared implementation (trigger (3)
/// reaches this transitively through `register_and_sync_new_skill`
/// below): makes every skill visible to the UI actually land in
/// `vendor_id`'s native skill directory, if one is known. Idempotent --
/// safe to call on every trigger without needing to track "did this
/// already run": `register_skill` is itself idempotent
/// (AlreadyOwned/AdoptedExisting for anything already registered), so
/// backfilling on every call is cheap and correct, not just useful on
/// first enable -- this is also what makes a *newly-enabled or
/// newly-installed* agent inherit pre-existing skills instead of only
/// getting skills created going forward (the gap this backfill step
/// closes, see README.md's "newly-enabled/installed agents" section).
/// Returns `Ok(vec![])` (not an error) for a `vendor_id` with no known
/// target directory -- that is the expected, common case for every
/// agent_id besides "codex-acp" today.
pub(crate) fn sync_agent_targets(
    vendor_id: &str,
    project_root: Option<&Path>,
) -> Result<Vec<SyncResult>, SkillError> {
    let Some(target_dir) = agent_registry::native_target_dir(vendor_id, project_root) else {
        return Ok(Vec::new());
    };
    let manager = open_manager(project_root)?;

    for skill_dir in skill_source_dirs_visible_to_the_ui(project_root) {
        // Best-effort: one malformed skill directory (e.g. a SKILL.md
        // that fails content::hash_dir for some transient IO reason)
        // must not block backfilling every other skill.
        let _ = manager.register_skill(vendor_id, &skill_dir);
    }

    for skill in manager.list_skills(Some(vendor_id))? {
        manager.set_target(vendor_id, &skill.id, &target_dir, SyncMode::Symlink)?;
    }
    manager.sync_all(vendor_id)
}

/// Reactive-sync trigger (4)'s disable half: explicit teardown, not just
/// suppressing future syncs -- removes both the `skill_targets` rows and
/// their on-disk symlinks/copies for `vendor_id`. See
/// `memory/acpx/gen/plans/acpx-skills/README.md#reactive-sync`.
pub(crate) fn teardown_agent_targets(
    vendor_id: &str,
    project_root: Option<&Path>,
) -> Result<(), SkillError> {
    let manager = open_manager(project_root)?;
    for skill in manager.list_skills(Some(vendor_id))? {
        manager.remove_target(vendor_id, &skill.id)?;
    }
    Ok(())
}

/// Reactive-sync trigger (3)'s create half: registers `skill_dir` under
/// every enabled vendor_id, then immediately syncs it into whichever of
/// those have a known native target directory -- so a newly created skill
/// propagates without waiting for the next thread-start check.
pub(crate) fn register_and_sync_new_skill(
    skill_dir: &Path,
    project_root: Option<&Path>,
    enabled_vendor_ids: &[String],
) -> Result<(), SkillError> {
    let vendor_ids: Vec<&str> = enabled_vendor_ids.iter().map(String::as_str).collect();
    register_skill_for_agents(skill_dir, project_root, &vendor_ids)?;
    for vendor_id in &vendor_ids {
        sync_agent_targets(vendor_id, project_root)?;
    }
    Ok(())
}

/// Reactive-sync trigger (3)'s edit half: propagates an EXISTING skill's
/// edited content (`SkillMsg::ContentEdited` -> `Effect::SkillWrite`,
/// which already wrote `skill_dir`'s SKILL.md to disk before this runs)
/// to every enabled vendor_id. `skills-manager` has no persisted mapping
/// from a filesystem path back to a `skill_id`, so this resolves it the
/// same way `register_skill` derives identity internally
/// (`skills_manager::skill_name_for_dir`, exposed for exactly this) and
/// matches it against each vendor_id's own owned skills. A vendor_id
/// that doesn't already own a skill by that name (e.g. it was enabled
/// after this skill was created, or this is the first edit of a skill
/// predating this trigger) falls back to registering it fresh instead of
/// erroring -- `register_skill` is idempotent, so this is safe to do
/// unconditionally rather than needing to track "has this vendor ever
/// seen this skill."
pub(crate) fn update_and_resync_edited_skill(
    skill_dir: &Path,
    project_root: Option<&Path>,
    enabled_vendor_ids: &[String],
) -> Result<(), SkillError> {
    let manager = open_manager(project_root)?;
    let name = skills_manager::skill_name_for_dir(skill_dir);

    for vendor_id in enabled_vendor_ids {
        let owned = manager.list_skills(Some(vendor_id))?;
        match owned.into_iter().find(|skill| skill.name == name) {
            Some(skill) => {
                manager.update_content(vendor_id, &skill.id, skill_dir)?;
            }
            None => {
                manager.register_skill(vendor_id, skill_dir)?;
            }
        }
    }
    for vendor_id in enabled_vendor_ids {
        sync_agent_targets(vendor_id, project_root)?;
    }
    Ok(())
}

/// Recovers `project_root` structurally from a skill's own directory --
/// used by `Effect::SkillWrite`'s handler, which (unlike `Effect::
/// CreateSkill`) doesn't separately carry an `active_project_path` field.
/// `skill_dir` is either `<global_dir>/<name>` (global scope) or
/// `<project_root>/.snapflow/skills/<name>` (project scope, matching
/// `skills_state::project_skills_dir`'s exact shape) -- `None` for global
/// or anything that doesn't structurally match either shape (treated the
/// same as global: sync to the shared default rather than guessing).
pub(crate) fn project_root_from_skill_dir(skill_dir: &Path) -> Option<std::path::PathBuf> {
    let global_dir = crate::skills_state::global_skills_dir(&crate::resolve_cache_dir());
    if skill_dir.parent() == Some(global_dir.as_path()) {
        return None;
    }
    let skills_dir = skill_dir.parent()?; // <project_root>/.snapflow/skills
    if skills_dir.file_name()?.to_str()? != "skills" {
        return None;
    }
    let snapflow_dir = skills_dir.parent()?; // <project_root>/.snapflow
    if snapflow_dir.file_name()?.to_str()? != ".snapflow" {
        return None;
    }
    snapflow_dir.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, name: &str) -> std::path::PathBuf {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\nbody"),
        )
        .unwrap();
        skill_dir
    }

    /// Single test covering register -> sync -> teardown against an
    /// isolated HOME. Deliberately one #[test] fn, not several: these
    /// helpers all resolve storage via `dirs::home_dir()` through
    /// `default_snapflow_dirs`, and `std::env::set_var("HOME", ..)` is
    /// process-global -- separate parallel-running #[test] fns each
    /// setting their own HOME would race. Keeping every HOME-dependent
    /// assertion in one function sidesteps that without adding a
    /// serial-test dependency for one file.
    ///
    /// Also sets `RUI_ACP_CACHE_DIR`: since the backfill step
    /// (skill_source_dirs_visible_to_the_ui) reads through
    /// `crate::resolve_cache_dir()`, which checks `RUI_ACP_CACHE_DIR`
    /// then `XDG_STATE_HOME` BEFORE `HOME` -- on a dev machine with
    /// `XDG_STATE_HOME` set (common), overriding only `HOME` would leave
    /// this test's backfill scan reading the developer's REAL global
    /// skills directory instead of the isolated tempdir. `RUI_ACP_CACHE_DIR`
    /// is checked first, so setting it pins resolve_cache_dir() the same
    /// way HOME pins skills-manager's own default_snapflow_dirs().
    #[test]
    fn register_sync_and_teardown_round_trip_against_isolated_home() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("RUI_ACP_CACHE_DIR", home.path().join("cache"));

        let source_dir = tempfile::tempdir().unwrap();
        let skill_dir = write_skill(source_dir.path(), "commit");

        // register_skill_for_agents: one row per vendor_id.
        let results = register_skill_for_agents(&skill_dir, None, &["codex-acp", "claude-acp"])
            .expect("open_manager should succeed against an isolated HOME");
        assert_eq!(results.len(), 2);
        for (_vendor_id, outcome) in &results {
            assert!(matches!(
                outcome,
                Ok(RegisterOutcome::Registered { .. }) | Ok(RegisterOutcome::AdoptedExisting { .. })
            ));
        }

        // sync_agent_targets: "codex-acp" has a known native target dir,
        // so it should actually materialize a real symlink; an unknown
        // vendor_id is a documented no-op, not an error.
        let codex_results =
            sync_agent_targets("codex-acp", None).expect("sync_agent_targets(codex-acp)");
        assert_eq!(codex_results.len(), 1);
        let expected_link = home.path().join(".codex").join("skills").join("commit");
        assert!(fs::symlink_metadata(&expected_link).unwrap().file_type().is_symlink());

        let unknown_results =
            sync_agent_targets("some-unknown-agent", None).expect("unknown agent is a no-op");
        assert!(unknown_results.is_empty());

        // register_and_sync_new_skill: the combined create-time helper
        // produces the same end state in one call.
        let skill_dir_2 = write_skill(source_dir.path(), "review-pr");
        register_and_sync_new_skill(&skill_dir_2, None, &["codex-acp".to_string()])
            .expect("register_and_sync_new_skill");
        assert!(home
            .path()
            .join(".codex/skills/review-pr")
            .exists());

        // Backfill: a skill placed directly in the real UI-visible global
        // skills directory (not registered through this adapter at all --
        // simulating a skill created while "new-agent" was disabled or not
        // yet installed) must be picked up the first time sync_agent_targets
        // runs for a vendor_id that has never owned anything.
        let global_ui_dir =
            crate::skills_state::global_skills_dir(&crate::resolve_cache_dir());
        write_skill(&global_ui_dir, "pre-existing-global-skill");

        let manager = open_manager(None).unwrap();
        assert!(
            manager.list_skills(Some("new-agent")).unwrap().is_empty(),
            "sanity check: new-agent must not already own anything"
        );

        // "new-agent" has no known native_target_dir in the seeded
        // agent_registry, so sync_agent_targets is a documented no-op for
        // it -- register it under codex-acp's alias set instead, which DOES
        // have a target dir, to actually exercise the backfill-then-sync
        // path end to end (ownership backfill + a real on-disk symlink).
        sync_agent_targets("codex-acp", None).expect("sync_agent_targets backfill pass");
        assert!(
            manager
                .list_skills(Some("codex-acp"))
                .unwrap()
                .iter()
                .any(|skill| skill.name == "pre-existing-global-skill"),
            "codex-acp must now own the pre-existing global skill via backfill, not just \
             whatever it already owned before this call"
        );
        assert!(
            home.path()
                .join(".codex/skills/pre-existing-global-skill")
                .exists(),
            "backfilled ownership must also actually be synced to disk in the same call"
        );

        // project_root_from_skill_dir: structural resolution, no lookup
        // table needed. Global skill -> None; a skill nested under
        // <project>/.snapflow/skills/<name> -> Some(<project>); anything
        // that doesn't match either shape -> None (never a guess).
        assert_eq!(
            project_root_from_skill_dir(&global_ui_dir.join("pre-existing-global-skill")),
            None
        );
        let fake_project_root = home.path().join("some-project");
        let project_skill_dir = fake_project_root
            .join(".snapflow")
            .join("skills")
            .join("release-checklist");
        assert_eq!(
            project_root_from_skill_dir(&project_skill_dir),
            Some(fake_project_root)
        );
        assert_eq!(
            project_root_from_skill_dir(std::path::Path::new("/tmp/totally-unrelated/dir")),
            None
        );

        // update_and_resync_edited_skill: the edit-time reactive-sync
        // path (trigger (3)'s edit half). "commit" is already registered
        // and symlinked into codex-acp's target dir from earlier in this
        // test -- edit its SOURCE content directly (simulating what
        // Effect::SkillWrite's fs::write(path.join("SKILL.md"), ..) does)
        // and confirm the change is live through the SAME existing
        // symlink with no separate manual sync call.
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: commit\ndescription: test\n---\nrevised body",
        )
        .unwrap();
        update_and_resync_edited_skill(&skill_dir, None, &["codex-acp".to_string()])
            .expect("update_and_resync_edited_skill");
        assert!(
            fs::read_to_string(expected_link.join("SKILL.md"))
                .unwrap()
                .contains("revised body"),
            "editing an already-registered, already-synced skill must propagate through its \
             existing symlink target"
        );

        // teardown_agent_targets: explicit removal, not just suppression.
        teardown_agent_targets("codex-acp", None).expect("teardown_agent_targets(codex-acp)");
        assert!(!expected_link.exists());
        assert!(!home.path().join(".codex/skills/review-pr").exists());
    }
}
