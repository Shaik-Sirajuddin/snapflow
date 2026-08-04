//! Real, live, end-to-end coverage for the `autohand-pre-session-error-hang`
//! fix, driven through Slint's own MCP UI-testing server -- same harness
//! shape as `slint_mcp_live_grok_acp_e2e_test.rs` (real Xvfb + real
//! unmocked `acpx-server` + the real compiled editor, no `rui-mock-agent`
//! anywhere in this path), adapted from "assert a real reply renders" to
//! "assert a real, visible error renders instead of an indefinite Loading
//! hang".
//!
//! Background: the "autohand" agent adapter (package
//! `@autohandai/autohand-acp`, installed under
//! `~/.acpx/adapters/autohand`) requires the separate AutoHand CLI binary,
//! which is commonly not installed. When it's missing, the adapter's own
//! `session/new` handshake fails with a real, specific error --
//! "backend requires authentication before session/new" (confirmed
//! independently of any UI by `acpx/scripts/verify_autohand_acp.sh`,
//! driving the raw adapter process through a real `acpx-server`). Before
//! this fix, that real error never reached the panel UI: `AgentBridge`
//! genuinely pushed a `BridgeEvent::Error` (`agent_bridge.rs`'s
//! `open_session`'s `Err` branch), but the reducer's frame-routing
//! (`update_frame` in `update.rs`) could never resolve which thread it
//! belonged to, because the model's thread row still carried the
//! synthetic `"thread:{index}"` placeholder `ThreadMsg::New` seeds every
//! new thread with -- only a *successful* ACP session binding
//! (`AgentBridge::thread_binding`) ever replaced it with the real durable
//! id, and a pre-session failure never produces one. The thread just sat
//! in `Loading` forever, exactly like the grok-build hang looked before
//! that (separate, already-fixed) bug.
//!
//! The fix (`external_snapshot.rs`'s `hydrate_thread_ids_from_bridge`,
//! see its own doc comment for the full trace) hydrates the model's
//! thread-row id from the bridge's durable *pre-session* identity
//! (`AgentBridge::thread_id`) unconditionally, every frame -- not only on
//! a successful session binding -- so a pre-session failure's error event
//! can always be routed to the thread that produced it.
//!
//! A faster, more targeted regression test for the exact routing
//! invariant (real autohand adapter + real acpx-server, no UI/VNC needed)
//! lives in `panel-rust/src/external_snapshot.rs`'s own `#[cfg(test)]`
//! module -- see
//! `autohand_pre_session_failure_is_never_routed_by_the_pre_fix_hydration_but_is_by_the_real_fix`.
//! This file is the full-stack proof: a real click-through, through the
//! real compiled UI, ending in a real visible error banner.
//!
//! **Real, ambient-install-dependent, and deliberately NOT run by
//! default** -- matches this repo's own convention for real-backend tests
//! (see `slint_mcp_live_grok_acp_e2e_test.rs` and
//! `acpx/TEST_REPORT.md`'s "External Verification" section): `#[ignore]`d,
//! opt-in via `PANEL_MCP_E2E_LIVE_AUTOHAND=1`, and the whole body is
//! wrapped in a single bounded `tokio::time::timeout` so a stuck adapter
//! can never wedge CI. Requires the real AutoHand ACP adapter installed at
//! `~/.acpx/adapters/autohand/node_modules/@autohandai/autohand-acp/dist/index.js`
//! (see `acpx/scripts/verify_autohand_acp.sh`) -- deliberately does NOT
//! require the separate AutoHand CLI binary itself to be installed; the
//! whole point of this test is the case where it is *not*.
//!
//! Run it explicitly:
//!
//! ```bash
//! PANEL_MCP_E2E_LIVE_AUTOHAND=1 cargo test -p panel-rust \
//!   --test slint_mcp_live_autohand_hang_e2e_test \
//!   live_autohand_pre_session_auth_failure_renders_a_real_error_not_a_hang \
//!   -- --ignored --nocapture
//! ```

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;
#[allow(unused_imports)]
use common::acpx_server_bin;

fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("repo root")
}

/// Same resolution order as `slint_mcp_live_grok_acp_e2e_test.rs`'s
/// `shotcut_bin`.
fn shotcut_bin() -> PathBuf {
    if let Ok(path) = std::env::var("PANEL_MCP_E2E_SHOTCUT_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("SNAPFLOW_BIN_OVERRIDE") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }
    let snapflow = repo_root().join("shotcut-rebrand/build-local/src/snapflow");
    if snapflow.exists() {
        return snapflow;
    }
    let shared = repo_root()
        .join("../../shotcut-rebrand/build-local/src/snapflow")
        .canonicalize()
        .ok();
    if let Some(p) = shared {
        if p.exists() {
            return p;
        }
    }
    repo_root().join("shotcut/build/cc-debug-linux/src/shotcut")
}

/// Real, installed AutoHand ACP adapter entry point -- same resolution
/// `acpx/scripts/verify_autohand_acp.sh` uses (`AUTOHAND_ADAPTER_ROOT`
/// override, else `~/.acpx/adapters/autohand`). Deliberately does not
/// require the separate AutoHand CLI binary: the adapter itself detects
/// that binary's absence and reports a real ACP authentication error --
/// exactly the case this test exists to cover.
fn autohand_adapter_entry() -> PathBuf {
    let adapter_root = std::env::var("AUTOHAND_ADAPTER_ROOT").unwrap_or_else(|_| {
        format!(
            "{}/.acpx/adapters/autohand",
            std::env::var("HOME").expect("HOME set")
        )
    });
    PathBuf::from(adapter_root).join("node_modules/@autohandai/autohand-acp/dist/index.js")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn free_x_display() -> u32 {
    let mut display = 500;
    while std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
        display += 1;
    }
    display
}

/// Real Xvfb + real (unmocked) `acpx-server` + real compiled editor, driven
/// over Slint's MCP JSON-RPC surface -- same shape as
/// `slint_mcp_live_grok_acp_e2e_test.rs::LiveGrokHarness`, but the server's
/// own native default backend (`ACPX_DEFAULT_ACP_COMMAND`, legacy alias
/// `ACPX_BACKEND_CMD`) is pinned to the real installed autohand adapter
/// instead of routing through the live agent registry.
struct LiveAutohandHarness {
    xvfb: Child,
    acpx_server: Child,
    shotcut: Child,
    state_dir: PathBuf,
    mcp_port: u16,
    client: reqwest::Client,
}

impl Drop for LiveAutohandHarness {
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

impl LiveAutohandHarness {
    async fn spawn(adapter_entry: &std::path::Path) -> Self {
        for binary in [acpx_server_bin(), shotcut_bin()] {
            assert!(
                binary.exists(),
                "required binary missing, build it first: {}",
                binary.display()
            );
        }

        let state_dir = std::env::temp_dir().join(format!(
            "panel-slint-mcp-live-autohand-{}-{}",
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
        eprintln!("[live-autohand-harness] state_dir={}", state_dir.display());

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
        // Same mechanism `verify_autohand_acp.sh` uses: the server's own
        // native default backend is the real installed autohand adapter,
        // under agent id "autohand" -- `Router::ensure_default_profiles_
        // seeded` self-seeds a matching "autohand" profile at startup
        // (`profile.name == agent.id`, PROF-2), which the panel-side
        // `default_agent_id` setting below selects.
        let acpx_log_level =
            std::env::var("PANEL_MCP_E2E_ACPX_LOG").unwrap_or_else(|_| "info".to_owned());
        let acpx_server = Command::new(acpx_server_bin())
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{gateway_port}"))
            .env("ACPX_DB_PATH", state_dir.join("acpx/gateway.sqlite3"))
            .env("ACPX_DEFAULT_AGENT_ID", "autohand")
            .env(
                "ACPX_DEFAULT_ACP_COMMAND",
                format!("node {}", adapter_entry.display()),
            )
            .env("RUST_LOG", acpx_log_level)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(state_dir.join("acpx.stdout.log")).unwrap())
            .stderr(std::fs::File::create(state_dir.join("acpx.stderr.log")).unwrap())
            .spawn()
            .expect("spawn real acpx-server binary");

        let client = reqwest::Client::new();
        let health_deadline = std::time::Instant::now() + Duration::from_secs(45);
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

        // Confirms the real, self-seeded "autohand" profile exists before
        // the UI ever launches (see this file's module doc comment for
        // why this test doesn't create its own profile -- same lease-pool
        // cold-start race `slint_mcp_live_grok_acp_e2e_test.rs` documents
        // and avoids the same way).
        let base_url = format!("http://127.0.0.1:{gateway_port}");
        let persona = "autohand";
        let handle = panel_rust::gateway_actor::spawn_acpx_thread(base_url);
        let listed = handle.list_profiles().await;
        eprintln!("[debug] profiles/list right after acpx health: {listed:?}");
        assert!(
            listed.as_ref().is_ok_and(|profiles| profiles
                .iter()
                .any(|p| p.name == "autohand" && p.agent_id == "autohand")),
            "expected the server's self-seeded autohand profile to already exist \
             (Router::ensure_default_profiles_seeded), got: {listed:?}"
        );

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
            .env("RUI_PANEL_INPUT_TRACE", "1")
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

        let harness = LiveAutohandHarness {
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

    async fn labels(&self, window_handle: &Value) -> Vec<String> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .filter_map(|e| e["accessibleLabel"].as_str().map(str::to_owned))
            .collect()
    }

    async fn find_by_exact_label(&self, window_handle: &Value, label: &str) -> Option<Value> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .find(|e| e["accessibleLabel"].as_str() == Some(label))
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

    /// Creates a real thread via the real "New thread" control -- same
    /// sequencing as `slint_mcp_live_grok_acp_e2e_test.rs`'s
    /// `create_new_thread_with_default_profile`. This alone is enough to
    /// trigger PUI-014's deferred-attach path; autohand's real auth
    /// failure fires as soon as the first message is sent (see
    /// `send_via_compose` below), not at thread creation.
    async fn create_new_thread_with_default_profile(&self, window_handle: &Value) {
        self.click_by_exact_label(window_handle, "Expand thread sidebar")
            .await;
        self.click_by_exact_label(window_handle, "New thread").await;
        self.click_by_exact_label(window_handle, "Collapse thread sidebar")
            .await;
    }

    async fn set_element_value(&self, element: &Value, value: &str) {
        self.tool_call(
            "set_element_value",
            json!({"elementHandle": element["handle"].clone(), "value": value}),
        )
        .await;
    }

    async fn dispatch_key(&self, window_handle: &Value, text: &str) {
        self.tool_call(
            "dispatch_key_event",
            json!({"windowHandle": window_handle, "text": text}),
        )
        .await;
    }

    async fn send_via_compose(&self, window_handle: &Value, text: &str) {
        let compose = wait_for(Duration::from_secs(15), || async {
            self.find_by_exact_label(window_handle, "Compose message")
                .await
        })
        .await;
        self.tool_call(
            "click_element",
            json!({"elementHandle": compose["handle"].clone(), "action": "SingleClick"}),
        )
        .await;
        self.set_element_value(&compose, text).await;
        self.dispatch_key(window_handle, "\n").await;
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Drives a real autohand thread through the real compiled UI and
/// verifies a real, visible error banner appears within a bounded
/// timeout -- NOT that the thread just silently sits in Loading forever
/// (the pre-fix bug this test is written to catch a regression of).
///
/// Step by step:
/// 1. Spawn a real Xvfb + real `acpx-server` (no mock backend anywhere,
///    native default backend pinned to the real installed autohand
///    adapter) + real compiled `shotcut`/`snapflow`, with
///    `default_agent_id` set to the server's own self-seeded "autohand"
///    profile.
/// 2. Create a new thread (PUI-014 deferred create -- no session opens
///    yet) and send one message through the real compose box, which
///    triggers the real first-send attach
///    (`dispatch_compose_send_maybe_attach` -> `attach_deferred_thread`).
/// 3. Wait for the real, visible error banner (`chat_area.slint`'s
///    `last-error` block, the `"Dismiss error"` control) to appear.
///    Before the fix, this control never appeared -- the real
///    `BridgeEvent::Error` `AgentBridge` pushes for autohand's real
///    "backend requires authentication before session/new" failure got
///    silently dropped by `update_frame`'s routing (see this file's and
///    `external_snapshot.rs`'s module doc comments), and the thread
///    stayed in `Loading` forever instead.
/// 4. Confirms the empty-conversation placeholder or Loading indicator is
///    not still masking the failure (the error banner rendering is the
///    real, sufficient signal either way).
#[tokio::test]
#[ignore = "real, ambient-install-dependent autohand pre-session-failure coverage -- opt in \
            with PANEL_MCP_E2E_LIVE_AUTOHAND=1, see this file's module doc comment"]
async fn live_autohand_pre_session_auth_failure_renders_a_real_error_not_a_hang() {
    if std::env::var("PANEL_MCP_E2E_LIVE_AUTOHAND").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set PANEL_MCP_E2E_LIVE_AUTOHAND=1 to run this test against the real \
             installed autohand adapter (requires \
             ~/.acpx/adapters/autohand/node_modules/@autohandai/autohand-acp/dist/index.js, \
             see acpx/scripts/verify_autohand_acp.sh)"
        );
        return;
    }

    let adapter_entry = autohand_adapter_entry();
    if !adapter_entry.exists() {
        panic!(
            "PANEL_MCP_E2E_LIVE_AUTOHAND=1 was set but the real AutoHand ACP adapter is not \
             installed at {} -- see acpx/scripts/verify_autohand_acp.sh",
            adapter_entry.display()
        );
    }

    // Bounds the entire body -- a real hung adapter, or a real regression
    // of the bug this test covers (an indefinite Loading spinner with no
    // error ever appearing), fails this test loudly within a fixed
    // budget instead of ever wedging a CI runner. Same shape as
    // `slint_mcp_live_grok_acp_e2e_test.rs`'s 420s bound, but this test
    // only needs one turn (not three), so its budget is smaller.
    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let harness = LiveAutohandHarness::spawn(&adapter_entry).await;
        let window = harness.window_handle().await;
        harness
            .create_new_thread_with_default_profile(&window)
            .await;
        eprintln!(
            "[debug] labels after thread create: {:?}",
            harness.labels(&window).await
        );

        harness
            .send_via_compose(&window, "diagnostic ping: are you there?")
            .await;

        // THE REAL ASSERTION: a visible error banner must appear -- not
        // an indefinite Loading state with nothing ever surfacing.
        wait_for(Duration::from_secs(60), || async {
            harness.find_by_exact_label(&window, "Dismiss error").await
        })
        .await;

        eprintln!(
            "[debug] labels once the error banner appeared: {:?}",
            harness.labels(&window).await
        );
    })
    .await;

    assert!(
        outcome.is_ok(),
        "the real autohand pre-session authentication failure never rendered a visible error \
         within the 180s bound -- likely a regression of the exact bug this test exists to \
         catch (the thread stuck silently in Loading instead of surfacing the real \
         `BridgeEvent::Error` `AgentBridge` genuinely produces); re-run with \
         PANEL_MCP_E2E_KEEP_STATE=1 to inspect shotcut.stderr.log / acpx.stderr.log in the \
         preserved state_dir"
    );
}
