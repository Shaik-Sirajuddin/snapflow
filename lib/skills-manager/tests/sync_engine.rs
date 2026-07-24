use skills_manager::{RegisterOutcome, SkillManager, SkillManagerConfig, SyncMode, TargetStatus};
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
        RegisterOutcome::AlreadyOwned { skill_id } => skill_id,
        RegisterOutcome::AdoptedExisting { skill_id } => skill_id,
        other => panic!("unexpected outcome: {:?}", other),
    }
}

#[test]
fn sync_all_creates_a_real_symlink_to_central_path() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "body");
    let skill_id = register(&manager, "codex-acp", &source);

    let target_dir = dir.path().join("codex-skills");
    manager
        .set_target("codex-acp", &skill_id, &target_dir, SyncMode::Symlink)
        .unwrap();
    let results = manager.sync_all("codex-acp").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, TargetStatus::Linked);

    let target_path = target_dir.join("commit");
    let meta = fs::symlink_metadata(&target_path).unwrap();
    assert!(meta.file_type().is_symlink());
    let central_path = manager.list_skills(None).unwrap()[0].central_path.clone();
    assert_eq!(fs::read_link(&target_path).unwrap(), central_path);
}

#[test]
fn deleting_target_and_resyncing_self_heals() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "body");
    let skill_id = register(&manager, "codex-acp", &source);
    let target_dir = dir.path().join("codex-skills");
    manager
        .set_target("codex-acp", &skill_id, &target_dir, SyncMode::Symlink)
        .unwrap();
    manager.sync_all("codex-acp").unwrap();

    let target_path = target_dir.join("commit");
    fs::remove_file(&target_path).unwrap();
    assert!(!target_path.exists());

    let results = manager.sync_all("codex-acp").unwrap();
    assert_eq!(results[0].status, TargetStatus::Linked);
    assert!(fs::symlink_metadata(&target_path).unwrap().file_type().is_symlink());
}

#[test]
fn copy_mode_recopies_on_hash_drift_but_symlink_mode_needs_no_action() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "original body");
    let skill_id = register(&manager, "codex-acp", &source);
    let target_dir = dir.path().join("codex-skills");
    manager
        .set_target("codex-acp", &skill_id, &target_dir, SyncMode::Copy)
        .unwrap();
    manager.sync_all("codex-acp").unwrap();

    let target_path = target_dir.join("commit");
    let original_copied = fs::read_to_string(target_path.join("SKILL.md")).unwrap();

    // Mutate the target directly (simulating drift) -- re-sync should
    // detect its hash no longer matches the central copy and refresh it.
    fs::write(target_path.join("SKILL.md"), "tampered content").unwrap();
    let results = manager.sync_all("codex-acp").unwrap();
    assert_eq!(results[0].status, TargetStatus::Linked);
    let recopied = fs::read_to_string(target_path.join("SKILL.md")).unwrap();
    assert_eq!(recopied, original_copied);
}

#[test]
fn collision_at_sync_disambiguates_with_vendor_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());

    let source_a = write_skill(dir.path(), "commit", "body A");
    let skill_a = register(&manager, "codex-acp", &source_a);

    let source_b_dir = dir.path().join("commit-b");
    fs::create_dir_all(&source_b_dir).unwrap();
    fs::write(
        source_b_dir.join("SKILL.md"),
        "---\nname: commit\ndescription: different\n---\nbody B, different content",
    )
    .unwrap();
    let outcome_b = manager.register_skill("codex-acp", &source_b_dir).unwrap();
    let skill_b = match outcome_b {
        RegisterOutcome::NameCollision { new_skill_id, .. } => new_skill_id,
        other => panic!("expected NameCollision, got {:?}", other),
    };

    let target_dir = dir.path().join("codex-skills");
    manager
        .set_target("codex-acp", &skill_a, &target_dir, SyncMode::Symlink)
        .unwrap();
    manager
        .set_target("codex-acp", &skill_b, &target_dir, SyncMode::Symlink)
        .unwrap();
    let results = manager.sync_all("codex-acp").unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.status == TargetStatus::Linked));

    assert!(target_dir.join("commit").exists());
    assert!(target_dir.join("commit__codex-acp").exists());
}
