//! Integration tests for the HTTP/WS transport (`transport::http`/`ws`).
//! Follows the same synthetic stand-in "backend" trick as
//! `acpx-core/tests/router_dispatch_test.rs` (a tiny `sh -c '...'` script
//! that echoes back a canned JSON-RPC response) so these tests don't
//! depend on a real ACP adapter being installed.

use std::net::SocketAddr;
use std::sync::Arc;

use acpx_bridge::{BridgeConfig, BridgeModel};
use acpx_conductor::SpawnSpec;
use acpx_core::{router::Router, NotificationHub};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// `acpx-server` is a binary-only crate (no `[lib]` target, and adding one
// is outside this task's file ownership -- `main.rs`/`Cargo.toml`'s crate
// shape are owned by the main agent's concurrent work). Integration tests
// for a bin-only crate can't `use acpx_server::...`, so instead we compile
// the actual `transport::http`/`transport::ws` source files directly into
// this test binary via `#[path]`. This exercises the real production code
// (not a copy), just via a different crate root than `main.rs`'s.
// Declared directly at this file's (crate) root -- not nested inside an
// extra `mod transport { .. }` wrapper -- so `#[path]` here resolves
// relative to this file's own directory (`tests/`) rather than an
// implicit `tests/transport/` subdirectory. `http.rs`'s internal
// `super::ws::ws_handler` reference still resolves correctly since
// `super` from this `http` module is this crate's root, which is exactly
// where `ws` is declared too -- mirroring `src/transport/mod.rs`'s
// `pub mod http; pub mod ws;` shape.
#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, serve_on_with_bridge, SharedRouter};

/// Echoes back a canned `session/new` result carrying `sessionId`
/// `"backend-abc"`, or `{"ok": true}` for anything else -- same shape as
/// the reference stand-in in `acpx-core/tests/router_dispatch_test.rs`.
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

/// Same as the default stand-in, but the `session/new` result carries a
/// distinguishable `agentTag` field so tests can tell which registered
/// agent actually served a request -- used to verify `X-Acpx-Profile`
/// header routing picks the right backend.
fn stand_in_backend_script_with_tag(tag: &str) -> String {
    format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc","agentTag":"{tag}"}}}}\n' "$id"
  elif echo "$line" | grep -q 'session/fork'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-fork-{tag}","agentTag":"{tag}"}}}}\n' "$id"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true,"agentTag":"{tag}"}}}}\n' "$id"
  fi
done
"#
    )
}

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

fn tagged_backend_spec(tag: &str) -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), stand_in_backend_script_with_tag(tag)],
    )
}

fn streaming_backend_spec() -> SpawnSpec {
    let script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-stream"}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-stream","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"stream"}}}}\n'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;
    SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()])
}

fn delayed_resume_backend_spec() -> SpawnSpec {
    let script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-resume"}}\n' "$id"
  elif echo "$line" | grep -q 'session/resume'; then
    sleep 0.3
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;
    SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()])
}

/// Starts `transport::serve` on `127.0.0.1:0` (OS-assigned port) in a
/// background task and returns the resolved local address to connect to.
/// We bind the listener ourselves (rather than letting `serve` do it) so
/// we can hand back the real port before the caller needs it -- `serve`'s
/// signature takes a `SocketAddr` to bind, so we probe an ephemeral port
/// first and pass that in; a `0.0.0.0`/`127.0.0.1:0` bind never collides
/// with another test running concurrently.
async fn spawn_server(router: SharedRouter) -> SocketAddr {
    // Bind once here to learn the OS-assigned port, then drop the
    // listener and immediately hand the same address to `serve` -- the
    // brief gap is safe for a local test (nothing else in this process
    // binds ports concurrently).
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        serve(router, addr, None).await.expect("transport::serve");
    });

    // Give the listener a moment to come up before the test issues its
    // first request.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

/// Starts the bridge-enabled transport on an already-bound loopback
/// listener, matching the production `serve_on_with_bridge` startup path.
async fn spawn_server_with_bridge(router: SharedRouter, bridge: BridgeConfig) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        serve_on_with_bridge(listener, router, None, Some(bridge))
            .await
            .expect("transport::serve_on_with_bridge");
    });

    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

#[tokio::test]
async fn legacy_server_does_not_mount_acp_model_catalog() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let addr = spawn_server(Arc::new(Mutex::new(router))).await;

    let response = reqwest::get(format!("http://{addr}/acp/models"))
        .await
        .expect("GET /acp/models");

    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bridge_catalog_routes_expose_only_configured_public_entries() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let addr = spawn_server_with_bridge(
        Arc::new(Mutex::new(router)),
        BridgeConfig {
            default_model: "public/sonnet".to_string(),
            models: vec![
                BridgeModel {
                    id: "public/sonnet".to_string(),
                    name: Some("Public Sonnet".to_string()),
                    agent_id: "codex-acp".to_string(),
                    model_id: "internal-model-secret-one".to_string(),
                },
                BridgeModel {
                    id: "public/private".to_string(),
                    name: None,
                    agent_id: "not-a-registry-adapter".to_string(),
                    model_id: "internal-model-secret-two".to_string(),
                },
            ],
        },
    )
    .await;
    let client = reqwest::Client::new();

    let models_response = client
        .get(format!("http://{addr}/acp/models"))
        .send()
        .await
        .expect("GET /acp/models");
    assert!(models_response.status().is_success());
    let models: serde_json::Value = models_response.json().await.expect("models json body");
    assert_eq!(models["defaultModel"], json!("public/sonnet"));
    assert_eq!(
        models["models"]
            .as_array()
            .expect("models is an array")
            .iter()
            .map(|model| model["id"].as_str().expect("model id"))
            .collect::<Vec<_>>(),
        vec!["public/sonnet", "public/private"]
    );
    assert!(models["models"]
        .as_array()
        .expect("models is an array")
        .iter()
        .all(|model| model
            .get("available")
            .and_then(serde_json::Value::as_bool)
            .is_some()));
    let models_text = models.to_string();
    assert!(!models_text.contains("internal-model-secret-one"));
    assert!(!models_text.contains("internal-model-secret-two"));
    assert!(!models_text.contains("modelId"));

    let agents_response = client
        .get(format!("http://{addr}/acp/agents"))
        .send()
        .await
        .expect("GET /acp/agents");
    assert!(agents_response.status().is_success());
    let agents: serde_json::Value = agents_response.json().await.expect("agents json body");
    let configured_agent_ids = ["codex-acp", "not-a-registry-adapter"];
    assert!(agents["agents"]
        .as_array()
        .expect("agents is an array")
        .iter()
        .all(|agent| configured_agent_ids.contains(&agent["id"].as_str().expect("agent id"))));
}

#[tokio::test]
async fn strict_acp_http_bridge_lazily_binds_selected_models_without_profiles() {
    let mut router = Router::new("codex-acp");
    router.register_agent("claude-acp", tagged_backend_spec("claude"));
    router.register_agent("codex-acp", tagged_backend_spec("codex"));
    let addr = spawn_server_with_bridge(
        Arc::new(Mutex::new(router)),
        BridgeConfig {
            default_model: "codex/gpt-5.5".to_string(),
            models: vec![
                BridgeModel {
                    id: "claude/sonnet".to_string(),
                    name: Some("Claude Sonnet".to_string()),
                    agent_id: "claude-acp".to_string(),
                    model_id: "sonnet".to_string(),
                },
                BridgeModel {
                    id: "codex/gpt-5.5".to_string(),
                    name: Some("Codex GPT-5.5".to_string()),
                    agent_id: "codex-acp".to_string(),
                    model_id: "gpt-5.5".to_string(),
                },
            ],
        },
    )
    .await;
    let client = reqwest::Client::new();

    let new = |id| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/new",
            "params": {"cwd": "/tmp"}
        })
    };
    let claude_new: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&new(1))
        .send()
        .await
        .expect("bridge session/new")
        .json()
        .await
        .expect("bridge session/new JSON");
    let claude_session = claude_new["result"]["sessionId"]
        .as_str()
        .expect("virtual session id")
        .to_string();
    assert_eq!(
        claude_new["result"]["configOptions"][0]["id"],
        json!("model")
    );
    assert_eq!(
        claude_new["result"]["configOptions"][0]["currentValue"],
        json!("codex/gpt-5.5")
    );
    assert!(claude_new.to_string().contains("claude/sonnet"));
    assert!(!claude_new.to_string().contains("claude-acp"));

    let selected: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/set_config_option",
            "params": {"sessionId": claude_session, "configId": "model", "value": "claude/sonnet"}
        }))
        .send()
        .await
        .expect("bridge model select")
        .json()
        .await
        .expect("bridge model select JSON");
    assert!(selected.get("error").is_none(), "{selected:?}");
    assert_eq!(selected["result"]["configOptions"][0]["id"], json!("model"));

    let claude_prompt: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": claude_session, "prompt": []}
        }))
        .send()
        .await
        .expect("bridge claude prompt")
        .json()
        .await
        .expect("bridge claude prompt JSON");
    assert_eq!(
        claude_prompt["result"]["agentTag"],
        json!("claude"),
        "unexpected bridged Claude response: {claude_prompt:?}"
    );

    let forked: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 31, "method": "session/fork",
            "params": {"sessionId": claude_session, "cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("bridge Claude fork")
        .json()
        .await
        .expect("bridge Claude fork JSON");
    let forked_session = forked["result"]["sessionId"]
        .as_str()
        .expect("forked virtual session id")
        .to_string();
    assert_ne!(forked_session, "backend-fork-claude");
    let fork_prompt: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 32, "method": "session/prompt",
            "params": {"sessionId": forked_session, "prompt": []}
        }))
        .send()
        .await
        .expect("bridge fork prompt")
        .json()
        .await
        .expect("bridge fork prompt JSON");
    assert_eq!(fork_prompt["result"]["agentTag"], json!("claude"));

    let codex_new: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&new(4))
        .send()
        .await
        .expect("bridge default session/new")
        .json()
        .await
        .expect("bridge default session/new JSON");
    let codex_session = codex_new["result"]["sessionId"]
        .as_str()
        .expect("virtual session id")
        .to_string();
    let codex_prompt: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/prompt",
            "params": {"sessionId": codex_session, "prompt": []}
        }))
        .send()
        .await
        .expect("bridge codex prompt")
        .json()
        .await
        .expect("bridge codex prompt JSON");
    assert_eq!(codex_prompt["result"]["agentTag"], json!("codex"));

    let rejected: serde_json::Value = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 6, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "forbidden"}}
        }))
        .send()
        .await
        .expect("bridge forbidden extension request")
        .json()
        .await
        .expect("bridge forbidden extension JSON");
    assert_eq!(rejected["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn http_post_rpc_round_trips_gateway_native_method() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/list",
            "params": {}
        }))
        .send()
        .await
        .expect("POST /rpc");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["jsonrpc"], json!("2.0"));
    assert_eq!(body["id"], json!(1));
    assert_eq!(body["result"]["sessions"], json!([]));
}

#[tokio::test]
async fn http_post_rpc_session_new_routes_via_profile_header() {
    let mut router = Router::new("agent-a");
    router.register_agent("agent-a", tagged_backend_spec("A"));
    router.register_agent("agent-b", tagged_backend_spec("B"));
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = reqwest::Client::new();

    // `_acpx.profile` resolves through the Phase 3 `ProfileStore`, not a
    // raw agent id directly (see `router.rs`'s `resolve_profile`) -- a
    // profile whose `agent_id` names an already-registered spec (as
    // "agent-b" is, above) reuses that spec directly rather than
    // requiring a live registry entry, so this test double works
    // unmodified as a profile target.
    let create_profile = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "profiles/create",
            "params": {"name": "agent-b", "agent_id": "agent-b"}
        }))
        .send()
        .await
        .expect("POST /rpc (profiles/create)");
    assert!(create_profile.status().is_success());

    // No header -> falls back to the default agent ("agent-a").
    let default_response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("POST /rpc (no header)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(default_response["result"]["agentTag"], json!("A"));

    // X-Acpx-Profile header selects "agent-b" instead, even though the
    // inline params carry no _acpx field at all.
    let header_response = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Profile", "agent-b")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("POST /rpc (with header)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(header_response["result"]["agentTag"], json!("B"));

    // Header also wins over an inline params._acpx.profile that names a
    // *different* agent -- highest precedence per 02-architecture.md.
    let override_response = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Profile", "agent-b")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "agent-a"}}
        }))
        .send()
        .await
        .expect("POST /rpc (header overrides inline)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(override_response["result"]["agentTag"], json!("B"));
}

#[tokio::test]
async fn ws_round_trips_a_request() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let (mut socket, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/new",
                "params": {"cwd": "/tmp"}
            })
            .to_string(),
        ))
        .await
        .expect("send ws frame");

    let reply = socket
        .next()
        .await
        .expect("ws stream ended early")
        .expect("ws frame error");
    let text = match reply {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
    assert_eq!(body["jsonrpc"], json!("2.0"));
    assert_eq!(body["id"], json!(1));
    assert!(body["result"]["sessionId"].is_string());
    // The client only ever sees the gateway-issued id, never the backend's
    // own "backend-abc" -- same invariant `router_dispatch_test.rs` checks
    // directly against `Router`.
    assert_ne!(body["result"]["sessionId"], json!("backend-abc"));
}

#[tokio::test]
async fn ws_rejects_an_over_limit_subscriber_without_disrupting_the_existing_stream() {
    let mut router =
        Router::new("stand-in-agent").with_notification_hub(NotificationHub::with_limits(16, 1));
    router.register_agent("stand-in-agent", streaming_backend_spec());
    let addr = spawn_server(Arc::new(Mutex::new(router))).await;

    let (mut first, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("first websocket connect");
    first
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/new",
                "params": {"cwd": "/tmp"}
            })
            .to_string(),
        ))
        .await
        .expect("first session/new");
    let created_frame = match first
        .next()
        .await
        .expect("first session/new response")
        .expect("first session/new frame")
    {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let created: serde_json::Value =
        serde_json::from_str(&created_frame).expect("first session/new JSON");
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_string();

    let (mut second, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("second websocket connect");
    second
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": []}
            })
            .to_string(),
        ))
        .await
        .expect("second prompt");
    let rejected_frame = match second
        .next()
        .await
        .expect("over-limit response")
        .expect("over-limit frame")
    {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let rejected: serde_json::Value =
        serde_json::from_str(&rejected_frame).expect("over-limit JSON");
    assert_eq!(rejected["error"]["code"], json!(-32050));
    assert_eq!(rejected["error"]["data"]["maxSubscribers"], json!(1));

    first
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": []}
            })
            .to_string(),
        ))
        .await
        .expect("first prompt");
    let mut saw_update = false;
    let mut saw_response = false;
    for _ in 0..2 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), first.next())
            .await
            .expect("first client timed out waiting for streamed turn")
            .expect("first websocket closed")
            .expect("first websocket frame error");
        let text = match frame {
            WsMessage::Text(text) => text,
            other => panic!("expected text frame, got {other:?}"),
        };
        let body: serde_json::Value = serde_json::from_str(&text).expect("first client JSON");
        saw_update |= body["method"] == json!("session/update");
        saw_response |= body["id"] == json!(3) && body.get("error").is_none();
    }
    assert!(saw_update, "existing subscriber missed its streamed update");
    assert!(saw_response, "existing subscriber's prompt was disrupted");
}

#[tokio::test]
async fn ws_resume_replays_and_tails_updates_once_while_resume_is_in_flight() {
    // The replay buffer intentionally retains only two updates. Three more
    // updates are published while the backend delays `session/resume`, so a
    // subscription installed after dispatch would lose one. The transport
    // must attach first, replay seq=1, then tail seq=2..4 exactly once.
    let mut router = Router::new("stand-in-agent")
        .with_notification_hub(NotificationHub::with_replay_limits(16, 8, 2));
    router.register_agent("stand-in-agent", delayed_resume_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(Arc::clone(&router)).await;
    let client = reqwest::Client::new();
    let created: serde_json::Value = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("session/new request")
        .json()
        .await
        .expect("session/new JSON");
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_string();
    let hub = { router.lock().await.notification_hub() };
    let state = acpx_core::router::stream_resume_state_shared(
        &router,
        &acpx_core::TenantId::default(),
        &session_id,
    )
    .await;
    let mut bootstrap = hub
        .subscribe_resuming(
            &acpx_core::TenantId::default(),
            session_id.clone(),
            None,
            acpx_core::StreamResumeState {
                backend_session_id: state.backend_session_id,
                durable_state_changed: state.durable_state_changed,
            },
        )
        .await
        .expect("bootstrap stream");
    assert!(
        hub.publish(
            &acpx_core::TenantId::default(),
            &session_id,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":{"n":1}}})
        )
        .await
    );
    let epoch = bootstrap
        .recv()
        .await
        .expect("initial sequence")
        .into_value()["params"]["_acpx"]["epoch"]
        .as_str()
        .expect("epoch metadata")
        .to_string();
    drop(bootstrap);

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("websocket connect");
    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/resume",
                "params": {
                    "sessionId": session_id,
                    "_acpx": {"resume": {"lastSeq": 0, "epoch": epoch}}
                }
            })
            .to_string(),
        ))
        .await
        .expect("resume request");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    for n in 2..=4 {
        assert!(
            hub.publish(
                &acpx_core::TenantId::default(),
                &session_id,
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":{"n":n}}})
            )
            .await
        );
    }

    let mut sequences = Vec::new();
    let mut saw_resume_response = false;
    for _ in 0..5 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("resume stream timed out")
            .expect("websocket closed")
            .expect("websocket frame");
        let WsMessage::Text(text) = frame else {
            panic!("expected text frame");
        };
        let body: serde_json::Value = serde_json::from_str(&text).expect("JSON frame");
        if body["method"] == json!("session/update") {
            sequences.push(
                body["params"]["_acpx"]["seq"]
                    .as_u64()
                    .expect("sequence metadata"),
            );
        } else if body["id"] == json!(2) {
            saw_resume_response = true;
        }
    }
    assert_eq!(sequences, vec![1, 2, 3, 4]);
    assert!(saw_resume_response, "missing session/resume response");
}

#[tokio::test]
async fn strict_acp_ws_exposes_virtual_session_model_selection() {
    let mut router = Router::new("codex-acp");
    router.register_agent("codex-acp", tagged_backend_spec("codex"));
    let addr = spawn_server_with_bridge(
        Arc::new(Mutex::new(router)),
        BridgeConfig {
            default_model: "codex/gpt-5.5".to_string(),
            models: vec![BridgeModel {
                id: "codex/gpt-5.5".to_string(),
                name: None,
                agent_id: "codex-acp".to_string(),
                model_id: "gpt-5.5".to_string(),
            }],
        },
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/acp/ws"))
        .await
        .expect("strict ACP websocket connect");
    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {"cwd": "/tmp"}
            })
            .to_string(),
        ))
        .await
        .expect("send strict ACP websocket frame");
    let reply = socket
        .next()
        .await
        .expect("strict ACP websocket stream ended")
        .expect("strict ACP websocket frame");
    let text = match reply {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let body: serde_json::Value = serde_json::from_str(&text).expect("strict ACP JSON");
    assert!(body["result"]["sessionId"].is_string());
    assert_eq!(
        body["result"]["configOptions"][0]["options"][0]["value"],
        json!("codex/gpt-5.5")
    );
}

#[tokio::test]
async fn strict_acp_ws_forwards_bound_session_updates_with_virtual_ids() {
    let mut router = Router::new("streaming");
    router.register_agent("streaming", streaming_backend_spec());
    let addr = spawn_server_with_bridge(
        Arc::new(Mutex::new(router)),
        BridgeConfig {
            default_model: "stream/model".to_string(),
            models: vec![BridgeModel {
                id: "stream/model".to_string(),
                name: None,
                agent_id: "streaming".to_string(),
                model_id: "stream-model".to_string(),
            }],
        },
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/acp/ws"))
        .await
        .expect("strict ACP websocket connect");

    async fn send(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        value: serde_json::Value,
    ) {
        socket
            .send(WsMessage::Text(value.to_string()))
            .await
            .expect("send frame");
    }
    async fn receive(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        let frame = socket
            .next()
            .await
            .expect("socket ended")
            .expect("socket frame");
        let WsMessage::Text(text) = frame else {
            panic!("expected text frame");
        };
        serde_json::from_str(&text).expect("JSON frame")
    }

    send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}),
    )
    .await;
    let created = receive(&mut socket).await;
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
    send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[]}}),
    )
    .await;
    let first_update = receive(&mut socket).await;
    assert_eq!(first_update["method"], json!("session/update"));
    assert_eq!(first_update["params"]["sessionId"], json!(session_id));
    let first = receive(&mut socket).await;
    assert_eq!(first["id"], json!(2));
    // The transport installs its hub forwarder after the bind-completing
    // response; let that spawned task begin receiving before turn two.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[]}}),
    )
    .await;
    let update = receive(&mut socket).await;
    assert_eq!(
        update["method"],
        json!("session/update"),
        "expected live update before response, got {update:?}"
    );
    assert_eq!(update["params"]["sessionId"], json!(session_id));
    let final_response = receive(&mut socket).await;
    assert_eq!(final_response["id"], json!(3));
}
