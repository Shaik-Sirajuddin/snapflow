//! **End-to-end coverage tying together three properties that no single
//! prior test file exercised all at once:** real concurrency (genuinely
//! parallel in-flight requests, not just sequential `await`s), multi-
//! tenancy (`acpx-tenant-isolation` phases A-C), and multiple client
//! *connections* belonging to the same tenant sharing one session.
//!
//! Prior coverage exercised these pairwise but never all three together:
//! - `concurrency_test.rs` / `session_cancel_concurrency_test.rs`: real
//!   parallel backend I/O, but single-tenant (default tenant only).
//! - `tenant_isolation_test.rs`: multi-tenant isolation, but every call
//!   sequence is `await`ed one at a time -- never two tenants' requests
//!   genuinely in flight at once, so a race inside the tenant-nested
//!   `SessionRegistry`'s locking would not have been caught.
//! - Nothing prior opened more than one client connection under the
//!   *same* tenant against the *same* session at once.
//!
//! Same synthetic `sh -c '...'` stand-in-backend + `#[path]`-compiled-
//! real-transport-source technique as every other file in this
//! directory (see `http_ws_transport_test.rs`'s top-of-file comment for
//! why `acpx-server`, a binary-only crate, needs this to exercise its
//! own production transport code from an integration test at all).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use acpx_core::TenantId;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, SharedRouter};

/// Answers every method generically with `{"ok": true}` except
/// `session/new` (mints a fresh, request-counter-suffixed backend session
/// id so multiple concurrently-opened sessions against this one shared
/// backend process are still individually distinguishable) and
/// `session/list` (echoes back a fixed single entry, mirroring
/// `tenant_isolation_test.rs`'s stand-in). A tiny `sleep` on `session/
/// prompt` stands in for real backend/LLM latency, exactly like
/// `concurrency_test.rs`'s stand-in -- long enough that truly-parallel
/// vs. accidentally-serialized dispatch are unambiguously distinguishable.
const STAND_IN_BACKEND_SCRIPT: &str = r#"
counter=0
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    counter=$((counter+1))
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-%s"}}\n' "$id" "$counter"
  elif echo "$line" | grep -q 'session/prompt'; then
    sleep 0.4
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

/// Same ephemeral-port bring-up helper as every other file here.
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
    tenant: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", tenant)
        .json(&body)
        .send()
        .await
        .expect("POST /rpc")
        .json::<serde_json::Value>()
        .await
        .expect("json body")
}

/// Opens a WS connection carrying `X-Acpx-Tenant: tenant` at upgrade time
/// -- the only point in a WS connection's lifetime the header is
/// available (see `ws.rs`'s module doc comment), cached for that
/// connection's whole lifetime from then on.
async fn connect_ws_as_tenant(
    addr: SocketAddr,
    tenant: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let uri: tokio_tungstenite::tungstenite::http::Uri =
        format!("ws://{addr}/ws").parse().expect("parse ws uri");
    let request = ClientRequestBuilder::new(uri).with_header("X-Acpx-Tenant", tenant);
    let (socket, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    socket
}

async fn ws_rpc(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    body: serde_json::Value,
) -> serde_json::Value {
    socket
        .send(WsMessage::Text(body.to_string()))
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
    serde_json::from_str(&text).expect("json body")
}

/// **Multiple clients of the same tenant.** Two independent `reqwest::
/// Client` connections -- standing in for two separate ACP client
/// processes, e.g. two IDE windows or a CLI plus an editor plugin, both
/// authenticated as the *same* tenant -- concurrently list and then
/// concurrently prompt the exact same gateway session. Proves acpx's
/// ownership model (`notify.rs`'s doc comment: "a gateway session id is
/// only ever handed back to the one client connection whose call minted
/// or supplied it" -- but *any* connection authenticated as the owning
/// tenant may supply it) actually holds under real concurrent HTTP
/// traffic, not just sequential calls: both clients see the identical
/// session in their tenant-scoped aggregate, and both clients' concurrent
/// prompts against it succeed without either request's response getting
/// swapped, dropped, or corrupted by the other's concurrently in-flight
/// call.
#[tokio::test]
async fn multiple_http_clients_of_the_same_tenant_concurrently_share_one_session() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    // Two distinct `reqwest::Client`s -- separate connection pools, the
    // closest a single-process test can get to "two separate OS
    // processes" without actually spawning two `acp-client` binaries.
    let client_1 = reqwest::Client::new();
    let client_2 = reqwest::Client::new();

    let created = rpc(
        &client_1,
        addr,
        "acme",
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await;
    let gateway_id = created["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // Both clients concurrently list -- both must see exactly this one
    // session, under the same tenant-scoped aggregate, even though
    // client_2 never itself called session/new.
    let (list_1, list_2) = tokio::join!(
        rpc(
            &client_1,
            addr,
            "acme",
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}),
        ),
        rpc(
            &client_2,
            addr,
            "acme",
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}),
        ),
    );
    for (label, list) in [("client_1", &list_1), ("client_2", &list_2)] {
        let sessions = list["result"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: sessions array missing: {list:?}"));
        assert_eq!(
            sessions.len(),
            1,
            "{label} should see the one shared-tenant session: {sessions:?}"
        );
        assert_eq!(sessions[0]["sessionId"], json!(gateway_id));
    }

    // Now both clients concurrently prompt the *same* session. Unlike
    // `concurrency_test.rs`'s two-*different*-backends case, this is
    // deliberately *not* asserted to run in parallel wall-clock time:
    // both prompts target the exact same backend session/conversation,
    // and the stand-in backend is one single-threaded `sh -c` process
    // reading its stdin one line at a time -- so these two calls
    // genuinely queue behind each other at the *backend process* level,
    // exactly like a real conversational agent can't process two
    // simultaneous turns against one conversation identity either. That
    // is correct, not a lock bug; what this test actually proves is
    // *correctness* under concurrent same-session access from two
    // different client connections -- neither call's request id or
    // response gets dropped, swapped, or corrupted by the other's
    // concurrently in-flight call.
    let (prompt_1, prompt_2) = tokio::join!(
        rpc(
            &client_1,
            addr,
            "acme",
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": gateway_id, "prompt": []}
            }),
        ),
        rpc(
            &client_2,
            addr,
            "acme",
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                "params": {"sessionId": gateway_id, "prompt": []}
            }),
        ),
    );

    assert_eq!(
        prompt_1["id"],
        json!(3),
        "client_1's own id must come back on its own reply"
    );
    assert_eq!(
        prompt_2["id"],
        json!(4),
        "client_2's own id must come back on its own reply"
    );
    assert!(
        prompt_1.get("error").is_none(),
        "client_1's prompt against the shared session should succeed: {prompt_1:?}"
    );
    assert!(
        prompt_2.get("error").is_none(),
        "client_2's prompt against the shared session should succeed: {prompt_2:?}"
    );
    assert_eq!(prompt_1["result"]["stopReason"], json!("end_turn"));
    assert_eq!(prompt_2["result"]["stopReason"], json!("end_turn"));
}

/// **Concurrency + multi-tenancy together, under real parallel load.**
/// Two tenants, each with several "clients" hammering `session/new` +
/// `session/list` *genuinely concurrently* (via `futures_util::future::
/// join_all` over interleaved per-tenant tasks, not sequential `await`s)
/// -- proving the tenant-nested `SessionRegistry`'s locking holds up
/// under real contention from two tenants racing each other, not just
/// the one-call-at-a-time interleaving `tenant_isolation_test.rs`
/// exercises. Every session/list response must contain only sessions
/// this exact tenant created -- never a partial or corrupted mix from a
/// tenant racing to insert/read the shared `HashMap<TenantId,
/// HashMap<String, SessionEntry>>` concurrently.
#[tokio::test]
async fn concurrent_load_across_two_tenants_never_cross_leaks_under_real_parallel_traffic() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    const SESSIONS_PER_TENANT: usize = 8;

    async fn one_client_session_and_list(
        addr: SocketAddr,
        tenant: &'static str,
    ) -> (String, Vec<String>) {
        let client = reqwest::Client::new();
        let created = rpc(
            &client,
            addr,
            tenant,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await;
        let gateway_id = created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{tenant}: session/new failed: {created:?}"))
            .to_string();
        let listed = rpc(
            &client,
            addr,
            tenant,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}),
        )
        .await;
        let seen: Vec<String> = listed["result"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("{tenant}: session/list failed: {listed:?}"))
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap_or_default().to_string())
            .collect();
        (gateway_id, seen)
    }

    // Interleave both tenants' tasks into one flat set so `join_all`
    // schedules them in racing order, not tenant-a-then-tenant-b batches.
    let mut tasks = Vec::new();
    for i in 0..SESSIONS_PER_TENANT {
        let tenant = if i % 2 == 0 { "tenant-a" } else { "tenant-b" };
        tasks.push(tokio::spawn(one_client_session_and_list(addr, tenant)));
    }
    let results = futures_util::future::join_all(tasks).await;

    let mut tenant_a_ids = std::collections::HashSet::new();
    let mut tenant_b_ids = std::collections::HashSet::new();
    for (i, result) in results.into_iter().enumerate() {
        let (gateway_id, _seen) = result.expect("task join");
        if i % 2 == 0 {
            tenant_a_ids.insert(gateway_id);
        } else {
            tenant_b_ids.insert(gateway_id);
        }
    }
    assert!(
        tenant_a_ids.is_disjoint(&tenant_b_ids),
        "tenant-a and tenant-b must never mint colliding gateway ids: \
         a={tenant_a_ids:?} b={tenant_b_ids:?}"
    );

    // Final read-back: each tenant's own gateway-scoped session/list must
    // show *exactly* the sessions that tenant created above, no more, no
    // less -- proves the concurrent writes above landed correctly, not
    // just that they didn't crash.
    let client = reqwest::Client::new();
    let final_a = rpc(
        &client,
        addr,
        "tenant-a",
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/list", "params": {}}),
    )
    .await;
    let final_a_ids: std::collections::HashSet<String> = final_a["result"]["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        final_a_ids, tenant_a_ids,
        "tenant-a's final aggregate must match exactly what tenant-a created \
         under concurrent cross-tenant load"
    );

    let final_b = rpc(
        &client,
        addr,
        "tenant-b",
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/list", "params": {}}),
    )
    .await;
    let final_b_ids: std::collections::HashSet<String> = final_b["result"]["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        final_b_ids, tenant_b_ids,
        "tenant-b's final aggregate must match exactly what tenant-b created \
         under concurrent cross-tenant load, never any of tenant-a's ids"
    );
}

/// **Multi-client-per-tenant, over the persistent WS transport.** Two WS
/// connections both authenticated as `tenant-shared` (multiple clients of
/// one tenant, e.g. a CLI session and an IDE window both open against the
/// same project) each open their own session concurrently; a third WS
/// connection authenticated as a different tenant must see neither.
/// Proves the WS upgrade-time tenant header caching (`ws.rs`'s module doc
/// comment) composes correctly with concurrent multi-connection,
/// multi-tenant traffic -- not just the HTTP transport the two tests
/// above use.
#[tokio::test]
async fn two_ws_clients_of_the_same_tenant_share_sessions_while_a_third_tenant_sees_none() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let mut ws_shared_1 = connect_ws_as_tenant(addr, "tenant-shared").await;
    let mut ws_shared_2 = connect_ws_as_tenant(addr, "tenant-shared").await;
    let mut ws_other = connect_ws_as_tenant(addr, "tenant-other").await;

    let new_1 = ws_rpc(
        &mut ws_shared_1,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp/1"}}),
    )
    .await;
    let new_2 = ws_rpc(
        &mut ws_shared_2,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp/2"}}),
    )
    .await;
    let id_1 = new_1["result"]["sessionId"]
        .as_str()
        .expect("id_1")
        .to_string();
    let id_2 = new_2["result"]["sessionId"]
        .as_str()
        .expect("id_2")
        .to_string();
    assert_ne!(id_1, id_2);

    // Either connection's session/list -- same tenant -- must see *both*
    // sessions, proving the tenant, not the connection, owns the
    // aggregate view.
    for (label, ws) in [
        ("ws_shared_1", &mut ws_shared_1),
        ("ws_shared_2", &mut ws_shared_2),
    ] {
        let listed = ws_rpc(
            ws,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}),
        )
        .await;
        let seen: std::collections::HashSet<String> = listed["result"]["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: sessions array missing: {listed:?}"))
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            seen,
            std::collections::HashSet::from([id_1.clone(), id_2.clone()]),
            "{label} (tenant-shared) should see both same-tenant sessions regardless of \
             which connection created them: {seen:?}"
        );
    }

    let other_listed = ws_rpc(
        &mut ws_other,
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}),
    )
    .await;
    assert_eq!(
        other_listed["result"]["sessions"],
        json!([]),
        "a different tenant's WS connection must see neither shared-tenant session"
    );
}

/// **Documents, rather than assumes, the live-notification "last touch
/// wins" ownership rule (`acpx_core::notify`'s module doc comment) in the
/// specific multi-client-same-tenant scenario it's actually meant for.**
///
/// Two subtleties this test exists to pin down precisely (both easy to
/// get backwards by just reading the doc comment without checking
/// `ws.rs`'s actual subscribe ordering):
/// 1. `handle_socket`'s loop only calls `hub.subscribe` *after*
///    `dispatch_shared_for_tenant` has already fully returned (see
///    `ws.rs`). So a `session/update` the backend streams *during*
///    connection B's own `session/prompt` call is delivered to whoever
///    was *already* the registered subscriber at that moment -- here,
///    connection A (subscribed earlier via its own `session/new`) --
///    not to B, even though B is the connection that triggered the
///    backend to emit it.
/// 2. Only *after* B's call fully completes does B itself call
///    `hub.subscribe`, per `session_id_to_watch`'s `Proxied`-method
///    branch -- silently replacing A's registration. A subsequent
///    notification for this same gateway session now goes to B, not A:
///    proven directly against `NotificationHub::publish` here (using a
///    kept `Arc` clone of the router, exactly how a real backend's next
///    streamed chunk would route) rather than needing a second real
///    backend round trip.
/// Once both clients have touched the session, later updates must fan out
/// to both of them. This is the regression proof for session multiplexing.
#[tokio::test]
async fn same_tenant_connections_receive_the_same_later_session_updates() {
    let streaming_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-shared"}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-shared","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"from-connection-b"}}}}\n'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    let mut router = Router::new("streaming-agent");
    router.register_agent(
        "streaming-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), streaming_script.to_string()]),
    );
    let router: SharedRouter = Arc::new(Mutex::new(router));
    // Kept alongside the clone handed to `spawn_server` so this test can
    // reach `NotificationHub::publish` directly at the end, the same way
    // a real backend's own next streamed notification would.
    let router_handle = Arc::clone(&router);
    let addr = spawn_server(router).await;

    let mut ws_a = connect_ws_as_tenant(addr, "tenant-shared").await;
    let mut ws_b = connect_ws_as_tenant(addr, "tenant-shared").await;

    let new_resp = ws_rpc(
        &mut ws_a,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await;
    let gateway_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    // Connection A is now subscribed (session/new always watches the
    // session it just minted).

    // Connection B, same tenant, now prompts the same session. The
    // backend streams a chunk *during* this call -- delivered live to A
    // (still the registered subscriber at that instant), not B.
    let prompt_resp = ws_rpc(
        &mut ws_b,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        }),
    )
    .await;
    assert_eq!(prompt_resp["result"]["stopReason"], json!("end_turn"));
    // Delivered live (to A) means it was NOT also bundled into
    // `_acpx.updates` on B's own response -- `try_deliver_live` only
    // skips buffering when live delivery actually succeeds, to whoever
    // the subscriber happened to be.
    assert!(prompt_resp["_acpx"].get("updates").is_none());

    // A, the subscriber at the moment the chunk was emitted, receives it
    // as its own independent next frame (not tucked inside B's response
    // -- a genuinely separate, unsolicited JSON-RPC notification frame
    // per the whole point of ACP compatibility phase 14).
    let live_frame = tokio::time::timeout(Duration::from_secs(2), ws_a.next())
        .await
        .expect("live frame should arrive on A promptly (A was the subscriber when it fired)")
        .expect("ws stream ended early")
        .expect("ws frame error");
    let live_text = match live_frame {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let live_value: serde_json::Value = serde_json::from_str(&live_text).expect("json body");
    assert_eq!(live_value["method"], json!("session/update"));
    assert_eq!(
        live_value["params"]["update"]["content"]["text"],
        json!("from-connection-b")
    );
    assert_eq!(live_value["params"]["sessionId"], json!(gateway_id));

    // B must not have received that same chunk too -- it went to A only.
    let b_has_nothing = tokio::time::timeout(Duration::from_millis(300), ws_b.next()).await;
    assert!(
        b_has_nothing.is_err(),
        "connection B should not have received the chunk that went live to A, got: \
         {b_has_nothing:?}"
    );

    // B has now subscribed after its successful call. Publish a synthetic
    // next notification exactly as a backend would and confirm fan-out to
    // both same-tenant clients.
    let hub = { router_handle.lock().await.notification_hub() };
    let delivered = hub
        .publish(
            &TenantId::from("tenant-shared"),
            &gateway_id,
            json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": gateway_id, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "post-handoff"}}}
            }),
        )
        .await;
    assert!(delivered, "same-tenant subscribers should be registered");

    let handoff_frame = tokio::time::timeout(Duration::from_secs(2), ws_b.next())
        .await
        .expect("post-handoff frame should arrive on B promptly")
        .expect("ws stream ended early")
        .expect("ws frame error");
    let handoff_text = match handoff_frame {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let handoff_value: serde_json::Value = serde_json::from_str(&handoff_text).expect("json body");
    assert_eq!(
        handoff_value["params"]["update"]["content"]["text"],
        json!("post-handoff")
    );

    let fanout_frame = tokio::time::timeout(Duration::from_secs(2), ws_a.next())
        .await
        .expect("fan-out frame should arrive on A promptly")
        .expect("ws stream ended early")
        .expect("ws frame error");
    let fanout_text = match fanout_frame {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let fanout_value: serde_json::Value = serde_json::from_str(&fanout_text).expect("json body");
    assert_eq!(
        fanout_value["params"]["update"]["content"]["text"],
        json!("post-handoff")
    );
}
