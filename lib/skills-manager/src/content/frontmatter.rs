//! Ported from xingkongliang/skills-manager (MIT License,
//! Copyright (c) 2026 Tianliang Zhang), src-tauri/src/core/skill_metadata.rs
//! `parse_skill_md`/`parse_frontmatter` -- see README.md#provenance in the
//! plan doc (memory/acpx/gen/plans/acpx-skills/) for what's ported vs. fresh.

use std::path::Path;

pub(crate) struct SkillMeta {
    pub name: Option<String>,
    pub description: Option<String>,
}

fn read_named_file_exact(dir: &Path, target_name: &str) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().ok()?.is_file() {
            continue;
        }
        if entry.file_name().to_string_lossy() == target_name {
            return std::fs::read_to_string(entry.path()).ok();
        }
    }
    None
}

pub(crate) fn parse_skill_md(dir: &Path) -> SkillMeta {
    for candidate in ["SKILL.md", "skill.md"] {
        if let Some(content) = read_named_file_exact(dir, candidate) {
            return parse_frontmatter(&content);
        }
    }
    SkillMeta {
        name: None,
        description: None,
    }
}

fn parse_frontmatter(content: &str) -> SkillMeta {
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return SkillMeta {
            name: None,
            description: None,
        };
    }

    let rest = &trimmed[3..];
    let Some(end) = rest.find("---") else {
        return SkillMeta {
            name: None,
            description: None,
        };
    };
    let yaml_str = &rest[..end];
    let Ok(yaml) = serde_yaml::from_str::<serde_yaml::Value>(yaml_str) else {
        return SkillMeta {
            name: None,
            description: None,
        };
    };

    let name = yaml
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let description = yaml
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    SkillMeta { name, description }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_full() {
        let content = "---\nname: commit\ndescription: Create commits\n---\nBody";
        let meta = parse_frontmatter(content);
        assert_eq!(meta.name.as_deref(), Some("commit"));
        assert_eq!(meta.description.as_deref(), Some("Create commits"));
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let meta = parse_frontmatter("# Just markdown\nNo frontmatter here.");
        assert!(meta.name.is_none());
        assert!(meta.description.is_none());
    }

    #[test]
    fn parse_frontmatter_empty_string() {
        let meta = parse_frontmatter("");
        assert!(meta.name.is_none());
        assert!(meta.description.is_none());
    }
}
