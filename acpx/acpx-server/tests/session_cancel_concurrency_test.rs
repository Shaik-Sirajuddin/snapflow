//! ACP compatibility hardening, phase 7's deepest claim, proven for
//! real: `session/cancel` reaches a backend's stdin *while* a
//! `session/prompt` call already in flight against that exact same
//! backend process is still blocked reading its own reply -- not only
//! after that call finishes (at which point cancelling would be moot).
//!
//! Same structural trick as `concurrency_test.rs` (see that file's doc
//! comment for why `#[path]`-including the real transport source is
//! needed here): a stand-in backend sleeps a fixed, generous duration
//! before answering `session/prompt`, standing in for real LLM latency.
//! Unlike `concurrency_test.rs` (which proves two *different* agents run
//! in parallel), this drives `session/prompt` and `session/cancel`
//! against the *same* agent/session concurrently -- the scenario
//! `Supervisor::cancel_writer` exists specifically to make possible,
//! since both calls would otherwise contend for that one process's own
//! per-process lock (see `acpx_core::router::Router::
//! dispatch_session_cancel`'s doc comment for the full rationale, and
//! `BackendProcess::writer`'s for the mechanism).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;
use tokio::sync::Mutex;

#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, SharedRouter};

/// How long the stand-in backend sleeps before answering `session/prompt`.
/// Long enough that "cancel queued up behind the in-flight prompt's
/// per-process lock" (the pre-fix behavior) and "cancel reached the
/// backend independently" are unambiguously distinguishable even under
/// this environment's own scheduling jitter.
const SIMULATED_LLM_LATENCY: Duration = Duration::from_millis(1500);

/// Responds instantly to `session/new`. Sleeps `SIMULATED_LLM_LATENCY`
/// before answering `session/prompt`. Never replies to `session/cancel`
/// at all (real spec behavior -- see `session_cancel_test.rs`'s doc
/// comment) but does append every `session/cancel` line it receives to
/// `capture_path`, so this test can also verify the notification's real
/// shape landed on the backend's stdin, not just that acpx's own
/// response to the client came back quickly.
fn slow_backend_spec_with_cancel_capture(capture_path: &str) -> SpawnSpec {
    let script = format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    sleep {secs}
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/cancel"'; then
    echo "$line" >> {capture_path}
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#,
        secs = SIMULATED_LLM_LATENCY.as_secs_f64()
    );
    SpawnSpec::new("sh", vec!["-c".to_string(), script])
}

/// Same ephemeral-port bring-up helper as `concurrency_test.rs`.
async fn spawn_server(router: SharedRouter) -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        serve(router, addr, None).await.expect("transport::serve");
    });

    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    addr
}

async fn rpc(
    client: &reqwest::Client,
    addr: SocketAddr,
    body: serde_json::Value,
) -> serde_json::Value {
    client
        .post(format!("http://{addr}/rpc"))
        .json(&body)
        .send()
        .await
        .expect("POST /rpc")
        .json::<serde_json::Value>()
        .await
        .expect("json body")
}

fn unique_capture_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "acpx-session-cancel-concurrency-test-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ))
}

#[tokio::test]
async fn session_cancel_reaches_the_backend_while_a_same_agent_prompt_is_still_in_flight() {
    let capture_path = unique_capture_path();
    let mut router = Router::new("agent-a");
    router.register_agent(
        "agent-a",
        slow_backend_spec_with_cancel_capture(capture_path.to_str().unwrap()),
    );
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = reqwest::Client::new();

    let new_response = rpc(
        &client,
        addr,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await;
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // Fire `session/prompt` on its own task -- it'll block for
    // `SIMULATED_LLM_LATENCY` inside the stand-in backend, holding
    // `agent-a`'s per-process lock (`SharedBackendProcess`'s
    // `Arc<Mutex<BackendProcess>>`) for that entire duration.
    let prompt_client = client.clone();
    let prompt_session_id = session_id.clone();
    let prompt_task = tokio::spawn(async move {
        rpc(
            &prompt_client,
            addr,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": prompt_session_id, "prompt": []}
            }),
        )
        .await
    });

    // Give the prompt call time to actually reach the backend and start
    // its own blocking read before firing the cancel -- otherwise this
    // test could pass by accident (cancel racing ahead of the prompt
    // even acquiring the lock in the first place) regardless of whether
    // the fix under test is present.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cancel_start = Instant::now();
    let cancel_response = rpc(
        &client,
        addr,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/cancel",
            "params": {"sessionId": session_id}
        }),
    )
    .await;
    let cancel_elapsed = cancel_start.elapsed();

    assert_eq!(cancel_response["result"], json!({}));
    // Generous threshold: genuinely independent delivery finishes in
    // milliseconds (one small-mutex lock + one line write); serialized
    // behind the in-flight prompt's per-process lock (the pre-fix
    // behavior) would take roughly the *remaining* latency window --
    // with the 300ms head start above, that's still north of 1s out of
    // the full 1.5s. 700ms is comfortably below that remaining-latency
    // floor while well above "one fast write plus scheduling jitter".
    assert!(
        cancel_elapsed < Duration::from_millis(700),
        "session/cancel took {cancel_elapsed:?} while a same-agent session/prompt was still \
         in flight -- expected it to bypass the per-process lock via \
         Supervisor::cancel_writer, not queue up behind it (queuing behind it would take \
         roughly the remaining ~{:?} of SIMULATED_LLM_LATENCY)",
        SIMULATED_LLM_LATENCY.saturating_sub(Duration::from_millis(300))
    );

    // The in-flight prompt itself must still complete normally afterward
    // -- this fix must not have broken or short-circuited it.
    let prompt_response = prompt_task.await.expect("prompt task panicked");
    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));

    // And the real notification genuinely landed on the backend's own
    // stdin, with the real ACP shape (no `id`), not just that acpx's
    // reply to the client came back quickly for some unrelated reason.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let captured = std::fs::read_to_string(&capture_path)
        .expect("backend captured the cancel notification during the prompt's own sleep");
    let _ = std::fs::remove_file(&capture_path);
    assert!(captured.contains("backend-abc"));
    assert!(!captured.contains("\"id\""));
}
