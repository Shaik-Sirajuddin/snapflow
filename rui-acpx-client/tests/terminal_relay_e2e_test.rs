//! Real end-to-end proof that `rui-acpx-client`'s actor correctly
//! surfaces the interactive `terminal/create` approval relay
//! (`AgentEvent::PermissionRequest`) and live `acpx/terminal_output`
//! streaming (`AgentEvent::TerminalOutput`) -- the SDK-layer half of
//! the round trip whose gateway-transport half is already proven by
//! `acpx-server/tests/agent_request_fs_terminal_relay_test.rs` and
//! whose `panel-rust` consumer half is proven by
//! `panel-rust/src/agent_bridge.rs`'s
//! `permission_request_relay_round_trips_through_the_bridge` (for
//! `session/request_permission`, which needs no profile capability
//! gate). `terminal/create` specifically needs a profile with
//! `allow_terminal_access: true` -- this is also, therefore, the first
//! real end-to-end proof of [`AcpxThreadHandle::open_session_with_
//! profile`] actually reaching a real gateway's `_acpx.profile`
//! resolution.
//!
//! Same real-binary-spawning discipline as `gateway_e2e_test.rs`
//! (duplicated helpers rather than shared -- these are independent
//! test binaries).

use rui_acpx_client::{spawn_acpx_thread, AgentEvent};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn acpx_server_bin() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

struct GatewayProcess {
    child: Child,
    base_url: String,
}

impl GatewayProcess {
    /// Spawns a real `acpx-server` with `backend_script`'s contents as
    /// its `ACPX_BACKEND_CMD` (written to a temp file first --
    /// `ACPX_BACKEND_CMD` is parsed by naive whitespace-splitting, see
    /// `acpx-server/src/config.rs`, so an inline multi-word script
    /// cannot be passed directly).
    async fn spawn(backend_script: &str, script_dir: &std::path::Path) -> Self {
        let script_path = script_dir.join("stand_in_backend.sh");
        std::fs::write(&script_path, backend_script).expect("write stand-in backend script");
        let port = free_port();
        let child = Command::new(acpx_server_bin())
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
            .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
            .env("ACPX_DEFAULT_AGENT_ID", "terminal-relay-agent")
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn real acpx-server binary");
        let base_url = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        GatewayProcess { child, base_url }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const STAND_IN_TERMINAL_BACKEND_SCRIPT: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":970,"method":"terminal/create","params":{"sessionId":"backend-abc","command":"sh","args":["-c","printf sdk-terminal-output"]}}\n'
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":970' && break
    done
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

#[tokio::test]
async fn terminal_create_relay_and_live_output_reach_the_thread_actor() {
    let script_dir = tempfile::tempdir().expect("script tempdir");
    let gateway = GatewayProcess::spawn(STAND_IN_TERMINAL_BACKEND_SCRIPT, script_dir.path()).await;

    // Create a profile with allow_terminal_access enabled -- terminal/*
    // is gated on Profile::allow_terminal_access (acpx-core::router),
    // unlike session/request_permission which has no capability gate.
    let http_client = reqwest::Client::new();
    let create_profile = http_client
        .post(format!("{}/rpc", gateway.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "terminal-enabled",
                "agent_id": "terminal-relay-agent",
                "allow_terminal_access": true
            }
        }))
        .send()
        .await
        .expect("profiles/create request");
    assert!(create_profile.status().is_success());

    let mut handle = spawn_acpx_thread(gateway.base_url.clone());
    let mut events = handle.take_events();
    handle
        .open_session_with_profile(std::env::current_dir().unwrap(), "terminal-enabled")
        .await
        .expect("open_session_with_profile");

    let prompt = tokio::spawn(async move { handle.send_prompt("start a terminal").await });

    // Answer the relayed terminal/create request the moment it
    // surfaces, exactly as `panel-rust::permission::build_response`
    // would build it for a non-`session/request_permission` method.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut relay_answered = false;
    let mut terminal_id: Option<String> = None;
    let mut last_output = String::new();
    let mut exited = false;
    // Re-borrow the handle's `respond_agent_request` -- needs its own
    // Gateway, built directly here since `handle` was moved into the
    // `prompt` task above. Mirrors `AcpxThreadHandle::respond_agent_
    // request`'s own implementation for this test's purposes.
    let responder = acpx_client::Gateway::connect(gateway.base_url.clone()).await;

    while tokio::time::Instant::now() < deadline && !exited {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining.min(Duration::from_millis(200)), events.recv()).await
        {
            Ok(Some(AgentEvent::PermissionRequest(req))) if !relay_answered => {
                assert_eq!(req.method, "terminal/create");
                let delivered = responder
                    .respond_agent_request(&req.relay_id, serde_json::json!({"approved": true}))
                    .await
                    .expect("respond_agent_request");
                assert!(delivered, "relay hub had no pending request for this relay_id");
                relay_answered = true;
            }
            Ok(Some(AgentEvent::TerminalOutput(ev))) => {
                terminal_id = Some(ev.terminal_id.clone());
                last_output = ev.output.clone();
                exited = ev.exit_status.is_some();
            }
            _ => {}
        }
    }

    assert!(relay_answered, "terminal/create relay never surfaced");
    assert!(exited, "never observed a final TerminalOutput with an exit status");
    assert!(terminal_id.is_some(), "expected a real terminalId on the live push");
    assert!(
        last_output.contains("sdk-terminal-output"),
        "expected the live-streamed output to contain the real command's stdout, got {last_output:?}"
    );

    prompt
        .await
        .expect("prompt task join")
        .expect("send_prompt should complete once the relay is answered");
}
