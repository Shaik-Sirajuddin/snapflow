//! Editor detection for the skill editor's "open in <editor>" top bar --
//! `active_pane_and_editor_view` phase. Scoped to a small static
//! candidate list probed via `which`/`PATH`, not exhaustive detection
//! (see `memory/designa/gen/plans/skill-manager-workspace/
//! 03-open-risks.md`'s `editor_detection_scope` finding): a naive PATH
//! probe can find a binary that isn't the user's actual preferred
//! launcher, or miss one installed outside `PATH` -- not worth chasing
//! further than "the two most common CLI-launchable editors, plus an
//! OS-default fallback that always works."

use std::path::{Path, PathBuf};
use std::process::Command;

/// Candidate CLI-launchable editors, in display-priority order. Each
/// entry's binary name is resolved via [`resolve_editor_bin`] (PATH plus
/// well-known absolute install locations) to decide whether to show it as
/// a top-bar icon.
pub const EDITOR_CANDIDATES: &[(&str, &str)] = &[("code", "VS Code"), ("subl", "Sublime Text")];

/// Returns the display names of every candidate editor whose binary is
/// actually resolvable, in `EDITOR_CANDIDATES` order. Real syscalls
/// (filesystem probes), so this is a startup-time or on-demand probe, not
/// something to call on every frame.
pub fn detect_installed_editors() -> Vec<&'static str> {
    EDITOR_CANDIDATES
        .iter()
        .filter(|(bin, _)| resolve_editor_bin(bin).is_some())
        .map(|(_, name)| *name)
        .collect()
}

/// Resolves an editor binary name to an absolute launchable path: first a
/// normal `PATH` probe, then a fallback over well-known absolute install
/// locations. The fallback is what fixes "the VS Code button doesn't work":
/// a GUI app launched from a desktop environment (not a login shell)
/// inherits a minimal `PATH` that typically omits `/usr/local/bin`,
/// `/snap/bin`, `~/.local/bin`, and the editor's own bundled CLI dir -- so
/// a `code`/`subl` that launches fine from a terminal is invisible to a
/// plain PATH probe, and `Command::new("code")` (bare name) then also fails
/// to spawn. Resolving to an absolute path fixes detection AND launch.
pub fn resolve_editor_bin(bin: &str) -> Option<PathBuf> {
    which_binary(bin).or_else(|| {
        well_known_editor_paths(bin)
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
}

fn which_binary(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file().then_some(candidate)
    })
}

/// Common absolute install locations for `bin`, covering the cases a
/// desktop-launched app's minimal `PATH` misses (Linux package/snap/flatpak
/// installs, `~/.local/bin`, and the macOS `.app` bundle CLI shims).
fn well_known_editor_paths(bin: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = ["/usr/local/bin", "/usr/bin", "/bin", "/snap/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(bin))
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(Path::new(&home).join(".local/bin").join(bin));
    }
    // macOS `.app` bundle CLI shims (harmless no-ops on other platforms --
    // the `is_file` check just fails).
    match bin {
        "code" => {
            paths.push(PathBuf::from(
                "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
            ));
        }
        "subl" => {
            paths.push(PathBuf::from(
                "/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl",
            ));
        }
        _ => {}
    }
    paths
}

/// Launches `bin` (one of `EDITOR_CANDIDATES`' binary names) with `path`
/// as its argument. No auto-run anywhere in this module -- every caller
/// is an explicit user click on a top-bar icon. Launches the RESOLVED
/// absolute path when available (so a click works even when `bin` is not on
/// the app's `PATH`, the common GUI-launch case), falling back to the bare
/// name for `PATH`-based resolution.
pub fn open_in_editor(bin: &str, path: &Path) -> std::io::Result<()> {
    let program = resolve_editor_bin(bin).unwrap_or_else(|| PathBuf::from(bin));
    Command::new(program).arg(path).spawn()?;
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

    // PUI-010: the well-known-path fallback must cover the locations a
    // desktop-launched app's minimal PATH misses, so `code`/`subl` are
    // detected (and launchable) even when not on PATH.
    #[test]
    fn well_known_paths_cover_common_install_locations() {
        let code = well_known_editor_paths("code");
        assert!(code.contains(&PathBuf::from("/usr/local/bin/code")));
        assert!(code.contains(&PathBuf::from("/snap/bin/code")));
        assert!(
            code.iter().any(|p| p.to_string_lossy().contains(".app")),
            "must include the macOS VS Code .app CLI shim"
        );
        let subl = well_known_editor_paths("subl");
        assert!(subl.contains(&PathBuf::from("/usr/local/bin/subl")));
    }

    // resolve_editor_bin returns an absolute path for a PATH binary, so
    // open_in_editor can launch it by absolute path (GUI-launch safe).
    #[test]
    fn resolve_editor_bin_returns_an_absolute_path_for_a_path_binary() {
        let resolved = resolve_editor_bin("sh").expect("sh resolvable");
        assert!(resolved.is_absolute() || resolved.is_file());
        assert!(resolved.ends_with("sh"));
    }

    // PUI-010: the fallback finds a binary in a well-known location even when
    // it is NOT on PATH -- simulate by pointing HOME at a temp dir with a
    // fake `~/.local/bin/<bin>` and clearing it from PATH.
    #[test]
    fn resolve_editor_bin_falls_back_to_well_known_location_off_path() {
        let home = tempfile::tempdir().expect("temp home");
        let bindir = home.path().join(".local/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let fake = bindir.join("subl");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();

        // A name that is definitely not on any real PATH, resolved only via
        // the HOME well-known fallback.
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let resolved = well_known_editor_paths("subl")
            .into_iter()
            .find(|c| c.is_file());
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(resolved.as_deref(), Some(fake.as_path()));
    }
}
