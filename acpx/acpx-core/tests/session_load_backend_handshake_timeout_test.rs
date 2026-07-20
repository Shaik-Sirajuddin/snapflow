//! **Fix regression coverage for a real, previously-unfixed gap.**
//!
//! `ensure_backend_initialized` (`router.rs`) used to perform the ACP
//! `initialize` handshake with a bare `proc.reader.read_value().await` --
//! no timeout of its own, unlike the bounded read that follows it
//! (`read_matching_response_with_idle_timeout`). A backend that never
//! answers `initialize` left `session/load` (and `session/new`,
//! `session/prompt`, `session/resume`) hanging forever, holding the
//! per-process `BackendProcess` lock for the daemon's lifetime -- this
//! matched a real live incident: `"bridge session binding is in
//! progress; retry the request"` never clearing, with the router's own
//! idle scavenger later logging `"acpx idle scavenger saw an id-bearing
//! frame with no in-flight caller; ignoring"` for the orphaned backend
//! reply.
//!
//! Fixed by `BACKEND_HANDSHAKE_TIMEOUT`: `ensure_backend_initialized`'s
//! handshake reads are now bounded, killing the wedged process and
//! returning `RouterError::BackendHandshakeTimeout` instead of hanging.
//! `router.rs`'s own `backend_handshake_timeout_kills_a_wedged_process_
//! and_frees_the_lock` unit test proves the low-level mechanism (with a
//! millisecond-scale timeout, via a private test-only seam). This file
//! proves the fix is actually wired through the *public* `Router::
//! dispatch` entry point for `session/load` specifically -- the exact
//! method a bridge/native client is told to retry after a restart or
//! idle-evict (`acp_bridge.rs`'s `BindingInProgress`/`SessionRestoring`
//! messaging) -- at the real, unshortened 30-second production timeout,
//! since `Router::dispatch`'s public API has no seam to inject a shorter
//! one. See `bridge_binding_eventually_fails_cleanly_and_stops_livelocking_
//! when_the_backend_never_answers_initialize` in `acpx-server/src/
//! transport/acp_bridge.rs` for the end-to-end bridge-layer proof that
//! the `BindingInProgress` livelock itself is now broken, and
//! `acp_bridge_binary_test.rs` for the real-process, real-HTTP version of
//! the same proof.

use acpx_core::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    PersistenceStore,
};
use acpx_core::router::{Router, RouterError};
use serde_json::json;
use std::time::{Duration, Instant};

/// Reads and discards every line forever, never writing a single byte
/// back -- the same "wedged backend" idiom used by `router.rs`'s own
/// handshake/idle-timeout kill tests. A real `initialize` request sent
/// to it is therefore guaranteed to never be answered.
fn silent_backend_spec() -> acpx_conductor::SpawnSpec {
    acpx_conductor::SpawnSpec::new("sh", vec!["-c".to_string(), "cat > /dev/null".to_string()])
}

/// Seeds a durable, "already open" session row directly -- no
/// `session/new` round trip against the backend ever happens, so the
/// very first thing that touches this agent's `BackendProcess` is
/// `session/load`'s own `ensure_backend_initialized` call, exactly
/// matching a freshly restarted `acpx-server` receiving a real client's
/// `session/load` for a session it never itself created.
async fn seed_open_session_row(store: &PersistenceStore, gateway_id: &str) {
    store
        .record_session_with_recovery(
            gateway_id,
            "stand-in-agent",
            "backend-abc",
            None,
            "2026-07-19T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/tmp".to_string()),
                recovery_params: Some(json!({"cwd": "/tmp"})),
                status: RecoveryStatus::Restored,
                recovery_method: RecoveryMethod::Load,
                ..RecoveryMetadata::default()
            },
        )
        .await
        .expect("seed open session row");
}

/// **Proves the fix, not just the gap.** `session/load` against a
/// durable session whose backend never completes the `initialize`
/// handshake now resolves -- with a clear `BackendHandshakeTimeout`
/// error -- instead of hanging forever. Bounded in a generous outer
/// `tokio::time::timeout` (comfortably above the real 30-second
/// production constant) so a regression back to the pre-fix unbounded
/// hang still fails this test deterministically rather than wedging the
/// whole suite; asserts the call took at least ~30s so a future
/// accidental short-circuit (e.g. a bug that returns instantly instead
/// of genuinely exercising the timeout) would also be caught.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_load_fails_cleanly_after_the_handshake_timeout_instead_of_hanging_forever() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    seed_open_session_row(&store, "gateway-wedged").await;

    let mut router = Router::new("stand-in-agent").with_persistence(store);
    router.register_agent("stand-in-agent", silent_backend_spec());

    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/load",
            "params": {"sessionId": "gateway-wedged", "cwd": "/tmp"}
        })),
    )
    .await
    .expect(
        "session/load did not resolve within 40s -- ensure_backend_initialized's handshake \
         timeout regressed back to an unbounded hang",
    );

    assert!(
        started.elapsed() >= Duration::from_secs(28),
        "session/load resolved suspiciously fast ({:?}) -- expected it to genuinely wait out \
         BACKEND_HANDSHAKE_TIMEOUT (30s), not short-circuit some other way",
        started.elapsed()
    );
    assert!(
        matches!(
            outcome,
            Err(RouterError::BackendHandshakeTimeout("initialize", _))
        ),
        "expected RouterError::BackendHandshakeTimeout(\"initialize\", _), got {outcome:?}"
    );
}
