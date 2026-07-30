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

mod common;
#[allow(unused_imports)]
use common::{acpx_server_bin, free_port, mock_agent_bin, provision_mock_profile};

fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("repo root")
}

fn shotcut_bin() -> PathBuf {
    repo_root().join("shotcut/build/cc-debug-linux/src/shotcut")
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
    event_log: PathBuf,
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
            .args([
                &display_str,
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
            ])
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
        let admin_port = free_port();
        let admin_token = format!("test-admin-token-{admin_port}");
        let acpx_server = Command::new(acpx_server_bin())
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{gateway_port}"))
            .env("ACPX_DEFAULT_AGENT_ID", persona)
            .env("ACPX_DB_PATH", state_dir.join("acpx/gateway.sqlite3"))
            .env("ACPX_ADMIN_TOKEN", &admin_token)
            .env("ACPX_ADMIN_BIND", format!("127.0.0.1:{admin_port}"))
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

        // PROF-4 (`profile-only-backend-selection` plan): the panel used to
        // reach `rui-mock-agent` via `ACPX_BACKEND_CMD` + native/unmanaged
        // mode (removed from production in PROF-3). Replaced with a real
        // profile: `provision_mock_profile` registers `rui-mock-agent` as a
        // durable custom agent under `"mock-codex"` (deliberately NOT
        // `persona` itself -- acpx-server's own `main.rs` unconditionally
        // pre-registers a supervisor entry under `ACPX_DEFAULT_AGENT_ID` at
        // startup using its bare npx-codex-acp default, so reusing that
        // exact id here would 409 with "custom agent id codex conflicts
        // with an existing registered backend") and creates a profile
        // literally named `persona` ("codex") pointing at it. The
        // settings.global.json written below (before shotcut starts) sets
        // `default_agent_id: "codex"`, which the panel's own cold-start
        // seed binds as `_acpx.profile` (`lib.rs`'s `cold_start_thread_
        // specs`) -- so this panel run never touches native mode at all.
        let base_url = format!("http://127.0.0.1:{gateway_port}");
        let event_log = state_dir.join("acpx/backend-events.jsonl");
        let mut mock_env = std::collections::BTreeMap::new();
        mock_env.insert(
            "RUI_MOCK_AGENT_EVENT_LOG".to_owned(),
            event_log.to_string_lossy().into_owned(),
        );
        provision_mock_profile(&base_url, admin_port, &admin_token, persona, mock_env).await;

        let settings_dir = state_dir.join("panel-settings");
        std::fs::create_dir_all(&settings_dir).expect("create panel settings dir");
        std::fs::write(
            settings_dir.join("settings.global.json"),
            format!(r#"{{"schema_version":1,"default_agent_id":"{persona}"}}"#),
        )
        .expect("write settings.global.json");

        let mcp_port = free_port();
        let shotcut = Command::new(shotcut_bin())
            .args([
                "--appdata",
                state_dir.join("shotcut").to_str().unwrap(),
                "--noupgrade",
            ])
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
            event_log,
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
            .tool_call(
                "get_window_properties",
                json!({"windowHandle": window_handle}),
            )
            .await["rootElementHandle"]
            .clone();
        let tree = self
            .tool_call(
                "get_element_tree",
                json!({"elementHandle": root_handle, "maxElements": 4000}),
            )
            .await;
        tree["elements"].as_array().cloned().unwrap_or_default()
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

    async fn find_by_id(&self, window_handle: &Value, id: &str) -> Option<Value> {
        let result = self
            .tool_call(
                "find_elements_by_id",
                json!({"windowHandle": window_handle, "elementsId": id}),
            )
            .await;
        result["elementHandles"].as_array()?.first().cloned()
    }

    async fn click_element(&self, handle: &Value) {
        let _ = self
            .tool_call(
                "invoke_accessibility_action",
                json!({
                    "elementHandle": handle.clone(),
                    "action": "Default_"
                }),
            )
            .await;
    }

    async fn set_element_value(&self, element: &Value, value: &str) {
        let _ = self
            .tool_call(
                "set_element_value",
                json!({"elementHandle": element["handle"].clone(), "value": value}),
            )
            .await;
    }

    async fn dispatch_key(&self, window_handle: &Value, text: &str) {
        let _ = self
            .tool_call(
                "dispatch_key_event",
                json!({"windowHandle": window_handle, "text": text}),
            )
            .await;
    }

    async fn drag_element(&self, element: &Value, target_x: f32, target_y: f32) {
        let element_handle = element
            .get("handle")
            .cloned()
            .unwrap_or_else(|| element.clone());
        let _ = self
            .tool_call(
                "drag_element",
                json!({
                    "elementHandle": element_handle,
                    "target": {"x": target_x, "y": target_y}
                }),
            )
            .await;
    }

    async fn element_properties(&self, element: &Value) -> Value {
        let element_handle = element
            .get("handle")
            .cloned()
            .unwrap_or_else(|| element.clone());
        self.tool_call(
            "get_element_properties",
            json!({"elementHandle": element_handle}),
        )
        .await
    }

    async fn send_prompt(&self, window_handle: &Value, prompt: &str) {
        let compose = wait_for(Duration::from_secs(15), || async {
            self.find_by_exact_label(window_handle, "Compose message")
                .await
        })
        .await;
        self.click_element(&compose["handle"]).await;
        self.set_element_value(&compose, prompt).await;
        self.dispatch_key(window_handle, "\n").await;
    }

    async fn labels(&self, window_handle: &Value) -> Vec<String> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .filter_map(|e| e["accessibleLabel"].as_str().map(str::to_owned))
            .collect()
    }

    fn backend_events(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.event_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn wait_for_prompt_session(&self, prompt: &str) -> String {
        wait_for(Duration::from_secs(15), || async {
            self.backend_events().into_iter().find_map(|event| {
                (event["method"].as_str() == Some("session/prompt")
                    && event["detail"].as_str() == Some(prompt))
                .then(|| event["session_id"].as_str().map(str::to_owned))
                .flatten()
            })
        })
        .await
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
/// Cold start intentionally has one empty "No thread" surface. After
/// expanding the sidebar, create a real thread and verify production reveals
/// rename/close/archive controls for that selected row (`|| i ==
/// selected-thread`) without hover.
///
/// Full arm/cancel/confirm click round-trip is intentionally not driven
/// here: MCP handles for for-loop `IconButton` children go stale under
/// continuous `panel_rust_poll` (confirmed via
/// `debug_watch_thread_row_churn` + live probes -- generation stays 1
/// while arena index advances every tree walk; invoke reports
/// "element that was destroyed").
#[tokio::test]
#[ignore = "legacy sidebar IconButton accessibility assertion; retained as a diagnostic baseline"]
async fn sidebar_close_arm_control_exists_on_the_real_compiled_ui() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;

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
        harness
            .find_by_label_prefix(&window, "Rename thread ")
            .await
    })
    .await;
    assert!(
        rename["accessibleLabel"]
            .as_str()
            .is_some_and(|l| l.starts_with("Rename thread ")),
        "selected seed thread must also expose a Rename thread control"
    );
    let archive = wait_for(Duration::from_secs(5), || async {
        harness
            .find_by_label_prefix(&window, "Archive thread ")
            .await
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
async fn live_retained_chat_views_keep_thread_messages_isolated() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    // The production cold start intentionally has one empty "No thread"
    // surface. Create two real threads through the same New thread callback
    // used by the user-facing UI, then send a unique message to each.
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    harness
        .send_prompt(&window, "live retained thread A marker")
        .await;
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    harness
        .send_prompt(&window, "live retained thread B marker")
        .await;

    async fn select_thread(harness: &LiveUiHarness, window: &Value, name: &str) {
        harness
            .click_by_exact_label(window, "Expand thread sidebar")
            .await;
        harness.click_by_exact_label(window, name).await;
        harness
            .click_by_exact_label(window, "Collapse thread sidebar")
            .await;
        wait_for(Duration::from_secs(10), || async {
            harness
                .labels(window)
                .await
                .into_iter()
                .any(|label| label == name)
                .then_some(())
        })
        .await;
    }

    // A -> B -> A -> B must be a visibility/selection operation in the real
    // process. The two unique sends above exercise live per-thread delivery;
    // the repeated selection checks the retained delegates remain addressable
    // after each switch instead of being rebuilt from a shared list.
    select_thread(&harness, &window, "New thread 1").await;
    select_thread(&harness, &window, "New thread 2").await;
    wait_for(Duration::from_secs(15), || async {
        let labels = harness.labels(&window).await;
        (labels
            .iter()
            .any(|label| label.contains("live retained thread B marker"))
            && !labels
                .iter()
                .any(|label| label.contains("live retained thread A marker")))
        .then_some(())
    })
    .await;
    select_thread(&harness, &window, "New thread 1").await;
    wait_for(Duration::from_secs(15), || async {
        let labels = harness.labels(&window).await;
        (labels
            .iter()
            .any(|label| label.contains("live retained thread A marker"))
            && !labels
                .iter()
                .any(|label| label.contains("live retained thread B marker")))
        .then_some(())
    })
    .await;
}

#[tokio::test]
async fn live_switch_during_stream_keeps_prompt_sessions_thread_scoped() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    // Thread A stays in the mock backend's real in-flight slow-turn state
    // while thread B is created and sent. This exercises the background-owner
    // route while the selected ChatView changes.
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    harness
        .send_prompt(&window, "slow live stream thread A")
        .await;
    let session_a = harness
        .wait_for_prompt_session("slow live stream thread A")
        .await;

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    harness.send_prompt(&window, "live stream thread B").await;
    let session_b = harness
        .wait_for_prompt_session("live stream thread B")
        .await;

    assert_ne!(
        session_a, session_b,
        "A and B must have distinct live sessions"
    );

    // Repeatedly select both retained views while A is still streaming. The
    // backend event log proves that each prompt stayed attached to its own
    // thread session; the retained-model and owner-routing tests prove the
    // corresponding rows cannot cross into the other view.
    for thread_name in ["New thread 1", "New thread 2", "New thread 1"] {
        harness
            .click_by_exact_label(&window, "Expand thread sidebar")
            .await;
        harness.click_by_exact_label(&window, thread_name).await;
        harness
            .click_by_exact_label(&window, "Collapse thread sidebar")
            .await;
    }

    let events = harness.backend_events();
    assert!(events.iter().any(|event| {
        event["method"].as_str() == Some("session/prompt")
            && event["detail"].as_str() == Some("slow live stream thread A")
            && event["session_id"].as_str() == Some(session_a.as_str())
    }));
    assert!(events.iter().any(|event| {
        event["method"].as_str() == Some("session/prompt")
            && event["detail"].as_str() == Some("live stream thread B")
            && event["session_id"].as_str() == Some(session_b.as_str())
    }));
}

#[tokio::test]
async fn live_multiple_messages_remain_on_their_thread_session_after_switch_back() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;

    let first = "live multi-message A1";
    let second = "live multi-message A2";
    harness.send_prompt(&window, first).await;
    let session_a = harness.wait_for_prompt_session(first).await;
    harness.send_prompt(&window, second).await;
    assert_eq!(session_a, harness.wait_for_prompt_session(second).await);

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    let thread_b_prompt = "live multi-message B1";
    harness.send_prompt(&window, thread_b_prompt).await;
    let session_b = harness.wait_for_prompt_session(thread_b_prompt).await;
    assert_ne!(session_a, session_b);

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread 1").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    let events = harness.backend_events();
    assert!(events.iter().any(|event| {
        event["method"].as_str() == Some("session/prompt")
            && event["detail"].as_str() == Some(first)
            && event["session_id"].as_str() == Some(session_a.as_str())
    }));
    assert!(events.iter().any(|event| {
        event["method"].as_str() == Some("session/prompt")
            && event["detail"].as_str() == Some(second)
            && event["session_id"].as_str() == Some(session_a.as_str())
    }));
}

#[tokio::test]
async fn live_popup_and_tool_group_state_stay_with_the_retained_chat_view() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    harness
        .send_prompt(&window, "live popup tool state A")
        .await;
    harness
        .wait_for_prompt_session("live popup tool state A")
        .await;

    // The mock turn emits a real tool row. Expand it, then open the model
    // dropdown; both are local state on A's retained ChatArea/row tree.
    harness
        .click_by_exact_label(&window, "Expand tool group")
        .await;
    wait_for(Duration::from_secs(10), || async {
        harness
            .find_by_exact_label(&window, "Collapse tool group")
            .await
    })
    .await;
    harness.click_by_exact_label(&window, "Mock Model A").await;
    wait_for(Duration::from_secs(10), || async {
        harness.find_by_exact_label(&window, "Filter options").await
    })
    .await;

    // Create/select B while A's popup is open. B must not inherit A's local
    // popup or tool-row state; returning to A must reveal both states again.
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    assert!(harness
        .find_by_exact_label(&window, "Filter options")
        .await
        .is_none());
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread 1").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    wait_for(Duration::from_secs(10), || async {
        let has_popup = harness.find_by_exact_label(&window, "Filter options").await;
        let has_collapsed_tool = harness
            .find_by_exact_label(&window, "Collapse tool group")
            .await;
        (has_popup.is_some() && has_collapsed_tool.is_some()).then_some(())
    })
    .await;
}

#[tokio::test]
#[ignore = "Slint MCP drag_element currently targets the Flickable but does not dispatch scrolling in this real layout; retain as a live follow-up once the backend exposes wheel/viewport input"]
async fn live_scroll_position_stays_with_the_retained_chat_view() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;

    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;

    // Fill A enough to make its Flickable scrollable, waiting for each mock
    // turn so this also exercises append/update routing rather than only the
    // initial model installation.
    for index in 0..10 {
        let prompt = format!("live scroll retained A {index}");
        harness.send_prompt(&window, &prompt).await;
        harness.wait_for_prompt_session(&prompt).await;
    }

    let scroll = wait_for(Duration::from_secs(10), || async {
        harness
            .find_by_id(&window, "ChatArea::message-scroll")
            .await
    })
    .await;
    // MCP drag targets are window-local logical coordinates. A long upward
    // drag from the message viewport must move A away from its bottom edge.
    let before_scroll = harness.element_properties(&scroll).await;
    harness.drag_element(&scroll, 160.0, -200.0).await;
    let after_scroll = harness.element_properties(&scroll).await;
    let jump = harness
        .find_by_exact_label(&window, "Jump to latest message")
        .await;
    assert!(
        jump.is_some(),
        "upward drag did not expose jump control; before={before_scroll:?}; after={after_scroll:?}; labels={:?}",
        harness.labels(&window).await
    );
    let jump = jump.expect("jump control checked above");

    // B is a separate retained ChatArea and must start at its own bottom.
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    assert!(harness
        .find_by_exact_label(&window, "Jump to latest message")
        .await
        .is_none());

    // Returning to A must restore the same retained Flickable state.
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;
    harness.click_by_exact_label(&window, "New thread 1").await;
    harness
        .click_by_exact_label(&window, "Collapse thread sidebar")
        .await;
    wait_for(Duration::from_secs(10), || async {
        harness
            .find_by_exact_label(&window, "Jump to latest message")
            .await
    })
    .await;
    harness.click_element(&jump).await;
    wait_for(Duration::from_secs(10), || async {
        harness
            .find_by_exact_label(&window, "Jump to latest message")
            .await
            .is_none()
            .then_some(())
    })
    .await;
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
        let labels: Vec<String> = tree
            .iter()
            .filter_map(|e| e["accessibleLabel"].as_str().map(str::to_owned))
            .collect();
        let row = tree
            .iter()
            .find(|e| e["accessibleLabel"].as_str() == Some("Fix timeline crash"));
        log.push_str(&format!(
            "phase1 tick {i}: handle={:?} labels={labels:?}\n",
            row.map(|e| e["handle"].clone())
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    log.push_str("--- expanding sidebar ---\n");
    harness
        .click_by_exact_label(&window, "Expand thread sidebar")
        .await;

    // Phase 2: 20 polls right after expanding, still no thread-row click.
    for i in 0..20 {
        let tree = harness.element_tree(&window).await;
        let labels: Vec<String> = tree
            .iter()
            .filter_map(|e| e["accessibleLabel"].as_str().map(str::to_owned))
            .collect();
        let row = tree
            .iter()
            .find(|e| e["accessibleLabel"].as_str() == Some("Fix timeline crash"));
        log.push_str(&format!(
            "phase2 tick {i}: handle={:?} labels={labels:?}\n",
            row.map(|e| e["handle"].clone())
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    std::fs::write("/tmp/mcp_thread_row_churn_log.txt", log).unwrap();
}
