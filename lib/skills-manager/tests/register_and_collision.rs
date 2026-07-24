use skills_manager::{RegisterOutcome, SkillManager, SkillManagerConfig};
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

#[test]
fn same_vendor_registering_identical_dir_twice_is_already_owned() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "do a commit");

    let first = manager.register_skill("codex-acp", &source).unwrap();
    assert!(matches!(first, RegisterOutcome::Registered { .. }));

    let second = manager.register_skill("codex-acp", &source).unwrap();
    assert!(matches!(second, RegisterOutcome::AlreadyOwned { .. }));

    assert_eq!(manager.list_skills(None).unwrap().len(), 1);
}

#[test]
fn two_vendors_registering_identical_content_adopts_existing() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "do a commit");

    let first = manager.register_skill("codex-acp", &source).unwrap();
    let RegisterOutcome::Registered { skill_id: first_id } = first else {
        panic!("expected Registered")
    };

    let second = manager.register_skill("claude-acp", &source).unwrap();
    let RegisterOutcome::AdoptedExisting { skill_id: second_id } = second else {
        panic!("expected AdoptedExisting, got {:?}", second)
    };

    assert_eq!(first_id, second_id);
    assert_eq!(manager.list_skills(None).unwrap().len(), 1);
    assert_eq!(manager.list_skills(Some("codex-acp")).unwrap().len(), 1);
    assert_eq!(manager.list_skills(Some("claude-acp")).unwrap().len(), 1);
}

#[test]
fn two_vendors_same_name_different_content_is_a_name_collision() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source_a = write_skill(dir.path(), "commit", "version A body");
    let source_b = write_skill(dir.path(), "commit-v2", "version B body, different content");
    // Force the same `name` frontmatter field with different bodies/hashes.
    fs::write(
        source_b.join("SKILL.md"),
        "---\nname: commit\ndescription: different version\n---\nversion B body",
    )
    .unwrap();

    let first = manager.register_skill("codex-acp", &source_a).unwrap();
    let RegisterOutcome::Registered { skill_id: existing_id } = first else {
        panic!("expected Registered")
    };

    let second = manager.register_skill("claude-acp", &source_b).unwrap();
    match second {
        RegisterOutcome::NameCollision {
            existing_skill_id,
            new_skill_id,
            name,
        } => {
            assert_eq!(existing_skill_id, existing_id);
            assert_ne!(new_skill_id, existing_id);
            assert_eq!(name, "commit");
        }
        other => panic!("expected NameCollision, got {:?}", other),
    }

    // Neither row was overwritten -- both still present.
    assert_eq!(manager.list_skills(None).unwrap().len(), 2);
}

#[test]
fn remove_owner_gcs_central_copy_only_once_no_owners_remain() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager_at(dir.path());
    let source = write_skill(dir.path(), "commit", "do a commit");

    let RegisterOutcome::Registered { skill_id } =
        manager.register_skill("codex-acp", &source).unwrap()
    else {
        panic!("expected Registered")
    };
    manager.register_skill("claude-acp", &source).unwrap();

    let central_path = manager.list_skills(None).unwrap()[0].central_path.clone();
    assert!(central_path.exists());

    // One owner removed, one remains -- canonical copy must stay.
    manager.remove_owner("codex-acp", &skill_id).unwrap();
    assert!(central_path.exists());
    assert_eq!(manager.list_skills(None).unwrap().len(), 1);

    // Last owner removed -- canonical copy is GC'd.
    manager.remove_owner("claude-acp", &skill_id).unwrap();
    assert!(!central_path.exists());
    assert!(manager.list_skills(None).unwrap().is_empty());
}
