//! Real, headless end-to-end coverage driven through Slint's own official
//! MCP UI-testing server (`i-slint-backend-testing::mcp_server`, wired into
//! `panel_rust_create` -- see `lib.rs`'s `SpikePlatform`/`SLINT_MCP_PORT`
//! doc comments), against the actual compiled `shotcut` binary running
//! under a real Xvfb display with a real (mock-backed) `acpx-server`.
//!
//! This exists to close a real gap the in-process headless
//! `i_slint_backend_testing` harness (`slint_component_e2e_test.rs`)
//! cannot: `sidebar_thread_close_and_delete_controls_are_addressable_and_
//! two_step_confirmed` there documents a specific IconButton (the
//! thread-row close/delete arm control) that never appears in that
//! harness's own element tree, even with its render condition hardcoded
//! to `true`, while proven correct by code inspection and live VNC
//! click-through. Driving the real compiled UI over MCP (not a headless
//! stand-in, not a screenshot-and-guess) is the only way to get a real,
//! checked-in assertion that this control genuinely exists and responds
//! in production, closing that harness anomaly with actual evidence
//! instead of leaving it as a permanently-excused red test.
//!
//! Mirrors `host_e2e_smoke.sh`'s real-process wiring (Xvfb, real
//! `acpx-server`, real `shotcut`, one temp state dir) but drives the UI
//! through Slint's MCP JSON-RPC surface instead of XTEST coordinates --
//! see `memory/editor/gen/plans/video-generation-e2e-harness/scripts/
//! runtime_gate_full_matrix.md` for the recipe this test's calls follow
//! (already proven end to end there via manual `curl`; this is that same
//! sequence promoted to real, checked-in test code).

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("repo root")
}

fn mock_agent_bin() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/rui-mock-agent")
}

fn acpx_server_bin() -> PathBuf {
    repo_root().join("acpx/target/debug/acpx-server")
}

fn shotcut_bin() -> PathBuf {
    repo_root().join("shotcut/build/cc-debug-linux/src/shotcut")
}

/// Same "bind an ephemeral port, drop it immediately, hand the number to
/// the real process" trick `gateway_actor_e2e_test.rs`'s `free_port()`
/// uses -- has the same inherent TOCTOU gap that helper's own doc comment
/// documents; acceptable here for the same reason (this is a single,
/// bounded-once acquisition per test run, not a hot loop).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn free_x_display() -> u32 {
    let mut display = 200;
    while std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
        display += 1;
    }
    display
}

/// Kills every spawned real process and removes the temp state dir on
/// `Drop` -- so a panicking assertion mid-test still cleans up, matching
/// `startup_recovery_test.rs`'s `BinaryGuard` shape. Set
/// `PANEL_MCP_E2E_KEEP_STATE=1` (mirrors `host_e2e_smoke.sh`'s own
/// `PANEL_HOST_E2E_KEEP_STATE`) to keep the state dir (shotcut/acpx logs,
/// appdata) around for debugging a failure.
struct LiveUiHarness {
    xvfb: Child,
    acpx_server: Child,
    shotcut: Child,
    state_dir: PathBuf,
    mcp_port: u16,
    client: reqwest::Client,
}

impl Drop for LiveUiHarness {
    fn drop(&mut self) {
        for child in [&mut self.shotcut, &mut self.acpx_server, &mut self.xvfb] {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::env::var_os("PANEL_MCP_E2E_KEEP_STATE").is_none() {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }
}

impl LiveUiHarness {
    async fn spawn() -> Self {
        for binary in [mock_agent_bin(), acpx_server_bin(), shotcut_bin()] {
            assert!(
                binary.exists(),
                "required binary missing, build it first: {}",
                binary.display()
            );
        }

        let state_dir = std::env::temp_dir().join(format!(
            "panel-slint-mcp-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(state_dir.join("acpx")).expect("create acpx state dir");
        std::fs::create_dir_all(state_dir.join("panel")).expect("create panel cache dir");
        std::fs::create_dir_all(state_dir.join("shotcut")).expect("create shotcut appdata dir");

        let display = free_x_display();
        let display_str = format!(":{display}");
        let xvfb = Command::new("Xvfb")
            .args([&display_str, "-screen", "0", "1280x800x24", "-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb");
        eprintln!("[harness] state_dir={}", state_dir.display());

        let xdpyinfo_deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let ready = Command::new("xdpyinfo")
                .arg("-display")
                .arg(&display_str)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ready {
                break;
            }
            assert!(
                std::time::Instant::now() < xdpyinfo_deadline,
                "Xvfb on {display_str} never became ready"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let gateway_port = free_port();
        let persona = "codex";
        let acpx_server = Command::new(acpx_server_bin())
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{gateway_port}"))
            .env(
                "ACPX_BACKEND_CMD",
                mock_agent_bin().to_string_lossy().to_string(),
            )
            .env("ACPX_DEFAULT_AGENT_ID", persona)
            .env("ACPX_DB_PATH", state_dir.join("acpx/gateway.sqlite3"))
            .env("RUI_MOCK_AGENT_PERSONA", persona)
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn real acpx-server binary");

        let client = reqwest::Client::new();
        let health_deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if client
                .get(format!("http://127.0.0.1:{gateway_port}/health"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            assert!(
                std::time::Instant::now() < health_deadline,
                "acpx-server never became healthy"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let mcp_port = free_port();
        let shotcut = Command::new(shotcut_bin())
            .args(["--appdata", state_dir.join("shotcut").to_str().unwrap(), "--noupgrade"])
            .env("DISPLAY", &display_str)
            .env("QSG_RENDER_LOOP", "basic")
            .env("SLINT_MCP_PORT", mcp_port.to_string())
            .env("RUI_ACP_CACHE_DIR", state_dir.join("panel"))
            .env(
                "RUI_ACPX_CODEX_URL",
                format!("http://127.0.0.1:{gateway_port}"),
            )
            .env(
                "RUI_ACPX_CLAUDE_URL",
                format!("http://127.0.0.1:{gateway_port}"),
            )
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(state_dir.join("shotcut.stdout.log")).unwrap())
            .stderr(std::fs::File::create(state_dir.join("shotcut.stderr.log")).unwrap())
            .spawn()
            .expect("spawn real shotcut binary");

        let harness = LiveUiHarness {
            xvfb,
            acpx_server,
            shotcut,
            state_dir,
            mcp_port,
            client,
        };

        // The MCP HTTP listener only starts once the window-shown hook
        // fires and `spawn_local` actually schedules the server task
        // (see lib.rs's `SpikeEventLoopProxy` doc comment) -- poll
        // `initialize` rather than assuming a fixed settle time.
        let mcp_deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if harness
                .try_mcp_call("initialize", json!({}))
                .await
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < mcp_deadline,
                "Slint MCP server on port {} never became reachable",
                harness.mcp_port
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        harness
    }

    async fn try_mcp_call(&self, method: &str, params: Value) -> Option<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp = self
            .client
            .post(format!("http://127.0.0.1:{}/mcp", self.mcp_port))
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    async fn mcp_call(&self, method: &str, params: Value) -> Value {
        self.try_mcp_call(method, params)
            .await
            .unwrap_or_else(|| panic!("MCP call {method} failed"))
    }

    async fn tool_call(&self, name: &str, arguments: Value) -> Value {
        let resp = self
            .mcp_call("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("tool {name} returned no result: {resp}"));
        // MCP tool results wrap the real JSON payload as a serialized
        // string inside content[0].text (standard MCP tool-result shape).
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool {name} result missing content[0].text: {result}"));
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("tool {name} result not JSON ({e}): {text}"))
    }

    async fn window_handle(&self) -> Value {
        let windows = self.tool_call("list_windows", json!({})).await;
        windows["windowHandles"][0].clone()
    }

    /// Flat element list for the whole tree, generous enough for this
    /// app's real element count -- `get_element_tree`'s own default cap
    /// (200) truncates well before this UI's real size.
    async fn element_tree(&self, window_handle: &Value) -> Vec<Value> {
        let root_handle = self
            .tool_call("get_window_properties", json!({"windowHandle": window_handle}))
            .await["rootElementHandle"]
            .clone();
        let tree = self
            .tool_call(
                "get_element_tree",
                json!({"elementHandle": root_handle, "maxElements": 4000}),
            )
            .await;
        tree["elements"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    async fn find_by_label_prefix(&self, window_handle: &Value, prefix: &str) -> Option<Value> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .find(|e| {
                e["accessibleLabel"]
                    .as_str()
                    .is_some_and(|l| l.starts_with(prefix))
            })
    }

    async fn find_by_exact_label(&self, window_handle: &Value, label: &str) -> Option<Value> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .find(|e| e["accessibleLabel"].as_str() == Some(label))
    }

    /// Waits for an element with this exact accessible label, then invokes
    /// its default accessibility action. Re-finds fresh each attempt
    /// (handles for for-loop children go stale under continuous poll).
    /// Tree-walk lookup is used because it is what successfully drives
    /// stable chrome (Expand/Compose/Send); for-loop row actions may still
    /// race handle validity -- callers that only need existence should use
    /// `find_by_label_prefix` / `find_by_exact_label` instead of clicking.
    async fn click_by_exact_label(&self, window_handle: &Value, label: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let element = wait_for(Duration::from_secs(30), || async {
                self.find_by_exact_label(window_handle, label).await
            })
            .await;
            let resp = self
                .try_mcp_call(
                    "tools/call",
                    json!({
                        "name": "invoke_accessibility_action",
                        "arguments": {"elementHandle": element["handle"], "action": "Default_"},
                    }),
                )
                .await;
            let destroyed = resp
                .as_ref()
                .is_some_and(|r| r["result"]["isError"].as_bool().unwrap_or(false));
            if !destroyed {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "element {label:?} kept getting destroyed between lookup and invoke: {resp:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Polls `probe` (a fresh async call each attempt) until it returns
/// `Some`, bounded by `timeout` -- shared shape for "wait for the real
/// UI to settle after a real dispatch" across this file's assertions.
async fn wait_for<F, Fut, T>(timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "condition never became true within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Live MCP existence check for selected-row lifecycle controls
/// (rename/close/archive). Headless coverage of the full arm/confirm
/// close/delete round-trip and archive click lives in
/// `slint_component_e2e_test` (after settling the sidebar expand
/// animation). This live test only proves the labels appear on the
/// real compiled shotcut UI.
///
/// Cold start selects `DEFAULT_THREAD_NAMES[0]` ("Fix timeline crash").
/// After expanding the sidebar, production reveals rename/close/delete/
/// archive for the selected row (`|| i == selected-thread`) without
/// hover.
///
/// Full arm/cancel/confirm click round-trip is intentionally not driven
/// here: MCP handles for for-loop `IconButton` children go stale under
/// continuous `panel_rust_poll` (confirmed via
/// `debug_watch_thread_row_churn` + live probes -- generation stays 1
/// while arena index advances every tree walk; invoke reports
/// "element that was destroyed").
#[tokio::test]
async fn sidebar_close_arm_control_exists_on_the_real_compiled_ui() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;

    let close_arm = wait_for(Duration::from_secs(10), || async {
        harness.find_by_label_prefix(&window, "Close thread ").await
    })
    .await;
    let thread_label = close_arm["accessibleLabel"]
        .as_str()
        .expect("close-arm element has an accessible label")
        .to_string();
    assert!(
        thread_label.starts_with("Close thread "),
        "selected seed thread must expose a Close thread control, got {thread_label:?}"
    );
    // Also present: rename + archive arms for the same selected row.
    let rename = wait_for(Duration::from_secs(5), || async {
        harness.find_by_label_prefix(&window, "Rename thread ").await
    })
    .await;
    assert!(
        rename["accessibleLabel"]
            .as_str()
            .is_some_and(|l| l.starts_with("Rename thread ")),
        "selected seed thread must also expose a Rename thread control"
    );
    let archive = wait_for(Duration::from_secs(5), || async {
        harness.find_by_label_prefix(&window, "Archive thread ").await
    })
    .await;
    assert!(
        archive["accessibleLabel"]
            .as_str()
            .is_some_and(|l| l.starts_with("Archive thread ")),
        "selected seed thread must also expose an Archive thread control"
    );
}

#[tokio::test]
#[ignore]
async fn debug_watch_thread_row_churn() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    let mut log = String::new();
    // Phase 1: 10 polls with ZERO interaction, sidebar still collapsed --
    // isolates whether churn happens purely from background/poll activity.
    for i in 0..10 {
        let tree = harness.element_tree(&window).await;
        let row = tree.iter().find(|e| e["accessibleLabel"].as_str() == Some("Fix timeline crash"));
        log.push_str(&format!("phase1 tick {i}: handle={:?}\n", row.map(|e| e["handle"].clone())));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    log.push_str("--- expanding sidebar ---\n");
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;

    // Phase 2: 20 polls right after expanding, still no thread-row click.
    for i in 0..20 {
        let tree = harness.element_tree(&window).await;
        let row = tree.iter().find(|e| e["accessibleLabel"].as_str() == Some("Fix timeline crash"));
        log.push_str(&format!("phase2 tick {i}: handle={:?}\n", row.map(|e| e["handle"].clone())));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    std::fs::write("/tmp/mcp_thread_row_churn_log.txt", log).unwrap();
}
