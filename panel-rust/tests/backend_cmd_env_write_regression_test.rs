//! PROF-5 (`profile-only-backend-selection` plan) regression guard.
//!
//! PROF-3 removed every production write of `ACPX_BACKEND_CMD` from this
//! crate; PROF-4 migrated the dev/test harnesses under `tests/` off it onto
//! a real admin-plane profile. This test is what keeps that true: it scans
//! every `.rs` file under `src/` for a real `.env("ACPX_BACKEND_CMD", ...)`-
//! shaped WRITE (and its planned rename, `ACPX_DEFAULT_ACP_COMMAND`,
//! arriving later via the agents-install-runtime worktree -- covered here
//! now even though it greps to zero today, so a zero-hit search never gets
//! mistaken for coverage) and fails if either appears anywhere except the
//! one allowed block in `src/agent_bridge.rs`
//! (`test_only_set_backend_cmd_env`, bracketed by
//! `PROF5-GUARD-ALLOW-START`/`-END` comments) -- the single,
//! `#[cfg(test)]`-gated, in-crate-unit-test-only exemption documented
//! there.
//!
//! **Matches the call shape, not the bare name.** An earlier draft of this
//! guard matched any occurrence of the quoted string `"ACPX_BACKEND_CMD"`
//! and would have failed on every doc-comment/prose mention that explains
//! *why* production never writes it (agent_bridge.rs's own PROF-3/PROF-5
//! comments, this very module doc, `update.rs`'s one reference) -- the
//! tempting fix for that false-positive is to delete the prose, which
//! would delete the exact warning that stops a future edit from
//! reintroducing the bug. So this only matches `.env(` immediately
//! followed (allowing whitespace/newlines -- see below) by the quoted
//! name as the first argument: a real write, not a mention.
//!
//! **Must span multiple lines.** One of the real call sites this guard
//! exists to keep clean was originally written as
//! `.env(\n    "ACPX_BACKEND_CMD",\n    value,\n)` -- and fooled an
//! earlier hand-written single-line grep during PROF-3/PROF-4 audits
//! before someone re-checked with a bare substring search instead of a
//! call-shape pattern. [`write_pattern`] uses `\s*` (which matches
//! newlines) between `.env(` and the quoted name specifically so this
//! guard doesn't repeat that mistake. [`multiline_write_is_detected`]
//! below is the fixture that proves it: a regression guard nobody has
//! seen fail is not yet known to work.

use std::path::{Path, PathBuf};

/// `\.env\s*\(\s*"NAME"` for either guarded name -- `\s*` (not a fixed
/// number of spaces) is what makes this match both
/// `.env("ACPX_BACKEND_CMD", x)` and the multi-line
/// `.env(\n "ACPX_BACKEND_CMD",\n x,\n)` shape alike.
fn write_pattern() -> regex::Regex {
    regex::Regex::new(r#"\.env\s*\(\s*"(ACPX_BACKEND_CMD|ACPX_DEFAULT_ACP_COMMAND)""#)
        .expect("static regex must compile")
}

const ALLOW_START: &str = "PROF5-GUARD-ALLOW-START";
const ALLOW_END: &str = "PROF5-GUARD-ALLOW-END";

/// Byte ranges of `contents` that fall between an `ALLOW_START` line and
/// its matching `ALLOW_END` line (inclusive of both marker lines).
fn allow_block_ranges(contents: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut block_start: Option<usize> = None;
    let mut offset = 0usize;
    for line in contents.split_inclusive('\n') {
        if line.contains(ALLOW_START) {
            assert!(
                block_start.is_none(),
                "nested {ALLOW_START} -- exactly one allow block is expected per file"
            );
            block_start = Some(offset);
        } else if line.contains(ALLOW_END) {
            let start = block_start.take().unwrap_or_else(|| {
                panic!("{ALLOW_END} with no matching {ALLOW_START}");
            });
            ranges.push((start, offset + line.len()));
        }
        offset += line.len();
    }
    assert!(
        block_start.is_none(),
        "{ALLOW_START} was never closed with a matching {ALLOW_END}"
    );
    ranges
}

fn line_number_at(contents: &str, byte_offset: usize) -> usize {
    contents[..byte_offset].matches('\n').count() + 1
}

/// The core check, factored out so both the real `src/` scan below and
/// [`multiline_write_is_detected`]'s fixture exercise identical logic --
/// a hand-maintained "looks equivalent" copy in the fixture test would be
/// exactly the kind of divergence that lets a guard quietly stop working.
/// Returns one human-readable violation string per disallowed write found.
fn find_violations(path_label: &str, contents: &str) -> Vec<String> {
    let allowed = allow_block_ranges(contents);
    let pattern = write_pattern();
    pattern
        .find_iter(contents)
        .filter(|m| {
            !allowed
                .iter()
                .any(|(start, end)| *start <= m.start() && m.end() <= *end)
        })
        .map(|m| {
            let line_no = line_number_at(contents, m.start());
            format!("{path_label}:{line_no}: {}", m.as_str())
        })
        .collect()
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn acpx_backend_cmd_env_writes_appear_only_in_the_one_documented_exemption() {
    let mut files = Vec::new();
    collect_rs_files(&src_dir(), &mut files);
    assert!(
        !files.is_empty(),
        "found no .rs files under src/ -- src_dir()/collect_rs_files is broken, \
         not proof the crate is clean"
    );

    let mut violations: Vec<String> = Vec::new();
    let mut allowed_write_count = 0usize;

    for path in &files {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        let label = path.display().to_string();
        violations.extend(find_violations(&label, &contents));

        // Count writes inside the allow block separately (not exercised
        // via find_violations, which only returns disallowed ones) so the
        // "did the guard and the code drift apart" sanity check below has
        // something real to assert on.
        let allowed = allow_block_ranges(&contents);
        allowed_write_count += write_pattern()
            .find_iter(&contents)
            .filter(|m| {
                allowed
                    .iter()
                    .any(|(start, end)| *start <= m.start() && m.end() <= *end)
            })
            .count();
    }

    assert!(
        violations.is_empty(),
        "ACPX_BACKEND_CMD/ACPX_DEFAULT_ACP_COMMAND may only be WRITTEN from the one \
         documented, #[cfg(test)]-gated exemption in src/agent_bridge.rs \
         (test_only_set_backend_cmd_env) -- a real backend must be selected through a \
         profile (_acpx.profile), never an exported command string (see PROF-3/PROF-5's \
         own doc comments for why). Found {} violation(s):\n{}",
        violations.len(),
        violations.join("\n")
    );

    // Not a correctness requirement on its own (a future refactor could
    // legitimately reduce this to zero call sites and delete the allow
    // block entirely) -- but as long as the ALLOW_START/END markers exist
    // at all, they should bracket at least one real write, or the block
    // and the code it's meant to cover have drifted apart and this guard
    // is quietly checking nothing.
    assert!(
        allowed_write_count > 0,
        "the documented allow-block exists but contains zero guarded .env(...) writes -- \
         the block and the code it's meant to bracket have drifted apart"
    );
}

/// Proves [`find_violations`] actually catches the multi-line
/// `.env(\n "ACPX_BACKEND_CMD",\n ...)` shape described in this file's own
/// module doc comment -- exercised against a hand-written string, not a
/// real file under `src/`, specifically so this test's own pass/fail
/// doesn't depend on anything in the crate happening to still be
/// multi-line by coincidence. A regression guard nobody has seen fail is
/// not yet known to work.
#[test]
fn multiline_write_is_detected() {
    let fixture = concat!(
        "fn spawn_something(command: &mut std::process::Command) {\n",
        "    command\n",
        "        .env(\n",
        "            \"ACPX_BACKEND_CMD\",\n",
        "            \"npx -y some-fake-package\",\n",
        "        );\n",
        "}\n",
    );
    let violations = find_violations("fixture.rs", fixture);
    assert_eq!(
        violations.len(),
        1,
        "expected the guard to catch exactly one multi-line write, got: {violations:?}"
    );
    assert!(
        violations[0].contains("fixture.rs:3"),
        "expected the reported line to be the `.env(` line the multi-line call starts on, \
         got: {violations:?}"
    );
}

/// Companion to [`multiline_write_is_detected`]: proves a write correctly
/// placed inside an allow block is NOT reported, so the guard's "false
/// positive" side is exercised too, not just its "catches a real write"
/// side.
#[test]
fn write_inside_an_allow_block_is_not_a_violation() {
    let fixture = concat!(
        "// PROF5-GUARD-ALLOW-START\n",
        "fn test_only_helper(command: &mut std::process::Command) {\n",
        "    command.env(\"ACPX_BACKEND_CMD\", \"whatever\");\n",
        "}\n",
        "// PROF5-GUARD-ALLOW-END\n",
    );
    let violations = find_violations("fixture.rs", fixture);
    assert!(
        violations.is_empty(),
        "a write inside the documented allow block must not be reported, got: {violations:?}"
    );
}

/// Companion to the two above: a bare prose mention (no `.env(` call
/// shape at all) must never be reported -- this is the false-positive
/// mode the call-shape match specifically exists to avoid (see this
/// file's own module doc comment for the real history: an earlier
/// substring-only draft of this guard would have failed on this exact
/// shape and made deleting the warning comment the "fix").
#[test]
fn bare_prose_mention_is_not_a_violation() {
    let fixture = concat!(
        "// KNOWN ACCEPTED GAP: falls through to acpx-server's own\n",
        "// \"ACPX_BACKEND_CMD\" default. Do not fix this by reintroducing\n",
        "// an env-var write.\n",
    );
    let violations = find_violations("fixture.rs", fixture);
    assert!(
        violations.is_empty(),
        "a bare mention with no .env(...) call shape must not be reported, got: {violations:?}"
    );
}
