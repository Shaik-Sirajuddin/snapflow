//! Real end-to-end proof of the Coverage Matrix's `mcp_servers/*` and
//! `agents/*` rows through `rui-acpx-client`'s actor -- companion to
//! `gateway_e2e_test.rs`/`terminal_relay_e2e_test.rs`, same "spawn the
//! real compiled `acpx-server` binary, don't fake the boundary"
//! discipline. Two things are proven, not just "the call doesn't
//! error":
//!
//! 1. `list_mcp_servers`/`create_mcp_server`/`update_mcp_server`/
//!    `delete_mcp_server` reach the real `acpx-core::McpServerStore`
//!    (list reflects create/update/delete in order -- a client-side
//!    stub could return `Ok(())` for every call and still pass a
//!    weaker test that never re-lists).
//! 2. A profile whose `mcp_servers` field names a centrally-registered
//!    server actually causes that server to reach the backend agent's
//!    own `session/new` request (`acpx-core::mcp_servers::
//!    merge_mcp_servers`'s central-servers-are-additive contract,
//!    already proven at the router-dispatch layer by
//!    `acpx-core/tests/profile_resolution_test.rs`'s
//!    `session_new_with_profile_merges_central_mcp_servers_with_
//!    client_ones_winning` -- this test proves the *same* real
//!    contract is reachable through the full `rui-acpx-client` SDK
//!    path, not just direct in-process `Router::dispatch`).
//! 3. `list_agents`/`agent_status`/`install_agent` reach the real
//!    `acpx-registry` catalogue (fallback-bundled `claude-acp`/
//!    `codex-acp`/`gemini` entries, each carrying a real
//!    `acpx-core::detect::detect` status, not a client-side default).
//! 4. setup-followups plan, e2e_mcp_availability_during_turn: MCP
//!    availability is not an ACP-level concept -- acpx/panel-rust never
//!    mediate the MCP subprocess's lifecycle, only forward the
//!    `mcpServers` config to the backend agent, which spawns and speaks
//!    to it directly. So the only honest, real (not invented) way to
//!    prove "available, then flips to unavailable mid-turn" is to spawn
//!    the exact `snapflowd-mcp` binary `agent_bridge.rs` places in that
//!    config, drive real MCP JSON-RPC (`initialize`/`tools/list`) over
//!    its actual stdio, then kill it and prove the same live connection
//!    now fails -- see
//!    `snapflowd_mcp_availability_flips_when_the_process_is_killed_mid_turn`.

use panel_rust::gateway_actor::spawn_acpx_thread;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

fn acpx_server_bin() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
}

fn snapflowd_mcp_bin() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/snapflowd-mcp")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// Same TOCTOU-safe retry wrapper as the sibling e2e test files (see
/// `gateway_e2e_test.rs`'s copy for the full root-cause doc comment).
fn spawn_acpx_server_with_retry(configure: impl Fn(&mut Command, u16)) -> (Child, String) {
    for attempt in 0..5 {
        let port = free_port();
        let mut command = Command::new(acpx_server_bin());
        configure(&mut command, port);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn real acpx-server binary for test");

        let deadline = std::time::Instant::now() + Duration::from_millis(3000);
        let mut reachable = false;
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                reachable = true;
                break;
            }
            if let Ok(Some(_status)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        if reachable {
            return (child, format!("http://127.0.0.1:{port}"));
        }
        let _ = child.kill();
        let _ = child.wait();
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
    }
    panic!(
        "acpx-server never became reachable after 5 fresh-port attempts -- \
         this looks like more than ordinary port contention"
    );
}

struct GatewayProcess {
    child: Child,
    base_url: String,
}

impl GatewayProcess {
    fn spawn(backend_script: &str, script_dir: &std::path::Path) -> Self {
        let script_path = script_dir.join("stand_in_backend.sh");
        std::fs::write(&script_path, backend_script).expect("write stand-in backend script");
        let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
                .env("ACPX_DEFAULT_AGENT_ID", "mcp-agents-test-agent")
                .env("RUST_LOG", "error");
        });
        GatewayProcess { child, base_url }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A minimal backend that answers every `session/new`/`session/prompt`
/// call, recording (via a shell variable persisted across the loop's
/// own iterations -- this is one long-lived process, not one shell
/// invocation per line) whether the `session/new` request line it saw
/// contained the central MCP server's name, then echoing that fact
/// back as a real `agent_message_chunk` the test can assert on.
const MCP_OBSERVING_BACKEND_SCRIPT: &str = r#"#!/bin/sh
SAW_CENTRAL="false"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    if echo "$line" | grep -q 'central-fs'; then
      SAW_CENTRAL="true"
    fi
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"mcp-observing-session"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"mcp-observing-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"SAW_CENTRAL_FS=%s"}}}}\n' "$SAW_CENTRAL"
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

async fn wait_for_message_containing(
    rx: &mut UnboundedReceiver<panel_rust::protocol_types::AgentEvent>,
    needle: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Some(panel_rust::protocol_types::AgentEvent::Message(msg))) =
            tokio::time::timeout(remaining.min(Duration::from_millis(200)), rx.recv()).await
        {
            if msg.text.contains(needle) {
                return Some(msg.text);
            }
        }
    }
    None
}

#[tokio::test]
async fn mcp_servers_crud_round_trips_through_the_thread_actor() {
    let script_dir = tempfile::tempdir().expect("script tempdir");
    let gateway = GatewayProcess::spawn(MCP_OBSERVING_BACKEND_SCRIPT, script_dir.path());
    let handle = spawn_acpx_thread(gateway.base_url.clone());

    // Starts empty -- no MCP servers registered yet on a fresh gateway.
    let initial = handle.list_mcp_servers().await.expect("list_mcp_servers");
    assert!(initial.is_empty(), "expected no servers yet, got {initial:?}");

    let created = handle
        .create_mcp_server(serde_json::json!({
            "name": "central-fs",
            "command": "mcp-central-fs"
        }))
        .await
        .expect("create_mcp_server");
    assert_eq!(created["name"], "central-fs");

    let after_create = handle.list_mcp_servers().await.expect("list_mcp_servers");
    assert_eq!(after_create.len(), 1);
    assert_eq!(after_create[0].command.as_deref(), Some("mcp-central-fs"));

    handle
        .update_mcp_server(serde_json::json!({
            "name": "central-fs",
            "command": "mcp-central-fs-v2"
        }))
        .await
        .expect("update_mcp_server");
    let after_update = handle.list_mcp_servers().await.expect("list_mcp_servers");
    assert_eq!(after_update.len(), 1);
    assert_eq!(
        after_update[0].command.as_deref(), Some("mcp-central-fs-v2"),
        "expected update to have replaced the entry, not appended a second one"
    );

    handle
        .delete_mcp_server("central-fs")
        .await
        .expect("delete_mcp_server");
    let after_delete = handle.list_mcp_servers().await.expect("list_mcp_servers");
    assert!(
        after_delete.is_empty(),
        "expected the server to be gone after delete, got {after_delete:?}"
    );
}

#[tokio::test]
async fn profile_referencing_a_central_mcp_server_reaches_the_real_backend_session_new() {
    let script_dir = tempfile::tempdir().expect("script tempdir");
    let gateway = GatewayProcess::spawn(MCP_OBSERVING_BACKEND_SCRIPT, script_dir.path());
    let handle = spawn_acpx_thread(gateway.base_url.clone());

    handle
        .create_mcp_server(serde_json::json!({
            "name": "central-fs",
            "command": "mcp-central-fs"
        }))
        .await
        .expect("create_mcp_server");

    // Raw HTTP profiles/create -- `AcpxThreadHandle` has no typed
    // profile-create wrapper yet (settings-gear profile CRUD is a
    // separate, not-yet-started slice of Phase 1's remaining work);
    // same raw-`/rpc` technique `terminal_relay_e2e_test.rs` already
    // uses for profile setup.
    let http_client = reqwest::Client::new();
    let create_profile = http_client
        .post(format!("{}/rpc", gateway.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "mcp-linked",
                "agent_id": "mcp-agents-test-agent",
                "mcp_servers": ["central-fs"]
            }
        }))
        .send()
        .await
        .expect("profiles/create request");
    assert!(create_profile.status().is_success());

    let mut handle = handle;
    let mut events = handle.take_events();
    handle
        .open_session_with_profile(std::env::current_dir().unwrap(), "mcp-linked", Vec::new())
        .await
        .expect("open_session_with_profile");
    handle
        .send_prompt("does the backend see the central mcp server")
        .await
        .expect("send_prompt");

    let reply =
        wait_for_message_containing(&mut events, "SAW_CENTRAL_FS=", Duration::from_secs(10)).await;
    assert_eq!(
        reply.as_deref(),
        Some("SAW_CENTRAL_FS=true"),
        "expected the real backend's own session/new request to have included the \
         profile's central-fs MCP server (merge_mcp_servers's additive contract), got {reply:?}"
    );
}

#[tokio::test]
async fn agent_catalog_list_status_and_install_reach_the_real_registry() {
    let script_dir = tempfile::tempdir().expect("script tempdir");
    let gateway = GatewayProcess::spawn(MCP_OBSERVING_BACKEND_SCRIPT, script_dir.path());
    let handle = spawn_acpx_thread(gateway.base_url.clone());

    // `agents/list` draws from `acpx-registry`'s live-fetch-or-bundled-
    // fallback catalogue -- real known ids from `registry.fallback.json`
    // (`claude-acp`/`codex-acp`/`gemini`), each annotated with a real
    // `acpx-core::detect::detect` status, not a client-side default.
    let agents = handle.list_agents().await.expect("list_agents");
    assert!(
        !agents.is_empty(),
        "expected at least the bundled fallback registry's agents"
    );
    let codex_entry = agents
        .iter()
        .find(|a| a.id == "codex-acp")
        .cloned()
        .expect("expected a codex-acp entry from the registry (live or fallback)");
    assert!(
        !codex_entry.status.as_wire_str().is_empty(),
        "expected each catalogue entry to carry a live detection status, got {codex_entry:?}"
    );

    let status = handle
        .agent_status("codex-acp")
        .await
        .expect("agent_status");
    assert_eq!(status.id, "codex-acp");
    assert!(
        !status.status.as_wire_str().is_empty(),
        "expected agent_status to carry a real detection status, got {status:?}"
    );

    // `agents/install` against an id the registry has never heard of
    // must fail with a real gateway-side error (`UnknownAgentId`), not
    // silently succeed -- proves this reaches the same real registry
    // lookup `agents/status` just did, not a client-side stub that
    // accepts anything.
    let unknown_install = handle.install_agent("definitely-not-a-real-agent-id").await;
    assert!(
        unknown_install.is_err(),
        "expected agents/install against an unknown id to fail with a real gateway error"
    );
}

/// Real end-to-end proof of the Coverage Matrix's `profiles/create/
/// update/delete` row through `AcpxThreadHandle` -- companion to
/// `mcp_servers_crud_round_trips_through_the_thread_actor` above, same
/// "list reflects create/update/delete in order" discipline (a client-
/// side stub that always returned `Ok(())` without re-listing would
/// pass a weaker assertion set that never checked the list).
#[tokio::test]
async fn profiles_crud_round_trips_through_the_thread_actor() {
    let script_dir = tempfile::tempdir().expect("script tempdir");
    let gateway = GatewayProcess::spawn(MCP_OBSERVING_BACKEND_SCRIPT, script_dir.path());
    let handle = spawn_acpx_thread(gateway.base_url.clone());

    // Starts with no profiles named after this test's fixture -- a
    // fresh gateway process registers nothing by default.
    let initial = handle.list_profiles().await.expect("list_profiles");
    assert!(
        !initial.iter().any(|p| p.name == "crud-test-profile"),
        "expected no crud-test-profile yet, got {initial:?}"
    );

    let created = handle
        .create_profile(serde_json::json!({
            "name": "crud-test-profile",
            "agent_id": "mcp-agents-test-agent",
            "allow_terminal_access": false,
            "allow_fs_access": false,
        }))
        .await
        .expect("create_profile");
    assert_eq!(created["name"], "crud-test-profile");

    let after_create = handle.list_profiles().await.expect("list_profiles");
    let found = after_create
        .iter()
        .find(|p| p.name == "crud-test-profile")
        .expect("expected the created profile to be listed");
    assert!(!found.allow_terminal_access);
    assert!(!found.allow_fs_access);

    handle
        .update_profile(serde_json::json!({
            "name": "crud-test-profile",
            "agent_id": "mcp-agents-test-agent",
            "allow_terminal_access": true,
            "allow_fs_access": true,
        }))
        .await
        .expect("update_profile");
    let after_update = handle.list_profiles().await.expect("list_profiles");
    let updated = after_update
        .iter()
        .find(|p| p.name == "crud-test-profile")
        .expect("expected the updated profile to still be listed under the same name");
    assert!(
        updated.allow_terminal_access && updated.allow_fs_access,
        "expected update to have replaced the entry's capability flags, got {updated:?}"
    );
    assert_eq!(
        after_update
            .iter()
            .filter(|p| p.name == "crud-test-profile")
            .count(),
        1,
        "expected update to replace the entry, not append a second one"
    );

    handle
        .delete_profile("crud-test-profile")
        .await
        .expect("delete_profile");
    let after_delete = handle.list_profiles().await.expect("list_profiles");
    assert!(
        !after_delete.iter().any(|p| p.name == "crud-test-profile"),
        "expected the profile to be gone after delete, got {after_delete:?}"
    );
}

/// setup-followups plan, e2e_mcp_availability_during_turn: real proof
/// that the exact `snapflowd-mcp` binary `agent_bridge.rs` puts in every
/// session's `mcpServers` array is genuinely available (answers real MCP
/// JSON-RPC over its real stdio, not a stub), and that killing it mid-
/// turn is a real, observable transition to unavailable -- not a status
/// this test invents, since acpx/panel-rust have no mechanism to mediate
/// or report on the MCP subprocess's lifecycle at all (only the backend
/// agent spawns and speaks to it, per `mcpServers` config forwarding
/// already proven by `profile_referencing_a_central_mcp_server_reaches_
/// the_real_backend_session_new` above). A real backend agent facing
/// this same failure would see exactly what this test sees: the pipe
/// goes silent.
#[test]
fn snapflowd_mcp_availability_flips_when_the_process_is_killed_mid_turn() {
    let global_dir = tempfile::tempdir().expect("global dir tempdir");
    std::fs::create_dir_all(global_dir.path().join("release")).expect("skill dir");
    std::fs::write(
        global_dir.path().join("release").join("SKILL.md"),
        "---\nname: release\ndescription: release process\n---\n",
    )
    .expect("write SKILL.md");

    let mut child = Command::new(snapflowd_mcp_bin())
        .arg("--global-dir")
        .arg(global_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn real snapflowd-mcp binary for test");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    let send = |stdin: &mut std::process::ChildStdin, id: i64, method: &str| {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method})
        )
        .and_then(|_| stdin.flush())
    };
    let recv = |stdout: &mut BufReader<std::process::ChildStdout>| -> Option<serde_json::Value> {
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) => None, // EOF -- the process is gone, nothing left to read.
            Ok(_) => serde_json::from_str(line.trim()).ok(),
            Err(_) => None,
        }
    };

    // Available: a real `initialize` round trip against the real binary.
    send(&mut stdin, 1, "initialize").expect("send initialize");
    let init_reply = recv(&mut stdout).expect("initialize reply while the process is alive");
    assert_eq!(
        init_reply["result"]["serverInfo"]["name"], "snapflowd-mcp",
        "expected a real initialize response from the real binary, got {init_reply:?}"
    );

    // Available (functional, not just transport-level): tools/list must
    // report the real tool surface this binary actually implements.
    send(&mut stdin, 2, "tools/list").expect("send tools/list");
    let tools_reply = recv(&mut stdout).expect("tools/list reply while the process is alive");
    let tool_names: Vec<&str> = tools_reply["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tool_names.contains(&"list_skills"),
        "expected the real tool list to include list_skills, got {tool_names:?}"
    );

    // A real mid-turn crash: kill the process the same way an OOM/crash
    // would end it, not a graceful shutdown request.
    child.kill().expect("kill the real snapflowd-mcp process");
    child.wait().expect("wait for the killed process to exit");

    // Unavailable: writing to the now-dead process's stdin either fails
    // outright (broken pipe) or is silently buffered by the OS with no
    // process left to read it -- either way, no further response ever
    // arrives, which `recv` observes as an immediate EOF.
    let _ = send(&mut stdin, 3, "tools/list");
    let post_kill_reply = recv(&mut stdout);
    assert!(
        post_kill_reply.is_none(),
        "expected no reply once the MCP process was killed mid-turn (availability must \
         flip to unavailable), got {post_kill_reply:?}"
    );
}
