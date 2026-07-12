//! Proves the real multi-agent concurrency fix (see
//! `acpx_core::router::dispatch_shared`'s doc comment and
//! `acpx/COVERAGE.md`'s "real multi-agent concurrency" section): two
//! `session/prompt` calls against two *different* supervised backend
//! agents now genuinely run their backend I/O in parallel through the
//! HTTP transport, rather than fully serializing behind one whole-`Router`
//! mutex held for an entire request (including the backend's own
//! multi-second response latency) as every transport did through Phase 6.
//!
//! Uses the same synthetic `sh -c '...'` stand-in-backend trick as
//! `http_ws_transport_test.rs` (see that file's doc comment for why:
//! `acpx-server` is a binary-only crate, so `#[path]`-including the real
//! transport source is how these integration tests exercise production
//! code without a `[lib]` target) -- each stand-in agent's `session/prompt`
//! handler sleeps for a fixed, generous duration before responding, which
//! stands in for a real backend's LLM-call latency without this test
//! actually depending on network access or an API key.

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

/// How long each stand-in backend sleeps before answering `session/prompt`
/// -- stands in for real LLM latency. Long enough that a serialized
/// (buggy) implementation and a truly-parallel one are unambiguously
/// distinguishable even under this environment's own scheduling jitter,
/// short enough this test still runs quickly when the fix is in place.
const SIMULATED_LLM_LATENCY: Duration = Duration::from_millis(1500);

/// Responds instantly to `session/new`, but sleeps
/// `SIMULATED_LLM_LATENCY` before answering `session/prompt` -- the
/// slow-backend stand-in this whole test is built around.
fn slow_backend_spec() -> SpawnSpec {
    let script = format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    sleep {secs}
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#,
        secs = SIMULATED_LLM_LATENCY.as_secs_f64()
    );
    SpawnSpec::new("sh", vec!["-c".to_string(), script])
}

/// Same ephemeral-port bring-up helper as `http_ws_transport_test.rs`.
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

#[tokio::test]
async fn session_prompt_against_two_different_agents_runs_concurrently_not_serialized() {
    let mut router = Router::new("agent-a");
    router.register_agent("agent-a", slow_backend_spec());
    router.register_agent("agent-b", slow_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = reqwest::Client::new();

    // Two independent profiles, each targeting a distinct already-running
    // backend spec (mirrors `http_ws_transport_test.rs`'s
    // `http_post_rpc_session_new_routes_via_profile_header`'s technique for
    // reusing a directly-registered spec as a profile's agent without
    // needing a live registry entry).
    for name in ["profile-a", "profile-b"] {
        let created = rpc(
            &client,
            addr,
            json!({
                "jsonrpc": "2.0", "id": 0, "method": "profiles/create",
                "params": {"name": name, "agent_id": name.replace("profile", "agent")}
            }),
        )
        .await;
        assert!(
            created.get("error").is_none(),
            "profiles/create failed: {created:?}"
        );
    }

    // Open one session per agent up front (session/new on this stand-in is
    // instant, not part of what's being timed).
    let mut session_ids = Vec::new();
    for (i, profile) in ["profile-a", "profile-b"].iter().enumerate() {
        let resp = rpc(
            &client,
            addr,
            json!({
                "jsonrpc": "2.0", "id": 10 + i, "method": "session/new",
                "params": {"cwd": "/tmp", "_acpx": {"profile": profile}}
            }),
        )
        .await;
        let session_id = resp["result"]["sessionId"]
            .as_str()
            .expect("session/new returned a sessionId")
            .to_string();
        session_ids.push(session_id);
    }

    // Fire both `session/prompt` calls concurrently. Each stand-in backend
    // sleeps `SIMULATED_LLM_LATENCY` before answering; if these two calls
    // were still serialized behind one whole-`Router` lock (the pre-fix
    // behavior), the wall-clock total would be roughly `2 *
    // SIMULATED_LLM_LATENCY`. With the fix, both backends' sleeps overlap,
    // so the total should be close to *one* `SIMULATED_LLM_LATENCY`.
    let start = Instant::now();
    let (resp_a, resp_b) = tokio::join!(
        rpc(
            &client,
            addr,
            json!({
                "jsonrpc": "2.0", "id": 20, "method": "session/prompt",
                "params": {"sessionId": session_ids[0], "prompt": []}
            }),
        ),
        rpc(
            &client,
            addr,
            json!({
                "jsonrpc": "2.0", "id": 21, "method": "session/prompt",
                "params": {"sessionId": session_ids[1], "prompt": []}
            }),
        ),
    );
    let elapsed = start.elapsed();

    assert_eq!(resp_a["result"]["stopReason"], json!("end_turn"));
    assert_eq!(resp_b["result"]["stopReason"], json!("end_turn"));

    // Generous threshold: truly-parallel execution finishes in roughly one
    // latency window (plus scheduling/process-spawn slack); a serialized
    // implementation takes roughly two. 2.5x the single-call latency is
    // comfortably below "two full sequential calls" while still well above
    // "one call plus jitter", so this reliably distinguishes the two
    // without being a flaky hair-trigger on a loaded CI box.
    let threshold = SIMULATED_LLM_LATENCY.mul_f64(1.9);
    assert!(
        elapsed < threshold,
        "two concurrent session/prompt calls against different agents took {elapsed:?}, \
         expected well under {threshold:?} (2x{SIMULATED_LLM_LATENCY:?} would mean they \
         serialized instead of running in parallel)"
    );
}
