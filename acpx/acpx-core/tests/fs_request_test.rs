//! ACP compatibility hardening, phase 3: `fs/read_text_file`/
//! `fs/write_text_file` -- agent-initiated requests just like
//! `session/request_permission` (see `permission_request_test.rs`), so
//! they share the same "must not deadlock the backend" risk. This proves
//! two things: (1) disabled by default (native mode / a profile that
//! didn't opt in), the backend gets a clear error and the outer call
//! still completes rather than hanging or silently touching disk; (2)
//! opted in via `Profile::allow_fs_access`, acpx performs *real* disk
//! I/O against acpx's own host filesystem, verified against actual
//! temp files, not a stubbed/mocked response.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Answers `session/new` normally. On `session/prompt`: sends a real
/// `fs/read_text_file` request (id `901`) for `read_path`, blocks until
/// it sees that id's reply, forwards that reply's raw JSON back verbatim
/// as `result.readReply` (so the test can assert on exactly what acpx
/// answered, whether real content or an error); then sends a real
/// `fs/write_text_file` request (id `902`) writing a known string to
/// `write_path`, blocks until it sees that id's reply, and only then
/// answers the original call. Mirrors `permission_request_test.rs`'s
/// "inner `while read` loop" trick for the same reason: a regression
/// that leaves either request unanswered hangs this script (and the
/// test) rather than failing normally.
fn stand_in_fs_backend_script(read_path: &str, write_path: &str) -> String {
    format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{{"jsonrpc":"2.0","id":901,"method":"fs/read_text_file","params":{{"sessionId":"backend-abc","path":"{read_path}"}}}}\n'
    read_reply=""
    while IFS= read -r reply_line; do
      if echo "$reply_line" | grep -q '"id":901'; then
        read_reply="$reply_line"
        break
      fi
    done
    printf '{{"jsonrpc":"2.0","id":902,"method":"fs/write_text_file","params":{{"sessionId":"backend-abc","path":"{write_path}","content":"written-by-backend"}}}}\n'
    while IFS= read -r reply_line2; do
      echo "$reply_line2" | grep -q '"id":902' && break
    done
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn","readReply":%s}}}}\n' "$id" "$read_reply"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#
    )
}

fn stand_in_fs_backend_spec(read_path: &str, write_path: &str) -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            stand_in_fs_backend_script(read_path, write_path),
        ],
    )
}

#[tokio::test]
async fn fs_requests_are_disabled_by_default_and_still_complete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let read_path = dir.path().join("input.txt");
    let write_path = dir.path().join("output.txt");
    std::fs::write(&read_path, "line1\nline2\nline3\n").expect("seed input file");

    let mut router = Router::new("fs-agent");
    router.register_agent(
        "fs-agent",
        stand_in_fs_backend_spec(read_path.to_str().unwrap(), write_path.to_str().unwrap()),
    );

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"].as_str().unwrap();

    // Native/unmanaged mode -- no profile, so `Profile::allow_fs_access`
    // defaults `false`. `initialize`'s own `clientCapabilities.fs` already
    // told this backend both methods are unsupported; this asserts acpx
    // also holds that line if the backend asks anyway, rather than
    // performing the I/O just because it was asked.
    let prompt_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        })),
    )
    .await
    .expect("session/prompt must not hang even when fs access is disabled")
    .expect("session/prompt");

    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));
    let read_reply = &prompt_response["result"]["readReply"];
    assert!(
        read_reply.get("error").is_some(),
        "expected an error reply for a disabled fs/read_text_file, got {read_reply}"
    );
    assert!(read_reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("disabled"));

    // The write must never have actually happened either.
    assert!(!write_path.exists());
}

#[tokio::test]
async fn fs_requests_perform_real_disk_io_when_profile_opts_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let read_path = dir.path().join("input.txt");
    let write_path = dir.path().join("output.txt");
    std::fs::write(&read_path, "line1\nline2\nline3\n").expect("seed input file");

    let mut router = Router::new("fs-agent");
    router.register_agent(
        "fs-agent",
        stand_in_fs_backend_spec(read_path.to_str().unwrap(), write_path.to_str().unwrap()),
    );

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "fs-enabled",
                "agent_id": "fs-agent",
                "allow_fs_access": true
            }
        }))
        .await
        .expect("profiles/create");

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "fs-enabled"}}
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
    .expect("session/prompt must not hang once acpx answers both fs requests")
    .expect("session/prompt");

    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));

    // Real content, read from the real temp file on disk -- not a stub.
    // No `line`/`limit` params were sent, so the file's exact bytes come
    // back untouched (trailing newline included), not the line-windowed
    // path exercised in the unit test below.
    assert_eq!(
        prompt_response["result"]["readReply"]["result"]["content"],
        json!("line1\nline2\nline3\n")
    );

    // Real write, verified by reading the temp file back directly (not
    // through acpx at all) -- proves the write genuinely landed on disk.
    let written = std::fs::read_to_string(&write_path)
        .expect("backend's write_text_file should have created this file");
    assert_eq!(written, "written-by-backend");

    let agent_requests = prompt_response["_acpx"]["agentRequests"]
        .as_array()
        .expect("agentRequests recorded");
    assert_eq!(agent_requests.len(), 2);
}
