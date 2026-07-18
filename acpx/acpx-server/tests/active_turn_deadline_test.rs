//! **`active_turn_deadline`, `acpx-session-lifecycle` plan.** Proves the
//! bounded active-turn cancellation/recovery policy end-to-end against a
//! real backend process reached through the real HTTP transport: a
//! `session/prompt` call that has been in flight longer than
//! `LifecycleConfig::active_turn_deadline` gets a real `session/cancel`
//! notification delivered to the backend's stdin, and the session stops
//! being unconditionally reap-exempt afterward -- without the router
//! itself force-closing the session or interrupting the original,
//! still-in-flight client call.
//!
//! Same structural trick as `session_cancel_concurrency_test.rs` (see
//! that file's doc comment): a stand-in backend sleeps a fixed, generous
//! duration before answering `session/prompt`, standing in for a turn
//! that runs far longer than a configured deadline.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acpx_conductor::SpawnSpec;
use acpx_core::lifecycle::LifecycleConfig;
use acpx_core::router::Router;
use serde_json::json;
use tokio::sync::Mutex;

#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, SharedRouter};

/// Long enough that the test's short `active_turn_deadline` elapses well
/// before the backend would ever answer on its own.
const SIMULATED_LLM_LATENCY: Duration = Duration::from_secs(2);

/// Responds instantly to `session/new`. Sleeps `SIMULATED_LLM_LATENCY`
/// before answering `session/prompt` (never actually reached in this
/// test -- the deadline fires first), but -- unlike
/// `session_cancel_concurrency_test.rs`'s stand-in -- backgrounds that
/// sleep (`( sleep ...; printf ... ) &`) rather than blocking its own
/// `read` loop on it: this test never waits out the full latency (that
/// would defeat the point of a *bounded* deadline), so the script must
/// stay able to read and react to a subsequent `session/cancel` line
/// immediately, not only after its own delayed reply eventually prints.
/// Captures every `session/cancel` line it receives to `capture_path`,
/// so this test can verify the real notification landed on the
/// backend's stdin while the prompt is still genuinely pending.
fn slow_backend_spec_with_cancel_capture(capture_path: &str) -> SpawnSpec {
    let script = format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    ( sleep {secs}; printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id" ) &
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

/// Same ephemeral-port bring-up helper as `session_cancel_concurrency_test.rs`.
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
        "acpx-active-turn-deadline-test-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ))
}

#[tokio::test]
async fn stuck_turn_is_cancelled_and_no_longer_reap_exempt_once_the_deadline_elapses() {
    let capture_path = unique_capture_path();
    let mut router = Router::new("agent-a").with_lifecycle_config(LifecycleConfig {
        // Deliberately tiny -- this test controls the crossing itself by
        // waiting past it in real time (the backend's own sleep is far
        // longer, so a naturally-elapsed prompt round trip can never be
        // what makes this pass).
        active_turn_deadline: Some(Duration::from_millis(50)),
        ..LifecycleConfig::default()
    });
    router.register_agent(
        "agent-a",
        slow_backend_spec_with_cancel_capture(capture_path.to_str().unwrap()),
    );
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router.clone()).await;
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

    // Fire `session/prompt` on its own task -- it blocks for
    // `SIMULATED_LLM_LATENCY` inside the stand-in backend. Never awaited
    // to completion by this test (it would take 5s); left to finish on
    // its own after the process exits.
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

    // Give the prompt call time to actually mark the session in-flight,
    // and to run well past the 50ms deadline configured above.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let before = rpc(
        &client,
        addr,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/retention/get",
            "params": {"sessionId": session_id}
        }),
    )
    .await;
    assert_eq!(
        before["result"]["inFlight"],
        json!(1),
        "the prompt call must have marked the session in-flight before the deadline check runs"
    );

    let cancelled = router.lock().await.cancel_stuck_turns(Instant::now()).await;
    assert_eq!(
        cancelled, 1,
        "exactly the one stuck session must be cancelled"
    );

    let after = rpc(
        &client,
        addr,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/retention/get",
            "params": {"sessionId": session_id}
        }),
    )
    .await;
    assert_eq!(
        after["result"]["inFlight"],
        json!(0),
        "a cancelled stuck turn must no longer be unconditionally reap-exempt"
    );

    // The real notification genuinely landed on the backend's own
    // stdin, with the real ACP shape (no `id`).
    let captured = {
        let mut seen = None;
        for _ in 0..50 {
            if let Ok(contents) = std::fs::read_to_string(&capture_path) {
                seen = Some(contents);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        seen.expect("backend captured the cancel notification while session/prompt was still open")
    };
    let _ = std::fs::remove_file(&capture_path);
    assert!(captured.contains("backend-abc"));
    assert!(!captured.contains("\"id\""));

    // The original, still-in-flight client call is untouched by the
    // deadline recovery itself -- this test doesn't wait for it (that
    // would take the remainder of `SIMULATED_LLM_LATENCY`), it only
    // proves the task is still alive and not already resolved.
    assert!(
        !prompt_task.is_finished(),
        "cancel_stuck_turns must not itself force the original client call to return early"
    );
    prompt_task.abort();
}
