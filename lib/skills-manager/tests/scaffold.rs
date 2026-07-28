use skills_manager::{SkillManager, SkillManagerConfig};

#[test]
fn open_creates_fixed_schema_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let config = SkillManagerConfig::AtPath {
        db_path: dir.path().join("skills.db"),
        central_store_dir: dir.path().join("store"),
    };

    let manager = SkillManager::open(config.clone()).expect("first open");
    assert!(manager.list_skills(None).unwrap().is_empty());
    drop(manager);

    // Reopening an existing db must not error and must not lose data.
    let manager2 = SkillManager::open(config).expect("second open");
    assert!(manager2.list_skills(None).unwrap().is_empty());
}

#[test]
fn default_snapflow_dirs_resolves_project_and_global_scope() {
    let project_root = std::path::Path::new("/tmp/some-project");
    let project_cfg = SkillManagerConfig::default_snapflow_dirs(Some(project_root));
    match project_cfg {
        SkillManagerConfig::AtPath {
            db_path,
            central_store_dir,
        } => {
            assert!(db_path.ends_with(".snapflow/skills/skills.db"));
            assert!(!db_path.starts_with(project_root));
            assert_eq!(
                central_store_dir,
                project_root
                    .join(".snapflow")
                    .join("skills")
                    .join(".manager-store")
            );
            // Central store must be a subdirectory of, not equal to, the
            // project's own human-readable skills directory -- otherwise
            // this crate's opaque-id-keyed copies would show up as
            // spurious entries in a caller's own directory listing.
            assert_ne!(
                central_store_dir,
                project_root.join(".snapflow").join("skills")
            );
        }
        _ => panic!("expected AtPath"),
    }

    let global_cfg = SkillManagerConfig::default_snapflow_dirs(None);
    match global_cfg {
        SkillManagerConfig::AtPath {
            db_path,
            central_store_dir,
        } => {
            assert!(db_path.ends_with(".snapflow/skills/skills.db"));
            assert!(central_store_dir.ends_with(".snapflow/skills/.manager-store"));
        }
        _ => panic!("expected AtPath"),
    }
}
