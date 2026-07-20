//! **Regression: `process_reader_demux` startup-batch-recovery panic
//! gap.**
//!
//! Same bug class already closed at `dispatch_proxied_shared`,
//! `dispatch_session_fork_shared`, `dispatch_session_list_real_shared`,
//! `backend_idle_scavenger`, `reap_expired_sessions`, and
//! `probe_adapter_capabilities` -- but for `execute_open_session_
//! recovery` (the shared free function `recover_open_sessions_shared`'s
//! `run_recovery_candidate` calls for every durable session row this
//! process's own in-memory registry doesn't already have) specifically.
//! In real production `main.rs` ordering, startup batch recovery
//! finishes before either transport binds its listener, so nothing can
//! race it into activating `process_reader_demux` on a shared backend
//! it is still reading from -- but that ordering is an operational
//! invariant, not something this function itself enforces or can rely
//! on (two concurrent recovery candidates sharing one agent's backend
//! process is already the realistic multi-session shape this suite
//! exercises elsewhere, and nothing stops a future caller -- an
//! admin-triggered re-recovery, a differently-ordered embedding --
//! invoking this against a backend a live session already demuxed).
//! Pins the exact same unconditional `read_matching_response` ->
//! `reader_mut()` panic this whole bug class is about, this time via
//! `recover_open_sessions_shared`'s own public entry point.

use std::sync::Arc;
use std::time::Duration;

use acpx_conductor::SpawnSpec;
use acpx_core::persistence::PersistenceStore;
use acpx_core::router::{dispatch_shared, StartupRecoveryPolicy};
use acpx_core::{recover_open_sessions_shared, Router};
use serde_json::json;
use tokio::sync::Mutex;

const BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if [ -z "$id" ]; then
    id=$(echo "$line" | sed -n 's/.*"id":\("[^"]*"\).*/\1/p')
  fi
  if echo "$line" | grep -q '"method":"session/new"'; then
    n=$(cat "$COUNTER_FILE" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$COUNTER_FILE"
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-session-%s"}}\n' "$id" "$n"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

fn router_with(store: PersistenceStore, counter_file: &std::path::Path) -> Router {
    let mut router = Router::new("stand-in")
        .with_process_reader_demux(true)
        .with_persistence(store);
    let mut spec = SpawnSpec::new("sh", vec!["-c".to_string(), BACKEND.to_string()]);
    spec.env.insert(
        "COUNTER_FILE".to_string(),
        counter_file.display().to_string(),
    );
    router.register_agent("stand-in", spec);
    router
}

async fn wait_for_session_row(store: &PersistenceStore, gateway_id: &str) {
    for _ in 0..150 {
        if store
            .get_session(gateway_id.to_string())
            .await
            .expect("get_session")
            .is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {gateway_id} never landed in the persistence store");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_batch_recovery_works_against_a_backend_that_already_has_demux_active() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let counter_file =
        std::env::temp_dir().join(format!("acpx-startup-recovery-demux-{}", uuid::Uuid::new_v4()));

    // Session A is created and durably persisted (`recovery_method:
    // Load`) on one router instance ...
    let router1 = Arc::new(Mutex::new(router_with(store.clone(), &counter_file)));
    let session_a = dispatch_shared(
        &router1,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new for session A");
    let gateway_a = session_a["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_string();
    wait_for_session_row(&store, &gateway_a).await;

    // ... then a second, independent router shares the same durable
    // store and the same agent, but has never itself seen session A
    // (its in-memory registry is empty for it) -- exactly the shape
    // `recover_open_sessions_shared`'s real caller in `main.rs` expects.
    let router2 = Arc::new(Mutex::new(router_with(store.clone(), &counter_file)));

    // A live session on this second router activates `process_reader_
    // demux` for the shared "stand-in" backend process first.
    dispatch_shared(
        &router2,
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new for session C activates process-reader-demux");

    // Startup batch recovery against session A's durable row must still
    // succeed against that same already-demuxed shared backend, not
    // panic.
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        recover_open_sessions_shared(&router2, StartupRecoveryPolicy::default()),
    )
    .await
    .expect("must not hang")
    .expect("recovery must not error");
    assert_eq!(report.restored, 1, "must not panic once demux is already active");
    assert_eq!(report.failed, 0);

    let _ = tokio::fs::remove_file(&counter_file).await;
}
