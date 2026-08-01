//! Real, live, end-to-end grok-acp (`grok-build`) agent-turn coverage
//! against the actual compiled editor, driven through Slint's own MCP
//! UI-testing server -- same harness shape as `slint_mcp_live_ui_e2e_test.rs`
//! and `slint_mcp_acp_provider_matrix_e2e_test.rs`, but deliberately NOT
//! backed by `rui-mock-agent`. This spawns a real `acpx-server` (no
//! `ACPX_BACKEND_CMD` override, no mock registration) and configures the
//! app's `default_agent_id` to the real, server-self-seeded `grok-build`
//! profile (`Router::ensure_default_profiles_seeded` auto-creates one
//! profile per installed registry agent, named after the agent's own id --
//! see `update.rs`'s own comment on this), so the router launches and
//! authenticates the real, registry-driven `grok-build` npx distribution
//! exactly the way production does: `Router::ensure_registry_loaded`
//! fetching the live agent registry, then `npx_spawn_spec` building
//! `npx -y @xai-official/grok ...` (already installed under
//! `~/.acpx/adapters/grok-build` on hosts that have run it once) and
//! handing it whatever ambient credentials this process's own
//! environment/`~/.grok` already carries.
//!
//! This exists to close the gap the mock-backed provider matrix
//! (`slint_mcp_acp_provider_matrix_e2e_test.rs`) explicitly does not cover:
//! that file's own doc comment says it proves grok-build "looks like it
//! goes through" a real UI click-through while the actual reply comes from
//! the mock/misrouted-codex backend, not a real grok conversation. This
//! file is the real thing -- a real ACP `initialize`/`authenticate`
//! handshake against the real adapter, a real `session/new`, and 2-3 real
//! `session/prompt` turns, asserting the actual reply text lands in the
//! real compiled chat view.
//!
//! Session context this test exists to lock in: earlier work on this branch
//! found and fixed (1) `Router::ensure_registry_loaded` permanently caching
//! a 3-entry offline fallback registry (which does not include
//! `grok-build`) after a single failed live-registry fetch, so grok-build
//! could silently vanish from the catalog forever after one transient
//! network hiccup, and (2) the real ACP `initialize`/`authenticate`
//! handshake for `grok-build` requiring its `terminal` capability to be
//! sent as a bare `bool`, not an object, per the real
//! `agent-client-protocol-schema` crate. Both fixes are meaningless without
//! a live test that actually drives the real handshake+turn path they
//! protect -- this is that test.
//!
//! Deliberately configures the thread's profile through the app's
//! `default_agent_id` setting (`settings.global.json`, same persona-wiring
//! shape `LiveUiHarness::spawn_with_env` uses in
//! `slint_mcp_live_ui_e2e_test.rs`), rather than driving the compose bar's
//! Provider dropdown the way `slint_mcp_acp_provider_matrix_e2e_test.rs`
//! does. That dropdown path goes through `agent_bridge.rs`'s
//! `normalize_provider`, which is separately known (see that file's own
//! doc comment and `agent_bridge.rs`'s
//! `grok_build_thread_silently_shares_the_codex_gateway_not_a_distinct_
//! backend` unit test) to silently alias an explicitly-dropdown-selected
//! `grok-build` thread onto the shared codex gateway instead of routing it
//! to its own backend. Going through the default-profile cold-start path
//! instead sidesteps that separate, already-documented bug so this test's
//! signal is specifically "does a real grok-acp turn work end to end",
//! not "does the dropdown's provider-normalization bug also happen to
//! still misroute it" -- those are two different concerns and conflating
//! them here would make a real handshake regression and a known unrelated
//! routing bug indistinguishable from a single red test.
//!
//! Also deliberately reuses the server's own self-seeded `grok-build`
//! profile rather than creating a fresh custom-named one from this test
//! process. Live investigation while building this test found a second,
//! real, previously-undocumented bug: a profile created from a *second*
//! connection immediately before spawning the real UI can still lose a
//! race against the cold-start "Chat" thread's own very first auto-attach
//! attempt (`no profile named <name>`) -- and panel-rust's session lease
//! pool caches that first failure and never retries, permanently
//! poisoning every later attempt for that profile name for the rest of
//! the process's lifetime (confirmed live: a subsequent, explicitly
//! created thread got the identical cached error without a new
//! `session/new` ever reaching `acpx-server` again). The server's own
//! self-seeded profile does not have that race -- it exists before this
//! test process ever issues its first request -- which is why this test
//! uses it instead of `profiles/create`. That lease-pool bug is real and
//! reproducible but out of scope for this test to fix; it affects any
//! freshly-created profile raced against cold start, not anything
//! grok-build-specific.
//!
//! **Real, ambient-auth, network-dependent, and deliberately NOT run by
//! default** -- matches this repo's own convention for real-backend tests
//! (see `acpx/acpx-server/tests/real_ambient_multi_agent_test.rs` and
//! `acpx/TEST_REPORT.md`'s "External Verification" section): `#[ignore]`d,
//! opt-in via `PANEL_MCP_E2E_LIVE_GROK=1`, and the whole multi-turn body is
//! wrapped in a single bounded `tokio::time::timeout` (mirrors
//! `TEST_REPORT.md`'s "the real Codex ambient test is intentionally bounded
//! to 120 seconds" precedent) so a real adapter hang can never wedge CI.
//! Run it explicitly:
//!
//! ```bash
//! PANEL_MCP_E2E_LIVE_GROK=1 cargo test -p panel-rust \
//!   --test slint_mcp_live_grok_acp_e2e_test \
//!   live_grok_acp_multi_turn_conversation_renders_real_replies \
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

/// Same resolution order as `slint_mcp_live_ui_e2e_test.rs`'s `shotcut_bin`.
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn free_x_display() -> u32 {
    let mut display = 400;
    while std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
        display += 1;
    }
    display
}

/// Real Xvfb + real (unmocked) `acpx-server` + real compiled editor, driven
/// over Slint's MCP JSON-RPC surface -- same shape as
/// `slint_mcp_live_ui_e2e_test.rs::LiveUiHarness`, but with no
/// `ACPX_BACKEND_CMD` override and no mock-agent registration at all: the
/// gateway's own registry-driven `npx_spawn_spec` launches the real
/// `grok-build` adapter.
struct LiveGrokHarness {
    xvfb: Child,
    acpx_server: Child,
    shotcut: Child,
    state_dir: PathBuf,
    mcp_port: u16,
    client: reqwest::Client,
}

impl Drop for LiveGrokHarness {
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

impl LiveGrokHarness {
    async fn spawn() -> Self {
        for binary in [acpx_server_bin(), shotcut_bin()] {
            assert!(
                binary.exists(),
                "required binary missing, build it first: {}",
                binary.display()
            );
        }

        let state_dir = std::env::temp_dir().join(format!(
            "panel-slint-mcp-live-grok-{}-{}",
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
        eprintln!("[live-grok-harness] state_dir={}", state_dir.display());

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
        // Deliberately no ACPX_BACKEND_CMD / mock registration, and no
        // ACPX_DEFAULT_AGENT_ID override either: the panel-side
        // `default_agent_id` setting written below is what actually
        // selects grok-build (via the server's own self-seeded
        // `grok-build` profile); acpx-server's own default-agent
        // supervisor entry (codex-acp) is harmless and unused by this
        // test.
        let acpx_log_level =
            std::env::var("PANEL_MCP_E2E_ACPX_LOG").unwrap_or_else(|_| "info".to_owned());
        let acpx_server = Command::new(acpx_server_bin())
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{gateway_port}"))
            .env("ACPX_DB_PATH", state_dir.join("acpx/gateway.sqlite3"))
            .env("RUST_LOG", acpx_log_level)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(state_dir.join("acpx.stdout.log")).unwrap())
            .stderr(std::fs::File::create(state_dir.join("acpx.stderr.log")).unwrap())
            .spawn()
            .expect("spawn real acpx-server binary");

        // Unlike the mock-backed harnesses, this real `acpx-server` also
        // pre-registers a `default_agent_id` supervisor entry
        // (real `codex-acp` npx distribution) at startup and runs real
        // startup session recovery before its HTTP listener binds --
        // observed live to take several seconds longer than the
        // mock-backed harnesses' near-instant startup, so this deadline is
        // deliberately wider than `slint_mcp_live_ui_e2e_test.rs`'s 5s.
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

        // Confirms the real, self-seeded `grok-build` profile
        // (`Router::ensure_default_profiles_seeded`) is present server-side
        // before the UI ever launches -- this is what proves
        // `Router::ensure_registry_loaded`'s live-registry fetch found
        // `grok-build` at all (the bug this session fixed in commit
        // `3c19a52a` was this permanently falling back to a 3-entry
        // offline registry that never contains `grok-build`). See this
        // file's module doc comment for why this test reuses that
        // self-seeded profile instead of creating its own.
        let base_url = format!("http://127.0.0.1:{gateway_port}");
        let persona = "grok-build";
        let handle = panel_rust::gateway_actor::spawn_acpx_thread(base_url);
        let listed = handle.list_profiles().await;
        eprintln!("[debug] profiles/list right after acpx health: {listed:?}");
        assert!(
            listed.as_ref().is_ok_and(|profiles| profiles
                .iter()
                .any(|p| p.name == "grok-build" && p.agent_id == "grok-build")),
            "expected the server's self-seeded grok-build profile to already exist \
             (Router::ensure_default_profiles_seeded / ensure_registry_loaded), got: {listed:?}"
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
            .args(["--appdata", state_dir.join("shotcut").to_str().unwrap(), "--noupgrade"])
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

        let harness = LiveGrokHarness {
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
            .tool_call("get_window_properties", json!({"windowHandle": window_handle}))
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

    async fn click_element(&self, handle: &Value) {
        self.tool_call(
            "click_element",
            json!({"elementHandle": handle, "action": "SingleClick"}),
        )
        .await;
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

    /// Creates a real thread via the real "New thread" control (cold start
    /// has no thread at all -- there is nothing bound to `default_agent_id`
    /// until one is created; see `update.rs`'s
    /// `new_thread_with_no_default_profile_falls_back_to_default_agent_id_
    /// as_the_profile` unit test for the fallback this relies on). Mirrors
    /// `slint_mcp_live_ui_e2e_test.rs`'s `create_new_thread` /
    /// `slint_mcp_acp_provider_matrix_e2e_test.rs`'s
    /// `select_provider_for_new_thread` sidebar sequencing.
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

    /// Real send through the real compose bar: click to focus, set the
    /// value, submit with a real Return keypress (mirrors
    /// `LiveUiHarness::send_via_compose` in `slint_mcp_live_ui_e2e_test.rs`
    /// -- `dispatch_key_event`'s Return routes to current keyboard focus,
    /// `set_element_value` alone does not submit).
    async fn send_via_compose(&self, window_handle: &Value, text: &str) {
        let compose = wait_for(Duration::from_secs(15), || async {
            self.find_by_exact_label(window_handle, "Compose message").await
        })
        .await;
        self.click_element(&compose["handle"]).await;
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

/// Drives 3 real grok-acp turns through the real compiled UI and verifies
/// each produces a real, rendered reply.
///
/// Step by step:
/// 1. Spawn a real Xvfb + real `acpx-server` (no mock backend anywhere) +
///    real compiled `shotcut`/`snapflow`, with `default_agent_id` set to
///    the server's own self-seeded `grok-build` profile, so a newly
///    created thread is already grok-acp-backed -- equivalent to a user
///    opening/selecting a thread already configured for the grok-build
///    provider.
/// 2. Wait for the real compose box, then send 3 distinct prompts one at a
///    time through it (real click + real value + real Return keypress),
///    waiting after each for that specific turn's reply to actually render
///    (a new `"Copy message text"` control appears -- one per completed
///    agent message) before sending the next, so this is 3 real sequential
///    turns, not one prompt fired 3 times without waiting.
/// 3. After each turn: assert no `"Dismiss error"` control ever appears
///    (the real, visible signal for `RouterError::BackendRequiresAuthentication`
///    / `BackendInitializationError` / any other send-failure banner --
///    see `chat_area.slint`'s `last-error` banner) and that the initial
///    empty-conversation placeholder is gone.
/// 4. Final assertion: exactly 3 `"Copy message text"` controls exist,
///    proving 3 distinct real agent replies rendered, not 1 reply reused
///    or a stuck/duplicated bubble.
#[tokio::test]
#[ignore = "real, ambient-auth, network-dependent grok-acp conversation -- opt in with PANEL_MCP_E2E_LIVE_GROK=1, see this file's module doc comment"]
async fn live_grok_acp_multi_turn_conversation_renders_real_replies() {
    if std::env::var("PANEL_MCP_E2E_LIVE_GROK").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set PANEL_MCP_E2E_LIVE_GROK=1 to run this test against a real, \
             ambient-auth grok-build adapter (requires ~/.acpx/adapters/grok-build or a \
             fresh npx fetch of @xai-official/grok, real network access to the live agent \
             registry, and pre-established grok ambient auth on this host)"
        );
        return;
    }

    // Bounds the *entire* multi-turn body -- a real hung adapter (auth
    // prompt stuck waiting on a TTY that doesn't exist headlessly, a dead
    // network call, etc.) fails this test loudly within a fixed budget
    // instead of ever wedging a CI runner. 420s: `TEST_REPORT.md`'s real
    // Codex ambient test bounds a *single* conversation to 120s; this test
    // additionally pays for a real (non-mock) `acpx-server` cold start
    // (observed live to take up to ~45s before its HTTP listener is even
    // up) plus three sequential real turns each individually bounded at
    // 90s (observed live: a real grok-build reply can take signifcantly
    // longer than a mock backend's instant canned reply), so its budget is
    // set higher rather than reused as-is.
    let outcome = tokio::time::timeout(Duration::from_secs(420), async {
        let harness = LiveGrokHarness::spawn().await;
        let window = harness.window_handle().await;
        harness
            .create_new_thread_with_default_profile(&window)
            .await;
        eprintln!("[debug] labels after thread create: {:?}", harness.labels(&window).await);

        let prompts = [
            "diagnostic ping 1: reply with the single word ack",
            "diagnostic ping 2: what is 2 + 2? answer with just the number",
            "diagnostic ping 3: reply with the single word done",
        ];

        for (turn_index, prompt) in prompts.iter().enumerate() {
            let expected_reply_count = turn_index + 1;
            harness.send_via_compose(&window, prompt).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            eprintln!(
                "[debug] labels 2s after send turn {}: {:?}",
                turn_index + 1,
                harness.labels(&window).await
            );

            // Wait for this turn's reply to actually render: the Nth
            // "Copy message text" control appearing is the real signal a
            // real agent message (not just an echoed user bubble) landed
            // in the transcript for this specific turn.
            wait_for(Duration::from_secs(90), || async {
                let count = harness
                    .element_tree(&window)
                    .await
                    .into_iter()
                    .filter(|e| e["accessibleLabel"].as_str() == Some("Copy message text"))
                    .count();
                (count >= expected_reply_count).then_some(())
            })
            .await;

            // Real failure signal: the send/session-attach error banner
            // (`chat_area.slint`'s `last-error` block, surfaced whenever
            // `RouterError::BackendRequiresAuthentication`,
            // `BackendInitializationError`, or any other real send failure
            // happens) must never appear after a turn we just confirmed
            // rendered a reply.
            let error_banner = harness.find_by_exact_label(&window, "Dismiss error").await;
            assert!(
                error_banner.is_none(),
                "turn {} ({prompt:?}) rendered a reply alongside a real error banner -- \
                 labels: {:?}",
                turn_index + 1,
                harness.labels(&window).await
            );
        }

        // The cold-start empty-conversation placeholder must be long gone
        // by now -- a real send/reply cycle replaces it.
        assert!(
            harness
                .find_by_exact_label(&window, "Send a message to start the conversation")
                .await
                .is_none(),
            "empty-conversation placeholder still present after 3 real turns"
        );

        // Exactly 3 distinct rendered replies, one per real turn -- not
        // fewer (a stuck/dropped turn) and not more (a duplicated bubble).
        let final_reply_count = harness
            .element_tree(&window)
            .await
            .into_iter()
            .filter(|e| e["accessibleLabel"].as_str() == Some("Copy message text"))
            .count();
        assert_eq!(
            final_reply_count,
            prompts.len(),
            "expected exactly {} rendered real grok-acp replies, got {final_reply_count}",
            prompts.len()
        );
    })
    .await;

    assert!(
        outcome.is_ok(),
        "live grok-acp multi-turn conversation did not complete within the 420s bound -- \
         likely a real adapter hang (stuck ambient auth prompt, dead network call) rather \
         than a UI regression; re-run with PANEL_MCP_E2E_KEEP_STATE=1 to inspect \
         shotcut.stderr.log / acpx.stderr.log in the preserved state_dir"
    );
}
