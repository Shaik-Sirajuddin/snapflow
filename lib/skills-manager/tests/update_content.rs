use skills_manager::{
    RegisterOutcome, SkillError, SkillManager, SkillManagerConfig, SyncMode, TargetStatus,
    UpdateOutcome,
};
use std::fs;

fn manager_at(dir: &std::path::Path) -> SkillManager {
    SkillManager::open(SkillManagerConfig::AtPath {
        db_path: dir.join("skills.db"),
        central_store_dir: dir.join("store"),
    })
    .unwrap()
}

fn write_skill(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test skill\n---\n{body}"),
    )
    .unwrap();
    skill_dir
}

fn register(manager: &SkillManager, vendor_id: &str, source: &std::path::Path) -> String {
    match manager.register_skill(vendor_id, source).unwrap() {
        RegisterOutcome::Registered { skill_id } => skill_id,
        other => panic!("expected Registered, got {other:?}"),
    }
}

#[test]
fn updating_with_genuinely_different_content_is_live_through_an_existing_symlink_with_no_extra_sync() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "original instructions");
    let skill_id = register(&manager, "codex-acp", &source);

    let target_dir = dir.path().join("codex-skills");
    manager
        .set_target("codex-acp", &skill_id, &target_dir, SyncMode::Symlink)
        .unwrap();
    manager.sync_all("codex-acp").unwrap();
    let target_path = target_dir.join("commit");
    assert_eq!(
        fs::read_to_string(target_path.join("SKILL.md")).unwrap(),
        fs::read_to_string(source.join("SKILL.md")).unwrap()
    );

    // Edit the SOURCE (simulating the skill editor) and call update_content
    // -- deliberately no sync_all() call after this.
    fs::write(
        source.join("SKILL.md"),
        "---\nname: commit\ndescription: test skill\n---\nrevised instructions",
    )
    .unwrap();
    let outcome = manager
        .update_content("codex-acp", &skill_id, &source)
        .unwrap();
    let UpdateOutcome::Updated { new_content_hash } = outcome else {
        panic!("expected Updated, got {outcome:?}")
    };
    assert!(!new_content_hash.is_empty());

    // The existing symlink target must already reflect the new content --
    // central_path's bytes changed in place, no extra sync_all needed.
    let content_through_symlink = fs::read_to_string(target_path.join("SKILL.md")).unwrap();
    assert!(content_through_symlink.contains("revised instructions"));
}

#[test]
fn updating_with_unchanged_content_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "same instructions");
    let skill_id = register(&manager, "codex-acp", &source);
    let before = manager.list_skills(None).unwrap()[0].clone();

    let outcome = manager
        .update_content("codex-acp", &skill_id, &source)
        .unwrap();
    assert_eq!(outcome, UpdateOutcome::Unchanged);

    let after = manager.list_skills(None).unwrap()[0].clone();
    assert_eq!(before.content_hash, after.content_hash);
    assert_eq!(before.updated_at, after.updated_at, "no-op must not touch updated_at either");
}

#[test]
fn updating_a_skill_a_vendor_does_not_own_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "body");
    let skill_id = register(&manager, "codex-acp", &source);

    let edited = write_skill(dir.path(), "commit-edited", "different body");
    let result = manager.update_content("claude-acp", &skill_id, &edited);
    assert!(matches!(result, Err(SkillError::OwnerNotFound { .. })));
}

#[test]
fn copy_mode_target_reflects_the_update_only_after_the_next_sync_all() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "v1");
    let skill_id = register(&manager, "codex-acp", &source);

    let target_dir = dir.path().join("codex-skills");
    manager
        .set_target("codex-acp", &skill_id, &target_dir, SyncMode::Copy)
        .unwrap();
    manager.sync_all("codex-acp").unwrap();
    let target_path = target_dir.join("commit");
    let copied_v1 = fs::read_to_string(target_path.join("SKILL.md")).unwrap();

    fs::write(
        source.join("SKILL.md"),
        "---\nname: commit\ndescription: test skill\n---\nv2",
    )
    .unwrap();
    manager.update_content("codex-acp", &skill_id, &source).unwrap();

    // Copy-mode target is stale until the next sync_all -- unlike symlink
    // mode, its bytes are a physically separate copy.
    assert_eq!(
        fs::read_to_string(target_path.join("SKILL.md")).unwrap(),
        copied_v1,
        "copy-mode target must not change before an explicit sync_all"
    );

    let results = manager.sync_all("codex-acp").unwrap();
    assert_eq!(results[0].status, TargetStatus::Linked);
    let copied_v2 = fs::read_to_string(target_path.join("SKILL.md")).unwrap();
    assert!(copied_v2.contains("v2"));
}

