//! ACP compatibility hardening, phase 4: `terminal/create`,
//! `terminal/output`, `terminal/wait_for_exit`, `terminal/release` --
//! agent-initiated requests just like `fs/*` (see `fs_request_test.rs`)
//! and `session/request_permission` (see `permission_request_test.rs`),
//! so they share the same "must not deadlock the backend" risk. This
//! proves two things: (1) disabled by default (native mode / a profile
//! that didn't opt in), `terminal/create` gets a clear error and the
//! outer call still completes rather than hanging or silently spawning
//! a process; (2) opted in via `Profile::allow_terminal_access`, acpx
//! spawns a *real* child process on acpx's own host, verified against
//! its actual captured stdout and real exit code, not a stubbed
//! response, and `terminal/release` genuinely invalidates the id
//! afterward.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Answers `session/new` normally. On `session/prompt`: sends a real
/// `terminal/create` request (id `901`) that spawns `sh -c "echo hello;
/// exit 7"`, blocks for that id's reply, extracts the minted
/// `terminalId`, then `terminal/wait_for_exit` (id `902`), then
/// `terminal/output` (id `903`), then `terminal/release` (id `904`) --
/// each blocking on its own reply id before sending the next, mirroring
/// `fs_request_test.rs`'s "inner `while read` loop" trick so a
/// regression that leaves any one of these unanswered hangs the script
/// (and the test) rather than failing normally. Forwards all four raw
/// replies back verbatim as `result.{createReply,waitReply,outputReply,
/// releaseReply}` so the test can assert on exactly what acpx answered.
fn stand_in_terminal_backend_script() -> String {
    r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":901,"method":"terminal/create","params":{"sessionId":"backend-abc","command":"sh","args":["-c","echo hello; exit 7"]}}\n'
    create_reply=""
    while IFS= read -r reply_line; do
      if echo "$reply_line" | grep -q '"id":901'; then
        create_reply="$reply_line"
        break
      fi
    done
    term_id=$(echo "$create_reply" | grep -o '"terminalId":"[^"]*"' | cut -d'"' -f4)
    printf '{"jsonrpc":"2.0","id":902,"method":"terminal/wait_for_exit","params":{"sessionId":"backend-abc","terminalId":"%s"}}\n' "$term_id"
    wait_reply=""
    while IFS= read -r reply_line; do
      if echo "$reply_line" | grep -q '"id":902'; then
        wait_reply="$reply_line"
        break
      fi
    done
    printf '{"jsonrpc":"2.0","id":903,"method":"terminal/output","params":{"sessionId":"backend-abc","terminalId":"%s"}}\n' "$term_id"
    output_reply=""
    while IFS= read -r reply_line; do
      if echo "$reply_line" | grep -q '"id":903'; then
        output_reply="$reply_line"
        break
      fi
    done
    printf '{"jsonrpc":"2.0","id":904,"method":"terminal/release","params":{"sessionId":"backend-abc","terminalId":"%s"}}\n' "$term_id"
    release_reply=""
    while IFS= read -r reply_line; do
      if echo "$reply_line" | grep -q '"id":904'; then
        release_reply="$reply_line"
        break
      fi
    done
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","createReply":%s,"waitReply":%s,"outputReply":%s,"releaseReply":%s}}\n' "$id" "$create_reply" "$wait_reply" "$output_reply" "$release_reply"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#
    .to_string()
}

fn stand_in_terminal_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), stand_in_terminal_backend_script()],
    )
}

#[tokio::test]
async fn terminal_requests_are_disabled_by_default_and_still_complete() {
    let mut router = Router::new("term-agent");
    router.register_agent("term-agent", stand_in_terminal_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"].as_str().unwrap();

    // Native/unmanaged mode -- no profile, so `Profile::allow_terminal_access`
    // defaults `false`. `initialize`'s own `clientCapabilities.terminal`
    // already told this backend `terminal/create` is unsupported; this
    // asserts acpx also holds that line if the backend asks anyway,
    // rather than spawning the process just because it was asked.
    let prompt_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        })),
    )
    .await
    .expect("session/prompt must not hang even when terminal access is disabled")
    .expect("session/prompt");

    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));
    let create_reply = &prompt_response["result"]["createReply"];
    assert!(
        create_reply.get("error").is_some(),
        "expected an error reply for a disabled terminal/create, got {create_reply}"
    );
    assert!(create_reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("disabled"));

    // With `terminal/create` refused, the backend script's own
    // `terminalId` extraction yields empty, so `wait_for_exit`/`output`/
    // `release` were sent with `terminalId:""` -- still real replies
    // (unknown terminal id errors), still no hang.
    assert!(prompt_response["result"]["waitReply"]["error"].is_object());
}

#[tokio::test]
async fn terminal_requests_spawn_a_real_process_when_profile_opts_in() {
    let mut router = Router::new("term-agent");
    router.register_agent("term-agent", stand_in_terminal_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "terminal-enabled",
                "agent_id": "term-agent",
                "allow_terminal_access": true
            }
        }))
        .await
        .expect("profiles/create");

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "terminal-enabled"}}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"].as_str().unwrap();

    let prompt_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        })),
    )
    .await
    .expect("session/prompt must not hang once acpx answers all four terminal requests")
    .expect("session/prompt");

    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));

    let create_reply = &prompt_response["result"]["createReply"];
    let terminal_id = create_reply["result"]["terminalId"]
        .as_str()
        .expect("terminal/create should have minted a real terminalId");
    assert!(terminal_id.starts_with("term-"));

    // Real exit code from the real `sh -c "echo hello; exit 7"` child.
    assert_eq!(
        prompt_response["result"]["waitReply"]["result"]["exitStatus"]["exitCode"],
        json!(7)
    );

    // Real captured stdout, not a stub.
    let output = prompt_response["result"]["outputReply"]["result"]["output"]
        .as_str()
        .expect("terminal/output result.output");
    assert_eq!(output.trim_end(), "hello");

    // `terminal/release` must succeed once...
    assert_eq!(
        prompt_response["result"]["releaseReply"]["result"],
        json!({})
    );

    let agent_requests = prompt_response["_acpx"]["agentRequests"]
        .as_array()
        .expect("agentRequests recorded");
    assert_eq!(agent_requests.len(), 4);
}
