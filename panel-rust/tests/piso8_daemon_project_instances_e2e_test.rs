//! PISO-8 (project-isolation-mlt-binding plan) real e2e: spawns an actual
//! `snapshotd serve` daemon (built fresh from `../snapshotd`, not stubbed),
//! drives it via `snapshotd launch` the exact same way a real agent's
//! `daemon.launch` MCP call would create a live (headless-by-default)
//! instance for a project this panel's own host process never opened, then
//! calls this crate's real `fetch_daemon_project_instances()` -- the same
//! function `Effect::RefreshDaemonProjectInstances` calls in production --
//! and asserts it correlates that live instance to its real project path.
//!
//! Mirrors `snapshotd/cmd/snapshotd/main_e2e_test.go`'s own harness
//! (`buildBinary`/`runCLI`/`waitForSocket`) from the Rust side, so this is a
//! genuine "spawn the real binary, dial the real socket" round trip per the
//! plan's `need:e2e-tests`/ground-origin discipline -- not a stub, not a
//! hand-typed JSONL fixture (that pure-parsing case is already covered by
//! `agent_bridge::tests::parse_daemon_list_and_projects_*` in-crate).
//!
//! Skips (prints a message, does not fail) when the `go` toolchain isn't on
//! `PATH` -- this crate does not otherwise depend on Go being installed to
//! build or test, and CI/dev environments without it must not fail here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn snapshotd_module_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../snapshotd")
}

/// Mirrors `main_e2e_test.go`'s `buildBinary`: compiles a package under the
/// snapshotd module into `out_dir`, returning the built binary's path.
fn build_binary(out_dir: &Path, pkg: &str, name: &str) -> PathBuf {
    let out = out_dir.join(name);
    let status = Command::new("go")
        .current_dir(snapshotd_module_dir())
        .args(["build", "-o"])
        .arg(&out)
        .arg(pkg)
        .status()
        .expect("spawning `go build`");
    assert!(status.success(), "go build -o {out:?} {pkg} failed");
    out
}

fn wait_for_socket(path: &Path, timeout: Duration, serve: &mut Child) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        if let Ok(Some(status)) = serve.try_wait() {
            panic!("snapshotd serve exited early with {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for control socket at {path:?}");
}

#[test]
fn fetch_daemon_project_instances_reports_a_real_agent_launched_headless_project_as_live() {
    if !go_available() {
        println!(
            "skipping fetch_daemon_project_instances_reports_a_real_agent_launched_headless_project_as_live: \
             `go` not found on PATH"
        );
        return;
    }

    let build_dir = tempfile::tempdir().expect("build dir");
    let snapshotd_bin = build_binary(build_dir.path(), "./cmd/snapshotd", "snapshotd-bin");
    let fixture_bin = build_binary(
        build_dir.path(),
        "./internal/procmgr/testdata/fixture",
        "fixture-bin",
    );

    let home_dir = tempfile::tempdir().expect("SNAPSHOTD_HOME dir");
    let project_dir = tempfile::tempdir().expect("project dir");
    // A real, non-empty project directory -- `daemon.launch` resolving an
    // ordinary folder path is the same shape a real agent's `daemon.launch`
    // MCP call would be given, not a synthetic edge case.
    std::fs::create_dir_all(project_dir.path()).expect("project dir exists");

    let mut serve = Command::new(&snapshotd_bin)
        .args(["serve", "--no-mcp"])
        .env("SNAPSHOTD_HOME", home_dir.path())
        .env("SNAPSHOT_BIN_PATH", &fixture_bin)
        .env("SNAPSHOTD_ACPX_ENABLED", "false")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning snapshotd serve");

    let control_sock = home_dir.path().join("control.sock");
    wait_for_socket(&control_sock, Duration::from_secs(5), &mut serve);

    // Real `daemon.launch` via the CLI, headless-by-default (no `--gui`) --
    // exactly the path an agent's own MCP tool call drives, per PISO-8's
    // "agent chooses a different project" scenario: this panel's own host
    // process never opened `project_dir`.
    let launch_output = Command::new(&snapshotd_bin)
        .args(["launch"])
        .arg(project_dir.path())
        .env("SNAPSHOTD_HOME", home_dir.path())
        .output()
        .expect("spawning snapshotd launch");
    assert!(
        launch_output.status.success(),
        "snapshotd launch failed: {}",
        String::from_utf8_lossy(&launch_output.stderr)
    );
    let launched: serde_json::Value =
        serde_json::from_slice(&launch_output.stdout).expect("parse launch output as JSON");
    let launched_id = launched
        .get("ID")
        .and_then(|v| v.as_str())
        .expect("launch output has a non-empty ID");
    assert!(!launched_id.is_empty());

    // This is the exact function `Effect::RefreshDaemonProjectInstances`
    // calls in production (`effect_executor.rs`), pointed at the daemon
    // this test just spawned via the same env vars the CLI subprocess it
    // shells out to inherits (`run_snapshotd_subcommand`'s own doc comment:
    // it never touches `SNAPSHOTD_HOME` itself, it inherits the caller's).
    unsafe {
        std::env::set_var("RUI_SNAPSHOTD_BIN", &snapshotd_bin);
        std::env::set_var("SNAPSHOTD_HOME", home_dir.path());
    }
    let instances = panel_rust::test_support::fetch_daemon_project_instances()
        .expect("fetch_daemon_project_instances against a real spawned daemon");
    unsafe {
        std::env::remove_var("RUI_SNAPSHOTD_BIN");
        std::env::remove_var("SNAPSHOTD_HOME");
    }

    let expected_path = project_dir.path().join("project.mlt");
    let found = instances
        .iter()
        .find(|instance| Path::new(&instance.project_path) == expected_path);
    assert!(
        found.is_some(),
        "expected a live instance for {expected_path:?} (agent-launched project this panel's \
         own host never opened), got: {instances:?}"
    );

    // Teardown: mirror `main_e2e_test.go`'s cleanup -- kill rather than
    // `snapshotd stop` since this test doesn't need graceful shutdown
    // semantics, only that the process (and its control socket) go away.
    let _ = serve.kill();
    let _ = serve.wait();
    let mut stderr = String::new();
    if let Some(mut stream) = serve.stderr.take() {
        use std::io::Read;
        let _ = stream.read_to_string(&mut stderr);
    }
    if !stderr.trim().is_empty() {
        let _ = writeln!(std::io::stderr(), "snapshotd serve stderr:\n{stderr}");
    }
}
