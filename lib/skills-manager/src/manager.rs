use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::config::SkillManagerConfig;
use crate::content;
use crate::db::{self, OwnersRepo, SkillsRepo, TargetsRepo};
use crate::error::SkillError;
use crate::formats::{self, AgentSkillFormat};
use crate::sync;
use crate::types::{
    RegisterOutcome, SkillRecord, SkillTargetRecord, SyncMode, SyncResult, TargetStatus,
    UpdateOutcome,
};

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
}

/// Internal outcome of `update_content`'s transaction: whether a real
/// content swap is needed, decided while the db lock is held, but the
/// actual filesystem swap happens after the lock is released (see
/// `swap_central_path`).
enum UpdateSwap {
    Swap {
        old_central_path: std::path::PathBuf,
    },
    Unchanged,
}

fn content_copy_for_staging(source_dir: &Path, staging_path: &Path) -> Result<(), SkillError> {
    sync::copy_dir_recursive(source_dir, staging_path)
}

/// Replaces `central_path`'s contents with `staging_path`'s, keeping a
/// valid directory present at `central_path` for as much of the
/// operation as possible: renames the old content aside (to a discard
/// path) before renaming the new content into place, rather than
/// deleting first -- a concurrent reader (e.g. `sync_all` re-hashing for
/// copy-mode drift) sees either the old or the new content at every
/// point, never a missing directory. The discard is removed last,
/// best-effort.
fn swap_central_path(central_path: &Path, staging_path: &Path) -> Result<(), SkillError> {
    let discard_path = central_path.with_file_name(format!(
        "{}.discard-{}",
        central_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        uuid::Uuid::new_v4()
    ));
    std::fs::rename(central_path, &discard_path).map_err(|e| SkillError::io(central_path, e))?;
    if let Err(second_rename_err) = std::fs::rename(staging_path, central_path) {
        // Roll back: central_path must never be left missing just
        // because the update failed -- every existing symlink target
        // still points at it. Best-effort; if even the rollback fails
        // (e.g. the filesystem itself is now unwritable) there is
        // nothing further to do, so surface the ORIGINAL error, not the
        // rollback's, since that's the one the caller can actually act on.
        let _ = std::fs::rename(&discard_path, central_path);
        return Err(SkillError::io(staging_path, second_rename_err));
    }
    let _ = std::fs::remove_dir_all(&discard_path);
    Ok(())
}

/// Single mutex-guarded connection: sqlite only allows one writer anyway,
/// so this beats a pool -- same reasoning acpx-core's PersistenceStore
/// documents for its own connection handling.
pub struct SkillManager {
    conn: Arc<Mutex<Connection>>,
    central_store_dir: std::path::PathBuf,
}

impl SkillManager {
    pub fn open(config: SkillManagerConfig) -> Result<Self, SkillError> {
        let (db_path, central_store_dir) = config.resolve();
        std::fs::create_dir_all(&central_store_dir)
            .map_err(|e| SkillError::io(&central_store_dir, e))?;
        let conn = db::open_and_init(&db_path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            central_store_dir,
        })
    }

    pub fn list_skills(&self, vendor_id: Option<&str>) -> Result<Vec<SkillRecord>, SkillError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        SkillsRepo::list(&conn, vendor_id)
    }

    /// Registers `source_dir` (must contain SKILL.md or skill.md) under
    /// `vendor_id` (a custom-agent-format id). Identity = (name,
    /// content_hash) -- see README.md#collision-handling for the full
    /// decision table this implements.
    pub fn register_skill(
        &self,
        vendor_id: &str,
        source_dir: &Path,
    ) -> Result<RegisterOutcome, SkillError> {
        if !source_dir.join("SKILL.md").is_file() && !source_dir.join("skill.md").is_file() {
            return Err(SkillError::MissingSkillMd(source_dir.to_path_buf()));
        }
        let meta = content::parse_skill_md(source_dir);
        let name = meta.name.clone().unwrap_or_else(|| {
            source_dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let content_hash = content::hash_dir(source_dir)?;
        let now = now_secs();

        // Fast path: a plain read, its own short-lived lock -- if this
        // exact (name, content_hash) is already known, no filesystem copy
        // is needed at all, so the transaction below never has to touch
        // disk.
        if let Some(existing) =
            self.with_conn(|conn| SkillsRepo::find_by_name_and_hash(conn, &name, &content_hash))?
        {
            return self.with_transaction(|tx| {
                if OwnersRepo::exists(tx, &existing.id, vendor_id)? {
                    return Ok(RegisterOutcome::AlreadyOwned {
                        skill_id: existing.id.clone(),
                    });
                }
                OwnersRepo::insert(tx, &existing.id, vendor_id, now)?;
                Ok(RegisterOutcome::AdoptedExisting {
                    skill_id: existing.id.clone(),
                })
            });
        }

        // New content: copy OUTSIDE any db lock -- rust-audit review
        // flagged the original version of this function doing the copy
        // *inside* with_transaction, holding the mutex across a blocking
        // filesystem walk+copy and starving every other concurrent
        // SkillManager caller (panel-rust's reactive-sync triggers call
        // this from several independently spawned background threads) for
        // however long that copy took.
        let new_id = uuid::Uuid::new_v4().to_string();
        let central_path = self.central_store_dir.join(&new_id);
        sync::copy_dir_recursive(source_dir, &central_path)?;

        let outcome = self.with_transaction(|tx| {
            // Re-check inside the transaction: another thread may have
            // registered this exact (name, content_hash) concurrently
            // between the fast-path check above and now -- the fast path
            // is an optimization, not the sole correctness guarantee.
            if let Some(existing) = SkillsRepo::find_by_name_and_hash(tx, &name, &content_hash)? {
                if OwnersRepo::exists(tx, &existing.id, vendor_id)? {
                    return Ok(RegisterOutcome::AlreadyOwned {
                        skill_id: existing.id,
                    });
                }
                OwnersRepo::insert(tx, &existing.id, vendor_id, now)?;
                return Ok(RegisterOutcome::AdoptedExisting {
                    skill_id: existing.id,
                });
            }

            let same_name = SkillsRepo::find_by_name(tx, &name)?;
            SkillsRepo::insert(
                tx,
                &new_id,
                &name,
                meta.description.as_deref(),
                &content_hash,
                &central_path.to_string_lossy(),
                now,
            )?;
            OwnersRepo::insert(tx, &new_id, vendor_id, now)?;

            match same_name.into_iter().next() {
                Some(existing) => Ok(RegisterOutcome::NameCollision {
                    existing_skill_id: existing.id,
                    new_skill_id: new_id.clone(),
                    name: name.clone(),
                }),
                None => Ok(RegisterOutcome::Registered {
                    skill_id: new_id.clone(),
                }),
            }
        })?;

        // The re-check above won the race in our favor (AlreadyOwned /
        // AdoptedExisting) -- the copy we made is now orphaned, clean it
        // up rather than leaking it.
        if matches!(
            outcome,
            RegisterOutcome::AlreadyOwned { .. } | RegisterOutcome::AdoptedExisting { .. }
        ) {
            let _ = std::fs::remove_dir_all(&central_path);
        }

        Ok(outcome)
    }

    /// Removes `vendor_id`'s ownership of `skill_id`. Once a skill has zero
    /// remaining owners, its canonical copy is garbage-collected from disk.
    pub fn remove_owner(&self, vendor_id: &str, skill_id: &str) -> Result<(), SkillError> {
        let gc_path = self.with_transaction(|tx| {
            if !OwnersRepo::exists(tx, skill_id, vendor_id)? {
                return Err(SkillError::OwnerNotFound {
                    skill_id: skill_id.to_string(),
                    vendor_id: vendor_id.to_string(),
                });
            }
            OwnersRepo::remove(tx, skill_id, vendor_id)?;
            let remaining = OwnersRepo::count_owners(tx, skill_id)?;
            if remaining == 0 {
                let skill = SkillsRepo::find_by_id(tx, skill_id)?
                    .ok_or_else(|| SkillError::SkillNotFound(skill_id.to_string()))?;
                SkillsRepo::delete(tx, skill_id)?;
                Ok(Some(skill.central_path))
            } else {
                Ok(None)
            }
        })?;

        if let Some(path) = gc_path {
            if path.exists() {
                std::fs::remove_dir_all(&path).map_err(|e| SkillError::io(&path, e))?;
            }
        }
        Ok(())
    }

    /// Overwrites `skill_id`'s canonical content in place with
    /// `source_dir`'s current contents -- for editing a skill `vendor_id`
    /// already owns, distinct from `register_skill` (whose `(name,
    /// content_hash)` identity is for *new* registrations, not mutating
    /// an existing row -- see README.md#editing-an-existing-skills-content-
    /// update_content). Symlink-mode targets need no further action: they
    /// point AT `central_path`, so the new content is live the moment
    /// this returns. Copy-mode targets go stale until their next
    /// `sync_all` call (existing drift detection already handles that).
    pub fn update_content(
        &self,
        vendor_id: &str,
        skill_id: &str,
        source_dir: &Path,
    ) -> Result<UpdateOutcome, SkillError> {
        if !self.with_conn(|conn| OwnersRepo::exists(conn, skill_id, vendor_id))? {
            return Err(SkillError::OwnerNotFound {
                skill_id: skill_id.to_string(),
                vendor_id: vendor_id.to_string(),
            });
        }
        let new_hash = content::hash_dir(source_dir)?;

        // Stage the new content OUTSIDE any db lock (same rust-audit
        // lesson as register_skill: a blocking filesystem copy must never
        // happen while the mutex is held).
        let staging_path = self
            .central_store_dir
            .join(format!("{skill_id}.staging-{}", uuid::Uuid::new_v4()));
        content_copy_for_staging(source_dir, &staging_path)?;

        let outcome = self.with_transaction(|tx| {
            // Re-verify ownership and re-check the hash inside the
            // transaction: a concurrent remove_owner or update_content
            // call could have changed either since the checks above.
            if !OwnersRepo::exists(tx, skill_id, vendor_id)? {
                return Err(SkillError::OwnerNotFound {
                    skill_id: skill_id.to_string(),
                    vendor_id: vendor_id.to_string(),
                });
            }
            let current = SkillsRepo::find_by_id(tx, skill_id)?
                .ok_or_else(|| SkillError::SkillNotFound(skill_id.to_string()))?;
            if current.content_hash == new_hash {
                return Ok(UpdateSwap::Unchanged);
            }
            SkillsRepo::update_content_hash(tx, skill_id, &new_hash, now_secs())?;
            Ok(UpdateSwap::Swap {
                old_central_path: current.central_path,
            })
        });

        match outcome {
            Ok(UpdateSwap::Swap { old_central_path }) => {
                swap_central_path(&old_central_path, &staging_path)?;
                Ok(UpdateOutcome::Updated {
                    new_content_hash: new_hash,
                })
            }
            Ok(UpdateSwap::Unchanged) => {
                let _ = std::fs::remove_dir_all(&staging_path);
                Ok(UpdateOutcome::Unchanged)
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging_path);
                Err(error)
            }
        }
    }

    /// Declares that `vendor_id` wants `skill_id` synced into `target_dir`.
    /// Does not touch the filesystem itself -- call `sync_all` to actually
    /// project it. If another skill_id already has a target row for this
    /// vendor_id landing at `target_dir/<skill name>`, the new one is
    /// disambiguated by suffixing its on-disk leaf name with `vendor_id`
    /// (see README.md#collision-handling, "at sync time").
    pub fn set_target(
        &self,
        vendor_id: &str,
        skill_id: &str,
        target_dir: &Path,
        mode: SyncMode,
    ) -> Result<(), SkillError> {
        let now = now_secs();
        self.with_transaction(|tx| {
            let skill = SkillsRepo::find_by_id(tx, skill_id)?
                .ok_or_else(|| SkillError::SkillNotFound(skill_id.to_string()))?;

            let existing_targets = TargetsRepo::list_for_vendor(tx, vendor_id)?;
            let plain_leaf_taken = existing_targets.iter().any(|t| {
                t.skill_id != skill_id
                    && t.target_path.parent() == Some(target_dir)
                    && t.target_path.file_name().and_then(|n| n.to_str())
                        == Some(skill.name.as_str())
            });
            let leaf = if plain_leaf_taken {
                format!("{}__{}", skill.name, vendor_id)
            } else {
                skill.name.clone()
            };
            let target_path = target_dir.join(leaf);

            let id = uuid::Uuid::new_v4().to_string();
            TargetsRepo::upsert(
                tx,
                &id,
                skill_id,
                vendor_id,
                &target_path.to_string_lossy(),
                mode,
                TargetStatus::Missing,
                now,
            )
        })
    }

    /// Idempotent, self-healing reconcile: makes every `vendor_id` target's
    /// on-disk state match its `skill_targets` row (create/repair
    /// symlinks or junctions, re-copy on content-hash drift). This is the
    /// "always in sync" entry point -- see README.md's reactive-sync
    /// section for when panel-rust should call it.
    pub fn sync_all(&self, vendor_id: &str) -> Result<Vec<SyncResult>, SkillError> {
        let targets = self.with_conn(|conn| TargetsRepo::list_for_vendor(conn, vendor_id))?;
        let now = now_secs();
        let mut results = Vec::with_capacity(targets.len());

        for target in targets {
            let skill = self.with_conn(|conn| SkillsRepo::find_by_id(conn, &target.skill_id))?;
            let (status, error, actual_mode) = match skill {
                None => (
                    TargetStatus::Error,
                    Some("skill no longer exists".to_string()),
                    None,
                ),
                Some(skill) if !skill.central_path.exists() => (
                    TargetStatus::Error,
                    Some(format!(
                        "central copy missing: {}",
                        skill.central_path.display()
                    )),
                    None,
                ),
                Some(skill) => {
                    let format = formats::format_for_vendor(vendor_id);
                    match format.materialize(&skill.central_path, &target.target_path, target.mode)
                    {
                        Ok(mode_used) => (TargetStatus::Linked, None, Some(mode_used)),
                        Err(e) => (TargetStatus::Error, Some(e.to_string()), None),
                    }
                }
            };

            self.with_conn(|conn| {
                if let Some(mode_used) = actual_mode {
                    if mode_used != target.mode {
                        TargetsRepo::update_mode(conn, &target.id, mode_used)?;
                    }
                }
                TargetsRepo::update_status(
                    conn,
                    &target.id,
                    status,
                    matches!(status, TargetStatus::Linked).then_some(now),
                    error.as_deref(),
                )
            })?;

            results.push(SyncResult {
                target_id: target.id,
                skill_id: target.skill_id,
                target_path: target.target_path,
                status,
                error,
            });
        }

        Ok(results)
    }

    pub fn status(&self, vendor_id: &str) -> Result<Vec<SkillTargetRecord>, SkillError> {
        self.with_conn(|conn| TargetsRepo::list_for_vendor(conn, vendor_id))
    }

    /// Tears down `vendor_id`'s sync target(s) for `skill_id`: removes the
    /// on-disk symlink/copy and the `skill_targets` row(s). Used by the
    /// reactive "settings agent disabled" trigger (README.md#reactive-sync)
    /// -- an explicit teardown, not just suppressing future syncs.
    pub fn remove_target(&self, vendor_id: &str, skill_id: &str) -> Result<(), SkillError> {
        let target_paths = self.with_conn(|conn| {
            TargetsRepo::list_for_vendor(conn, vendor_id).map(|targets| {
                targets
                    .into_iter()
                    .filter(|t| t.skill_id == skill_id)
                    .map(|t| t.target_path)
                    .collect::<Vec<_>>()
            })
        })?;

        for target_path in &target_paths {
            sync::remove_target_artifact(target_path)?;
        }

        self.with_conn(|conn| TargetsRepo::delete_for_skill_and_vendor(conn, skill_id, vendor_id))
    }

    #[allow(dead_code)]
    pub(crate) fn central_store_dir(&self) -> &std::path::Path {
        &self.central_store_dir
    }

    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, SkillError>,
    ) -> Result<T, SkillError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    /// Runs `f` inside a sqlite transaction, committing only if `f` succeeds.
    /// Used whenever a domain operation (e.g. register_skill's
    /// insert-or-adopt-or-collide decision) needs its DB writes to be atomic.
    pub(crate) fn with_transaction<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction) -> Result<T, SkillError>,
    ) -> Result<T, SkillError> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }
}

#[cfg(test)]
mod swap_central_path_tests {
    use super::swap_central_path;

    /// Self-review finding: if the second rename (staging -> central_path)
    /// fails after the first one (central_path -> discard) already
    /// succeeded, central_path must not be left missing -- every existing
    /// symlink target still points at it. Triggered here by pointing
    /// `staging_path` at a directory that doesn't exist, so the first
    /// rename (a real directory that does exist) succeeds and the second
    /// (a nonexistent source) reliably fails.
    #[test]
    fn a_failed_second_rename_rolls_back_instead_of_leaving_central_path_missing() {
        let dir = tempfile::tempdir().unwrap();
        let central_path = dir.path().join("central");
        std::fs::create_dir_all(&central_path).unwrap();
        std::fs::write(central_path.join("SKILL.md"), "original content").unwrap();

        let nonexistent_staging = dir.path().join("staging-that-does-not-exist");

        let result = swap_central_path(&central_path, &nonexistent_staging);
        assert!(
            result.is_err(),
            "must report the failure, not silently succeed"
        );
        assert!(
            central_path.exists(),
            "central_path must be rolled back, not left missing, after a failed swap"
        );
        assert_eq!(
            std::fs::read_to_string(central_path.join("SKILL.md")).unwrap(),
            "original content",
            "rolled-back central_path must have its ORIGINAL content, not be empty/corrupted"
        );
    }
}
