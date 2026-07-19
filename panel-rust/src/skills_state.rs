//! Skill discovery -- `skills-management`/`assistant-sidebar refine`'s
//! backend half. Mirrors `protocol_types::McpServerEntry`'s split (a
//! Rust-internal entry type + a narrower Slint-facing `SkillOption`,
//! `models::to_skill_options` converting between them) even though skills
//! have no backend RPC to parse -- they're plain files on disk, one
//! directory per skill, each with a `SKILL.md` front-matter header.
//!
//! Storage location (resolved in `memory/designa/gen/plans/
//! skill-manager-workspace/01-architecture.md`): project-local skills
//! live at `<ProjectsRoot>/<project-name>/.skills/`, inside the same
//! per-project folder `snapshotd` already uses -- the project's existing
//! canonical name/path is the map key, not a second identity scheme.
//! This module takes that directory as a plain parameter rather than
//! reading it from panel state itself, since `active_project_binding`
//! (which would own "what is the active project") hasn't landed yet --
//! decoupling discovery from that wiring lets this phase be built and
//! tested in isolation.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Global,
    Project,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillScope::Global => "global",
            SkillScope::Project => "project",
        }
    }
}

/// One discovered skill directory. `started_from` carries the global
/// skill id a project-local skill was instantiated from, when its
/// `SKILL.md` front matter declares one -- `skills-management`'s
/// "show project_start skills include global `started_from`".
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub started_from: Option<String>,
}

/// Extracts `name`/`description`/`started_from` from a `SKILL.md`'s YAML
/// front matter (delimited by `---` lines, matching this repo's own
/// skill convention -- see e.g. `~/.claude/skills/*/SKILL.md`). Not a
/// full YAML parser: handles the two shapes real front matter here
/// actually uses -- a single-line `key: value`, and a folded block
/// scalar (`key: >-` followed by indented continuation lines, joined
/// with spaces) -- and is tolerant of anything else (missing fields
/// just come back empty, never an error), matching `McpServerEntry::
/// from_json`'s "skip, don't panic" philosophy for content this crate
/// doesn't fully own the schema of.
fn parse_front_matter(contents: &str) -> (String, String, Option<String>) {
    let mut lines = contents.lines();
    if lines.next() != Some("---") {
        return (String::new(), String::new(), None);
    }

    let mut name = String::new();
    let mut description = String::new();
    let mut started_from = None;
    let mut current_key: Option<&str> = None;

    for line in lines {
        if line == "---" {
            break;
        }
        let is_continuation = line.starts_with(' ') || line.starts_with('\t');
        if is_continuation {
            if current_key == Some("description") {
                if !description.is_empty() {
                    description.push(' ');
                }
                description.push_str(line.trim());
            }
            continue;
        }
        current_key = None;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "name" => name = value.to_string(),
            "description" => {
                if value.is_empty() || value == ">-" || value == "|" || value == ">" {
                    current_key = Some("description");
                } else {
                    description = value.trim_matches('"').to_string();
                }
            }
            "started_from" => {
                if !value.is_empty() {
                    started_from = Some(value.trim_matches('"').to_string());
                }
            }
            _ => {}
        }
    }
    (name, description, started_from)
}

/// Scans `dir` one level deep for `<subdir>/SKILL.md` entries. Returns an
/// empty list (not an error) if `dir` doesn't exist yet -- a project or
/// global skills directory that was never created is a cold start, not
/// a failure, matching `JsonlStore::load`'s "cache miss, not an error"
/// convention.
pub fn scan_skills_dir(dir: &Path, scope: SkillScope) -> Vec<SkillEntry> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<SkillEntry> = read_dir
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let skill_md = entry.path().join("SKILL.md");
            let contents = fs::read_to_string(&skill_md).ok()?;
            let (front_matter_name, description, started_from) = parse_front_matter(&contents);
            let name = if front_matter_name.is_empty() {
                entry.file_name().to_string_lossy().into_owned()
            } else {
                front_matter_name
            };
            Some(SkillEntry {
                name,
                description,
                path: entry.path(),
                scope,
                started_from,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// `<ProjectsRoot>/<project-name>/.skills/` -- the project-local skills
/// directory inside the project's existing canonical folder, per
/// `01-architecture.md`'s resolved storage-path decision.
pub fn project_skills_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".skills")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, subdir: &str, front_matter: &str) {
        let skill_dir = dir.join(subdir);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), front_matter).unwrap();
    }

    #[test]
    fn scans_single_line_front_matter() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "voice-embedding",
            "---\nname: voice-embedding\ndescription: turns narration into a video\n---\nbody\n",
        );
        let entries = scan_skills_dir(dir.path(), SkillScope::Global);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "voice-embedding");
        assert_eq!(entries[0].description, "turns narration into a video");
        assert_eq!(entries[0].scope, SkillScope::Global);
        assert_eq!(entries[0].started_from, None);
    }

    #[test]
    fn scans_folded_block_scalar_description() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "can-process",
            "---\nname: can-process\ndescription: >-\n  Checks whether it's safe to keep\n  processing given usage.\n---\n",
        );
        let entries = scan_skills_dir(dir.path(), SkillScope::Global);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].description,
            "Checks whether it's safe to keep processing given usage."
        );
    }

    #[test]
    fn falls_back_to_directory_name_when_front_matter_name_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "unnamed-skill", "---\ndescription: no name field\n---\n");
        let entries = scan_skills_dir(dir.path(), SkillScope::Project);
        assert_eq!(entries[0].name, "unnamed-skill");
    }

    #[test]
    fn parses_started_from_provenance() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "voice-embedding",
            "---\nname: voice-embedding\ndescription: d\nstarted_from: global/voice-embedding\n---\n",
        );
        let entries = scan_skills_dir(dir.path(), SkillScope::Project);
        assert_eq!(
            entries[0].started_from,
            Some("global/voice-embedding".to_string())
        );
    }

    #[test]
    fn ignores_subdirectories_without_a_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();
        let entries = scan_skills_dir(dir.path(), SkillScope::Global);
        assert!(entries.is_empty());
    }

    #[test]
    fn missing_directory_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let entries = scan_skills_dir(&missing, SkillScope::Project);
        assert!(entries.is_empty());
    }

    #[test]
    fn results_are_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "zeta", "---\nname: zeta\ndescription: z\n---\n");
        write_skill(dir.path(), "alpha", "---\nname: alpha\ndescription: a\n---\n");
        let entries = scan_skills_dir(dir.path(), SkillScope::Global);
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[1].name, "zeta");
    }

    #[test]
    fn project_skills_dir_is_dot_skills_inside_the_project_folder() {
        let project_dir = Path::new("/tmp/example-project");
        assert_eq!(
            project_skills_dir(project_dir),
            Path::new("/tmp/example-project/.skills")
        );
    }
}
