//! Skills abstraction / domain layer support: content identity (hash) and
//! metadata (frontmatter) reading. See README.md#abstraction-layers.

mod frontmatter;
mod hash;

pub(crate) use frontmatter::parse_skill_md;
pub(crate) use hash::hash_dir;

/// The `name` `register_skill` would assign a skill registered from
/// `source_dir`: the frontmatter's `name` field if present, else the
/// directory's own basename. Shared logic, not duplicated -- callers
/// that need to resolve "which skill_id does this directory on disk
/// correspond to" (e.g. panel-rust's skill-editor save path, which only
/// has a directory path, never a skill_id) should derive the same name
/// `register_skill` would have used, rather than reimplementing this
/// fallback separately and risking the two disagreeing.
pub(crate) fn resolve_skill_name(source_dir: &std::path::Path) -> String {
    parse_skill_md(source_dir).name.unwrap_or_else(|| {
        source_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}
