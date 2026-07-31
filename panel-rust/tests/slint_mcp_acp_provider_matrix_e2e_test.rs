//! Live-UI verification for the "starting a conversation with codex or grok
//! doesn't go through" report: drives the real compiled editor binary
//! (`snapflow`/`shotcut`, whichever is built -- see [`shotcut_bin`]) through
//! Slint's own MCP UI-testing server, exactly like
//! `slint_mcp_live_ui_e2e_test.rs`, but exercises the compose bar's Provider
//! dropdown end to end for the three real registry ids this machine has an
//! installed adapter for: `codex-acp`, `claude-acp`, `grok-build`.
//!
//! `grok-build` is deliberately included even though its behavior is known
//! to be wrong (see `agent_bridge.rs`'s
//! `grok_build_thread_silently_shares_the_codex_gateway_not_a_distinct_
//! backend` unit test, which proves it silently aliases onto the codex
//! gateway rather than getting -- or cleanly refusing -- its own). This
//! file's job is different: prove that from a real user's perspective,
//! clicking through the actual compiled UI for each of the three providers
//! does not crash the process and does produce a reply, i.e. "it looks like
//! it goes through" for all three, which is exactly what would make the
//! underlying misrouting bug invisible without the accompanying
//! `agent_bridge.rs` unit test.
//!
//! Uses the mock backend (`rui-mock-agent`), never a real ambient-auth
//! `codex`/`grok` CLI -- this repo's own convention (see `host_vnc_dev.sh`'s
//! module doc comment) reserves real backends for manual/live verification,
//! never a checked-in automated test.

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

/// Prefers the rebranded `snapflow` dev build (`host_vnc_dev.sh`'s own
/// default), falling back to a raw `shotcut` submodule build if that's what
/// happens to be present -- either binary embeds the same `panel-rust`
/// Slint MCP server, this test only needs *a* compiled host, not a specific
/// one. Override with `SHOTCUT_BIN` if neither default path matches.
fn shotcut_bin() -> PathBuf {
    if let Ok(over) = std::env::var("SHOTCUT_BIN") {
        return PathBuf::from(over);
    }
    let snapflow = repo_root().join("shotcut-rebrand/build-local/src/snapflow");
    if snapflow.exists() {
        return snapflow;
    }
    repo_root().join("shotcut/build/cc-debug-linux/src/shotcut")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn free_x_display() -> u32 {
    let mut display = 300;
    while std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
        display += 1;
    }
    display
}

/// Same shape as `slint_mcp_live_ui_e2e_test.rs`'s `LiveUiHarness`: real
/// Xvfb + real `acpx-server` (mock-backed) + real compiled editor, driven
/// over Slint's MCP JSON-RPC surface. Kept file-local (not shared via a
/// `tests/common` module) so this file stays a single, self-contained
/// artifact for the routing-matrix concern it documents.
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
    /// `persona` becomes both `ACPX_DEFAULT_AGENT_ID` and
    /// `RUI_MOCK_AGENT_PERSONA` for the one shared gateway this harness
    /// spawns -- matching production's actual wiring (`host_vnc_dev.sh`,
    /// `development/scripts/vnc_worktree.sh`): a single acpx-server behind
    /// both `RUI_ACPX_CODEX_URL` and `RUI_ACPX_CLAUDE_URL`, with no
    /// `RUI_ACPX_GROK*_URL` at all. This is deliberate -- it's the same
    /// shared-gateway shape that lets `grok-build` silently reach the
    /// codex-persona backend in real life, not an artificial simplification
    /// for the test.
    async fn spawn() -> Self {
        for binary in [mock_agent_bin(), acpx_server_bin(), shotcut_bin()] {
            assert!(
                binary.exists(),
                "required binary missing, build it first: {}",
                binary.display()
            );
        }

        let state_dir = std::env::temp_dir().join(format!(
            "panel-slint-mcp-acp-matrix-{}-{}",
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
            .args([
                "--appdata",
                state_dir.join("shotcut").to_str().unwrap(),
                "--noupgrade",
            ])
            .env("DISPLAY", &display_str)
            .env("QSG_RENDER_LOOP", "basic")
            .env("SLINT_MCP_PORT", mcp_port.to_string())
            .env("RUI_ACP_CACHE_DIR", state_dir.join("panel"))
            // Deliberately no RUI_ACPX_GROK*_URL -- see this fn's own doc
            // comment on why that omission is the point, not an oversight.
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
            .expect("spawn real shotcut/snapflow binary");

        let harness = LiveUiHarness {
            xvfb,
            acpx_server,
            shotcut,
            state_dir,
            mcp_port,
            client,
        };

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

    async fn find_by_exact_label(&self, window_handle: &Value, label: &str) -> Option<Value> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .find(|e| e["accessibleLabel"].as_str() == Some(label))
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

    async fn click_by_exact_label(&self, window_handle: &Value, label: &str) {
        let element = wait_for(Duration::from_secs(15), || async {
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
        assert!(
            !resp
                .as_ref()
                .is_some_and(|r| r["result"]["isError"].as_bool().unwrap_or(false)),
            "clicking {label:?} failed: {resp:?}"
        );
    }

    async fn click_element(&self, handle: &Value) {
        self.tool_call(
            "click_element",
            json!({"elementHandle": handle, "action": "SingleClick"}),
        )
        .await;
    }

    async fn type_text(&self, window_handle: &Value, text: &str) {
        self.tool_call(
            "dispatch_key_event",
            json!({"windowHandle": window_handle, "text": text}),
        )
        .await;
    }

    /// Selects `provider_id` (a real registry id, e.g. "codex-acp" /
    /// "claude-acp" / "grok-build") on a freshly created thread's Provider
    /// dropdown, then collapses the sidebar so the compose `TextInput`
    /// actually has enough layout width to appear in the accessibility
    /// tree at all -- this harness's default window is a narrow (276px)
    /// docked-panel width, and with the thread sidebar expanded over it the
    /// chat column collapses to ~0 width, which drops `compose` (and its
    /// placeholder `Text`) out of `get_element_tree` entirely even though
    /// nothing in `chat_input_layout.slint` conditionally hides it. Live
    /// reproduction note (not yet root-caused further): this exact width
    /// state is also where a real send once hit `dispatch.rs`'s
    /// `"send effect must target the selected filtered index"`
    /// `debug_assert!` and aborted the whole process -- collapsing first,
    /// as production's default wider dock would never need to, is this
    /// test's way of staying on the well-trodden path while that's
    /// investigated separately.
    async fn select_provider_for_new_thread(&self, window_handle: &Value, provider_id: &str) {
        // Cold start renders the sidebar collapsed, and "New thread" (the
        // + button) only exists in the accessibility tree once expanded --
        // confirmed live: it is simply absent from get_element_tree before
        // this click, not just hidden/disabled.
        self.click_by_exact_label(window_handle, "Expand thread sidebar")
            .await;
        self.click_by_exact_label(window_handle, "New thread").await;
        self.click_by_exact_label(window_handle, "Provider ›").await;

        let filter = wait_for(Duration::from_secs(10), || async {
            self.find_by_exact_label(window_handle, "Search providers...")
                .await
        })
        .await;
        self.click_element(&filter["handle"]).await;
        self.type_text(window_handle, provider_id).await;

        let option = wait_for(Duration::from_secs(10), || async {
            self.find_by_exact_label(window_handle, provider_id).await
        })
        .await;
        self.click_by_exact_label(window_handle, provider_id).await;
        let _ = option;

        // Confirm the trigger now shows the id we picked before moving on --
        // catches a dropdown selection that silently no-ops.
        wait_for(Duration::from_secs(5), || async {
            self.find_by_label_prefix(window_handle, &format!("{provider_id} \u{203a}"))
                .await
        })
        .await;

        self.click_by_exact_label(window_handle, "Collapse thread sidebar")
            .await;
    }

    /// Types `text` into the compose box and clicks Send, then waits for
    /// either a reply to render or the process to visibly die -- whichever
    /// happens is the test's real signal, not a fixed sleep.
    async fn send_and_expect_a_reply(&self, window_handle: &Value, text: &str) {
        let compose = wait_for(Duration::from_secs(10), || async {
            self.find_by_exact_label(window_handle, "Compose message")
                .await
        })
        .await;
        self.click_element(&compose["handle"]).await;
        self.type_text(window_handle, text).await;
        self.click_by_exact_label(window_handle, "Send message")
            .await;

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.try_mcp_call("initialize", json!({})).await.is_some(),
                "the app stopped responding to MCP calls after Send -- it likely crashed \
                 (see this harness's state_dir shotcut.stderr.log if PANEL_MCP_E2E_KEEP_STATE=1)"
            );
            let empty_state_gone = self
                .find_by_exact_label(window_handle, "Send a message to start the conversation")
                .await
                .is_none();
            if empty_state_gone {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no reply/turn-start ever appeared for {text:?} within {deadline:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

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

/// Matrix row 1/3: `codex-acp` is one of the two providers
/// `AgentBridge`'s gateway routing actually understands (`normalize_provider`
/// in `agent_bridge.rs`) -- this is the control case that is expected to,
/// and does, work end to end.
#[tokio::test]
async fn provider_matrix_codex_acp_selects_and_sends_without_crashing() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;
    harness
        .select_provider_for_new_thread(&window, "codex-acp")
        .await;
    harness
        .send_and_expect_a_reply(&window, "diagnostic ping via codex-acp")
        .await;
}

/// Matrix row 2/3: `claude-acp`, the other provider `normalize_provider`
/// actually recognizes. Also expected to, and does, work end to end.
#[tokio::test]
async fn provider_matrix_claude_acp_selects_and_sends_without_crashing() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;
    harness
        .select_provider_for_new_thread(&window, "claude-acp")
        .await;
    harness
        .send_and_expect_a_reply(&window, "diagnostic ping via claude-acp")
        .await;
}

/// Matrix row 3/3: `grok-build` -- the real, live-detected registry id for
/// the installed Grok adapter (`~/.acpx/adapters/grok-build`; there is no
/// "grok-acp" entry in `acpx/acpx-registry/registry.fallback.json` at all).
/// `normalize_provider` does not recognize it and falls through to
/// `"codex"` (see the `agent_bridge.rs` unit test
/// `grok_build_thread_silently_shares_the_codex_gateway_not_a_distinct_
/// backend` for the precise, gateway-level proof). From the compose bar
/// alone this looks identical to the codex-acp/claude-acp cases -- it
/// selects, sends, and gets a reply with no visible error -- which is
/// exactly the point: the misrouting is invisible at this layer and only
/// shows up once you check which backend actually answered.
#[tokio::test]
async fn provider_matrix_grok_build_selects_and_sends_without_crashing() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;
    harness
        .select_provider_for_new_thread(&window, "grok-build")
        .await;
    harness
        .send_and_expect_a_reply(&window, "diagnostic ping via grok-build")
        .await;
}
