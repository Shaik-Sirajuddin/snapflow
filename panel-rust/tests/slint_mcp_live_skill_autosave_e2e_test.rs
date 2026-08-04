//! Real, live, end-to-end coverage for the skill-editor live-edit-autosave
//! mechanism, driven through Slint's own MCP UI-testing server -- same
//! harness shape as `slint_mcp_live_ui_e2e_test.rs` / `slint_mcp_live_
//! grok_acp_e2e_test.rs` (real Xvfb, real unmocked `acpx-server`, real
//! compiled `shotcut`/`snapflow` binary, no fakes anywhere in the UI/save
//! path itself).
//!
//! Background: `panel-rust/src/effect_executor.rs`'s `Effect::SkillWrite`
//! handler writes the edited skill content straight to
//! `active_skill_md_path` (the real `SKILL.md` file) -- that part has been
//! correct since commit `0a9ac4e7` fixed a double-join `ENOTDIR` bug there.
//! But the SAME handler also schedules a debounced reactive-sync
//! (`schedule_debounced_skill_resync`) that propagates the edit to every
//! enabled agent vendor via `skills_manager_adapter::
//! update_and_resync_edited_skill` / `skills_manager::Manager::
//! register_skill` -- and those all expect a skill DIRECTORY, not the
//! `SKILL.md` FILE. Before this test's fix, `schedule_debounced_skill_
//! resync` was called with `path` itself (the file), a leftover from
//! before commit `60820a2f` split `active_skill_path` (directory) from
//! `active_skill_md_path` (file) -- that commit fixed the direct write
//! call site but never touched this one. The result: `register_skill`'s
//! `source_dir.join("SKILL.md").is_file()` guard fails on a file path,
//! producing `SkillError::MissingSkillMd`, whose Display text is literally
//! "skill source directory has no SKILL.md: <path>" -- matching this
//! test's own module name and the original bug report.
//!
//! This is a genuinely different bug from `0a9ac4e7`: that one broke the
//! primary file write (visible immediately, on every save). This one is a
//! silent background failure -- the file write always succeeds, so the
//! user sees their content saved; only the reactive-sync side effect
//! (propagating the edit to enabled agent vendors) errors out, surfaced
//! only via `dispatch_reactive_sync_failed`'s toast
//! (`EffectResultMsg::SkillReactiveSyncFailed`) and an `eprintln!` to
//! stderr. This test proves the fix (`schedule_debounced_skill_
//! resync(path.parent()...)` instead of `path` itself) by asserting BOTH
//! that the real on-disk `SKILL.md` file gets the edited content (the
//! always-worked half) AND that the reactive-sync failure toast never
//! appears (the newly-fixed half) -- for both a brand-new skill created
//! through the real "New skill" UI flow and a pre-existing skill
//! discovered on disk at startup.
//!
//! **Real, ambient-dependent, and deliberately NOT run by default** --
//! matches this repo's convention for real-backend tests: `#[ignore]`d,
//! opt-in via `PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE=1`, whole body wrapped in
//! a bounded `tokio::time::timeout`.
//!
//! Run it explicitly:
//!
//! ```bash
//! PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE=1 cargo test -p panel-rust \
//!   --test slint_mcp_live_skill_autosave_e2e_test \
//!   -- --ignored --nocapture
//! ```

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

fn free_x_display() -> u32 {
    let mut display = 700;
    while std::path::Path::new(&format!("/tmp/.X11-unix/X{display}")).exists() {
        display += 1;
    }
    display
}

/// Real Xvfb + real (mock-backed) `acpx-server` + real compiled editor,
/// driven over Slint's MCP JSON-RPC surface. Registers one real custom
/// agent + profile (`persona`, matching the other harnesses' "codex"
/// convention) purely so `model.agent_catalog` is non-empty once the app
/// pulls the settings-gateway catalog -- the reactive-sync bug this test
/// covers only fires when at least one vendor_id is enabled
/// (`enabled_vendor_ids` filters `agent_catalog`, and `AgentCatalogEntry::
/// enabled` defaults `true` with no admin token configured client-side,
/// see `protocol_types.rs`'s own doc comment).
struct LiveSkillHarness {
    xvfb: Child,
    acpx_server: Child,
    shotcut: Child,
    state_dir: PathBuf,
    mcp_port: u16,
    client: reqwest::Client,
}

impl Drop for LiveSkillHarness {
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

impl LiveSkillHarness {
    /// `preseed`: optional (skill directory name, SKILL.md content) to
    /// write into the global skills dir BEFORE the app starts, so the
    /// existing-skill-edit scenario discovers a genuinely pre-existing
    /// skill on first scan, not one created through this same test run.
    async fn spawn(preseed: Option<(&str, &str)>) -> (Self, PathBuf) {
        for binary in [mock_agent_bin(), acpx_server_bin(), shotcut_bin()] {
            assert!(
                binary.exists(),
                "required binary missing, build it first: {}",
                binary.display()
            );
        }

        let state_dir = std::env::temp_dir().join(format!(
            "panel-slint-mcp-live-skill-autosave-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(state_dir.join("acpx")).expect("create acpx state dir");
        let cache_dir = state_dir.join("panel");
        std::fs::create_dir_all(&cache_dir).expect("create panel cache dir");
        std::fs::create_dir_all(state_dir.join("shotcut")).expect("create shotcut appdata dir");

        // Global skills dir -- matches `skills_state::global_skills_dir`'s
        // shape (`<cache_dir>/skills`).
        let global_skills_dir = cache_dir.join("skills");
        std::fs::create_dir_all(&global_skills_dir).expect("create global skills dir");
        let mut preseeded_skill_dir = None;
        if let Some((name, content)) = preseed {
            let skill_dir = global_skills_dir.join(name);
            std::fs::create_dir_all(&skill_dir).expect("create preseeded skill dir");
            std::fs::write(skill_dir.join("SKILL.md"), content).expect("write preseeded SKILL.md");
            preseeded_skill_dir = Some(skill_dir);
        }

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
        eprintln!("[live-skill-harness] state_dir={}", state_dir.display());

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
        let acpx_log_level =
            std::env::var("PANEL_MCP_E2E_ACPX_LOG").unwrap_or_else(|_| "error".to_owned());
        let acpx_server = Command::new(acpx_server_bin())
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{gateway_port}"))
            .env("ACPX_DEFAULT_AGENT_ID", persona)
            .env("ACPX_DB_PATH", state_dir.join("acpx/gateway.sqlite3"))
            .env("ACPX_ADMIN_TOKEN", &admin_token)
            .env("ACPX_ADMIN_BIND", format!("127.0.0.1:{admin_port}"))
            .env("RUST_LOG", acpx_log_level)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(state_dir.join("acpx.stdout.log")).unwrap())
            .stderr(std::fs::File::create(state_dir.join("acpx.stderr.log")).unwrap())
            .spawn()
            .expect("spawn real acpx-server binary");

        let client = reqwest::Client::new();
        let health_deadline = std::time::Instant::now() + Duration::from_secs(10);
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

        // Registers a real custom agent + profile under `persona` so
        // `model.agent_catalog` is non-empty once pulled -- see this
        // struct's own doc comment for why that matters to this test.
        let base_url = format!("http://127.0.0.1:{gateway_port}");
        provision_mock_profile(
            &base_url,
            admin_port,
            &admin_token,
            persona,
            std::collections::BTreeMap::new(),
        )
        .await;

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
            .env("RUI_ACP_CACHE_DIR", &cache_dir)
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

        let harness = LiveSkillHarness {
            xvfb,
            acpx_server,
            shotcut,
            state_dir,
            mcp_port,
            client,
        };

        let mcp_deadline = std::time::Instant::now() + Duration::from_secs(15);
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

        (harness, preseeded_skill_dir.unwrap_or(global_skills_dir))
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

    async fn find_containing_text(&self, window_handle: &Value, needle: &str) -> Option<Value> {
        self.element_tree(window_handle)
            .await
            .into_iter()
            .find(|e| {
                e["accessibleLabel"]
                    .as_str()
                    .is_some_and(|s| s.contains(needle))
                    || e["accessibleValue"]
                        .as_str()
                        .is_some_and(|s| s.contains(needle))
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

    async fn click_element(&self, element: &Value) {
        self.tool_call(
            "click_element",
            json!({"elementHandle": element["handle"].clone()}),
        )
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

    /// Opens the sidebar's skill-browsing mode ("Threads"/"Skills" tab
    /// switch, `sidebar.slint`'s `skill-mode`) -- distinct from the
    /// Settings > Skills view (list + "Make Global" only, no editor).
    async fn open_skill_sidebar(&self, window_handle: &Value) {
        self.click_by_exact_label(window_handle, "Expand thread sidebar")
            .await;
        self.click_by_exact_label(window_handle, "Show skills")
            .await;
    }

    /// Real "New skill" click-through dialog flow -- exercises the
    /// newly-created-skill path (`Effect::CreateSkill` ->
    /// `scaffold_new_skill` -> auto-opens `Effect::OpenSkillEditor`).
    async fn create_skill_via_ui(&self, window_handle: &Value, name: &str) {
        self.click_by_exact_label(window_handle, "New skill").await;
        let field = wait_for(Duration::from_secs(10), || async {
            self.find_by_exact_label(window_handle, "New skill name")
                .await
        })
        .await;
        self.click_element(&field).await;
        self.set_element_value(&field, name).await;
        self.click_by_exact_label(window_handle, "Create").await;
    }

    /// Opens an existing skill row (`"Open skill " + name`,
    /// `sidebar.slint:480`) -- exercises the pre-existing-skill path.
    async fn open_existing_skill_via_ui(&self, window_handle: &Value, name: &str) {
        self.click_by_exact_label(window_handle, &format!("Open skill {name}"))
            .await;
    }

    /// Real live-edit: focuses the skill content editor
    /// (`skill_view.slint`'s `editor-input`, now labeled "Skill content
    /// editor") and types real content through it, firing the real
    /// `edited` signal -> `content-edited` -> `SkillMsg::ContentEdited` ->
    /// `Effect::SkillWrite`, exactly the same path a human typing in the
    /// editor takes. No debounce on the write itself -- only the
    /// reactive-sync side effect is debounced (see `schedule_debounced_
    /// skill_resync`'s doc comment) -- so this alone is the real
    /// "autosave" mechanism, not a separate timer/blur trigger.
    async fn edit_skill_content(&self, window_handle: &Value, new_content: &str) {
        let editor = wait_for(Duration::from_secs(15), || async {
            self.find_by_exact_label(window_handle, "Skill content editor")
                .await
        })
        .await;
        self.click_element(&editor).await;
        self.set_element_value(&editor, new_content).await;
        // A real keystroke after the programmatic value-set -- mirrors
        // `send_via_compose`'s own convention (set_element_value then a
        // real dispatch_key_event) so the TextInput's `edited` callback
        // definitely fires from a real key event, not merely a
        // property write the harness cannot fully distinguish from one.
        self.dispatch_key(window_handle, " ").await;
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

/// Polls the real file on disk until it contains `needle`, bounded by
/// `timeout` -- the real, sufficient signal that the real autosave path
/// (`Effect::SkillWrite`'s `std::fs::write`) genuinely persisted the
/// edit, not a UI-only assertion.
async fn wait_for_file_to_contain(path: &std::path::Path, needle: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if contents.contains(needle) {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} never contained {needle:?} within {timeout:?} (last read: {:?})",
            path.display(),
            std::fs::read_to_string(path)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// New-skill scenario: create a skill through the real "New skill" dialog,
/// type real content into its editor, and confirm BOTH that the real
/// `SKILL.md` on disk genuinely picks up the edit AND that no reactive-sync
/// failure toast (`"Dismiss error"` / the `SkillReactiveSyncFailed` toast)
/// ever appears -- before the fix, autosave of a newly-created skill still
/// wrote the file correctly (0a9ac4e7 already covers that) but the
/// reactive-sync side effect this test targets failed silently in the
/// background with "skill source directory has no SKILL.md", visible only
/// in stderr/a toast most users would miss entirely.
#[tokio::test]
#[ignore = "real, ambient-dependent live skill-autosave coverage -- opt in with \
            PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE=1, see this file's module doc comment"]
async fn live_new_skill_autosave_persists_content_and_reactive_sync_does_not_fail() {
    if std::env::var("PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE=1 to run this test, see this \
             file's module doc comment"
        );
        return;
    }

    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let (harness, global_skills_dir) = LiveSkillHarness::spawn(None).await;
        let window = harness.window_handle().await;

        harness.open_skill_sidebar(&window).await;
        harness
            .create_skill_via_ui(&window, "autosave e2e new skill")
            .await;

        eprintln!(
            "[debug] labels after create: {:?}",
            harness.labels(&window).await
        );

        let edited_marker = "autosave-e2e-new-skill-edit-marker";
        harness.edit_skill_content(&window, edited_marker).await;

        let skill_md_path = global_skills_dir
            .join("autosave-e2e-new-skill")
            .join("SKILL.md");
        wait_for_file_to_contain(&skill_md_path, edited_marker, Duration::from_secs(20)).await;

        // THE REAL REGRESSION ASSERTION: give the debounced reactive-sync
        // (see `schedule_debounced_skill_resync`'s own doc comment on its
        // debounce window) time to fire and settle, then confirm it never
        // surfaced a failure toast. Before the fix, this toast (or the
        // "skill source directory has no SKILL.md" text inside it)
        // reliably appeared here.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let sync_failure = harness
            .find_containing_text(&window, "Skill sync to agent failed")
            .await;
        assert!(
            sync_failure.is_none(),
            "reactive-sync failure toast surfaced in the UI after a real new-skill autosave \
             (exactly the `SkillReactiveSyncFailed` toast `update.rs`'s `EffectResultMsg::\
             SkillReactiveSyncFailed` arm renders): {:?}",
            sync_failure
        );
    })
    .await;

    assert!(
        outcome.is_ok(),
        "live new-skill autosave scenario did not complete within the 180s bound"
    );
}

/// Existing-skill scenario: a skill directory is written to disk BEFORE
/// the app starts (so it's discovered by the real filesystem scan, not
/// created by this test run), opened via the sidebar's existing-skill
/// list, edited, and the same two assertions checked -- covers the "genuine
/// pre-existing skill" half explicitly called out by the original bug
/// report's ambiguity between a new-skill-creation failure and an
/// existing-skill-edit failure.
#[tokio::test]
#[ignore = "real, ambient-dependent live skill-autosave coverage -- opt in with \
            PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE=1, see this file's module doc comment"]
async fn live_existing_skill_autosave_persists_content_and_reactive_sync_does_not_fail() {
    if std::env::var("PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set PANEL_MCP_E2E_LIVE_SKILL_AUTOSAVE=1 to run this test, see this \
             file's module doc comment"
        );
        return;
    }

    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let preseed_content =
            "---\nname: autosave e2e existing skill\ndescription: >-\n  pre-existing skill for the live autosave e2e test.\n---\n\n# Autosave E2E Existing Skill\n\noriginal body.\n";
        let (harness, skill_dir) = LiveSkillHarness::spawn(Some((
            "autosave-e2e-existing-skill",
            preseed_content,
        )))
        .await;
        let window = harness.window_handle().await;

        harness.open_skill_sidebar(&window).await;
        eprintln!(
            "[debug] labels after opening skill sidebar: {:?}",
            harness.labels(&window).await
        );
        harness
            .open_existing_skill_via_ui(&window, "autosave e2e existing skill")
            .await;

        let edited_marker = "autosave-e2e-existing-skill-edit-marker";
        harness.edit_skill_content(&window, edited_marker).await;

        let skill_md_path = skill_dir.join("SKILL.md");
        wait_for_file_to_contain(&skill_md_path, edited_marker, Duration::from_secs(20)).await;

        tokio::time::sleep(Duration::from_secs(3)).await;
        let sync_failure = harness
            .find_containing_text(&window, "Skill sync to agent failed")
            .await;
        assert!(
            sync_failure.is_none(),
            "reactive-sync failure toast surfaced in the UI after a real existing-skill \
             autosave: {:?}",
            sync_failure
        );
    })
    .await;

    assert!(
        outcome.is_ok(),
        "live existing-skill autosave scenario did not complete within the 180s bound"
    );
}
