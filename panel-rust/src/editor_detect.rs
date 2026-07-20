//! Editor detection for the skill editor's "open in <editor>" top bar --
//! `active_pane_and_editor_view` phase. Scoped to a small static
//! candidate list probed via `which`/`PATH`, not exhaustive detection
//! (see `memory/designa/gen/plans/skill-manager-workspace/
//! 03-open-risks.md`'s `editor_detection_scope` finding): a naive PATH
//! probe can find a binary that isn't the user's actual preferred
//! launcher, or miss one installed outside `PATH` -- not worth chasing
//! further than "the two most common CLI-launchable editors, plus an
//! OS-default fallback that always works."

use std::path::Path;
use std::process::Command;

/// Candidate CLI-launchable editors, in display-priority order. Each
/// entry's binary name is checked against `PATH` via `which` (not
/// invoked) to decide whether to show it as a top-bar icon.
pub const EDITOR_CANDIDATES: &[(&str, &str)] = &[("code", "VS Code"), ("subl", "Sublime Text")];

/// Returns the display names of every candidate editor whose binary is
/// actually found on `PATH`, in `EDITOR_CANDIDATES` order. Real syscalls
/// (`which`), so this is a startup-time or on-demand probe, not
/// something to call on every frame.
pub fn detect_installed_editors() -> Vec<&'static str> {
    EDITOR_CANDIDATES
        .iter()
        .filter(|(bin, _)| which_binary(bin).is_some())
        .map(|(_, name)| *name)
        .collect()
}

fn which_binary(bin: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file().then_some(candidate)
    })
}

/// Launches `bin` (one of `EDITOR_CANDIDATES`' binary names) with `path`
/// as its argument. No auto-run anywhere in this module -- every caller
/// is an explicit user click on a top-bar icon.
pub fn open_in_editor(bin: &str, path: &Path) -> std::io::Result<()> {
    Command::new(bin).arg(path).spawn()?;
    Ok(())
}

/// Opens `path` with whatever the OS considers its default handler for
/// that file type -- the top bar's always-available fallback icon, via
/// the `opener` crate (checked `Cargo.lock` first: no existing
/// default-app-opener dependency, so this is the one new dependency
/// rather than hand-rolling per-OS `xdg-open`/`open`/`start` branching).
pub fn open_with_os_default(path: &Path) -> std::io::Result<()> {
    opener::open(path).map_err(|error| std::io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_binary_finds_a_real_binary_known_to_exist() {
        // `sh` is about as close to a universal guarantee as this gets
        // on any Unix CI/dev box this crate already assumes (portable-pty
        // spawns real shells elsewhere in this crate too).
        assert!(which_binary("sh").is_some());
    }

    #[test]
    fn which_binary_returns_none_for_a_made_up_name() {
        assert!(which_binary("definitely-not-a-real-binary-xyz123").is_none());
    }

    #[test]
    fn detect_installed_editors_only_returns_real_candidates() {
        let detected = detect_installed_editors();
        for name in &detected {
            assert!(EDITOR_CANDIDATES.iter().any(|(_, n)| n == name));
        }
    }
}
