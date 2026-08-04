//! Real end-to-end coverage for the `pool-capability-fix` regression:
//! `acpx_client::pool::PoolKey` deliberately excludes thread identity
//! (see its own doc comment -- "sessions under the same key are
//! compatible to reuse"), so an idle pooled session one thread leaves
//! behind can be handed to a *different* thread next. Before this fix,
//! `thread_actor.rs`'s `Command::SetConfigOption` mutated the real
//! backend session's config with no reset on release, so that next
//! tenant silently inherited whatever config the previous one left.
//!
//! Spawns the real `acpx-server` + `rui-mock-agent` binaries (same
//! "spawn the real binary, don't fake the boundary" discipline as
//! `gateway_actor_e2e_test.rs`) and drives two independent
//! `AcpxThreadHandle`s sharing one `ProjectSessionPool`, purely through
//! `rui-acp-client`'s public API -- proving the fix at the same layer
//! the bug lived in, not through the UI.

use acpx_client::pool::{PoolKey, ProjectSessionPool};
use acpx_client::Gateway;
use panel_rust::gateway_actor::{
    provider_profile_key, spawn_acpx_thread_with_gateway_and_pool, AgentEvent,
    GatewaySessionOpener, SharedSessionPool,
};
use panel_rust::protocol_types::ConfigOptionInfo;
use std::collections::BTreeMap;
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

mod common;
use common::{free_port, provision_mock_profile, spawn_acpx_server_with_retry};

struct GatewayProcess {
    child: Child,
    base_url: String,
    profile_name: String,
}

impl GatewayProcess {
    async fn spawn(persona: &str, db_path: &std::path::Path) -> Self {
        let persona = persona.to_string();
        let persona_for_profile = persona.clone();
        let db_path = db_path.to_path_buf();
        let admin_port = free_port();
        let admin_token = format!("test-admin-token-{admin_port}");
        let admin_token_for_env = admin_token.clone();
        let (child, base_url) = spawn_acpx_server_with_retry(move |command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_DEFAULT_AGENT_ID", &persona)
                .env("ACPX_DB_PATH", &db_path)
                .env("ACPX_ADMIN_TOKEN", &admin_token_for_env)
                .env("ACPX_ADMIN_BIND", format!("127.0.0.1:{admin_port}"))
                .env("RUST_LOG", "error");
        });
        let profile_name = provision_mock_profile(
            &base_url,
            admin_port,
            &admin_token,
            &persona_for_profile,
            BTreeMap::new(),
        )
        .await;
        GatewayProcess {
            child,
            base_url,
            profile_name,
        }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits (bounded) for the next `AgentEvent::ConfigOptions` and returns
/// it -- both `AcquireAndAttach` and `SetConfigOption` push one of these
/// on every real gateway round trip (see `thread_actor.rs`'s
/// `emit_capability_events`/`Command::SetConfigOption` handler).
async fn wait_for_config_options(
    rx: &mut UnboundedReceiver<AgentEvent>,
    timeout: Duration,
) -> Vec<ConfigOptionInfo> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Some(AgentEvent::ConfigOptions(options))) =
            tokio::time::timeout(remaining.min(Duration::from_millis(200)), rx.recv()).await
        {
            return options;
        }
    }
    panic!("no ConfigOptions event arrived within {timeout:?}");
}

fn model_value(options: &[ConfigOptionInfo]) -> Option<String> {
    options
        .iter()
        .find(|option| option.id == "model")
        .and_then(|option| option.current_value.clone())
}

/// Matrix row 1 (the real regression): thread A picks a non-default
/// model, releases its session in the *background* (the one path that
/// hands the session back to the pool's idle list for a different
/// thread to reuse -- see `Command::CloseSession`'s own doc comment),
/// then thread B acquires under the identical `PoolKey`. Before this
/// fix, B silently inherited A's "mock-model-b" pick; after it, B must
/// see the session's original baseline, "mock-model-a".
///
/// Matrix row 2 (no-leak-by-construction guard): the same setup, but A
/// tears its session down with a *non-background* close instead
/// (`Command::CloseSession{background:false}` -> `pool.invalidate`,
/// never re-enters the idle list). The next acquire under the same key
/// must get a genuinely new session id, proving this path never had a
/// leak to begin with -- nothing carries over because nothing is reused.
#[tokio::test]
async fn pooled_session_config_does_not_leak_across_threads_on_background_release() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let gateway_process = GatewayProcess::spawn("codex", &db_dir.path().join("acpx.sqlite3")).await;
    let shared_gateway = Arc::new(Gateway::connect(gateway_process.base_url.clone()).await);

    let opener = GatewaySessionOpener::new(Arc::clone(&shared_gateway), serde_json::json!([]));
    let pool: SharedSessionPool = Arc::new(ProjectSessionPool::new(opener));

    let project_dir = std::env::current_dir().expect("cwd");
    let key = PoolKey::new(
        project_dir.to_string_lossy().into_owned(),
        "codex",
        provider_profile_key(Some(&gateway_process.profile_name)),
    );

    // -- Thread A: acquire, pick a non-default model, release in the
    // background (idle-pool-eligible).
    let mut thread_a =
        spawn_acpx_thread_with_gateway_and_pool(Arc::clone(&shared_gateway), pool.clone());
    let mut events_a = thread_a.take_events();
    let attach_a = thread_a
        .acquire_and_attach(
            key.clone(),
            "thread-a",
            None,
            project_dir.clone(),
            Vec::new(),
        )
        .await
        .expect("thread A acquire_and_attach");
    let baseline = wait_for_config_options(&mut events_a, Duration::from_secs(5)).await;
    assert_eq!(
        model_value(&baseline).as_deref(),
        Some("mock-model-a"),
        "fresh pooled session should start at the mock agent's real default"
    );

    thread_a
        .set_config_option("model", serde_json::json!("mock-model-b"))
        .await
        .expect("thread A set_config_option");
    let after_override = wait_for_config_options(&mut events_a, Duration::from_secs(5)).await;
    assert_eq!(
        model_value(&after_override).as_deref(),
        Some("mock-model-b"),
        "set_config_option must actually change the live backend session's config"
    );

    thread_a
        .close_session(true)
        .await
        .expect("thread A background close_session");

    // -- Thread B: acquire under the identical key. `ProjectSessionPool::
    // acquire`'s fast path takes the first `Idle` entry for this key
    // (see pool.rs) and nothing else has touched this key yet, so this
    // reliably reuses thread A's exact session rather than a fresh one --
    // asserted below so a future pool-internals change that broke that
    // premise would fail loudly here instead of this test silently
    // proving nothing.
    let mut thread_b =
        spawn_acpx_thread_with_gateway_and_pool(Arc::clone(&shared_gateway), pool.clone());
    let mut events_b = thread_b.take_events();
    let attach_b = thread_b
        .acquire_and_attach(
            key.clone(),
            "thread-b",
            None,
            project_dir.clone(),
            Vec::new(),
        )
        .await
        .expect("thread B acquire_and_attach");
    assert_eq!(
        attach_b.session_id, attach_a.session_id,
        "thread B must reuse thread A's exact pooled session for this test to exercise the leak path at all"
    );

    let reused_options = wait_for_config_options(&mut events_b, Duration::from_secs(5)).await;
    assert_eq!(
        model_value(&reused_options).as_deref(),
        Some("mock-model-a"),
        "PASS/FAIL gate for the fix: a reused pooled session must present its baseline config to \
         the new owning thread, not the previous thread's override -- a value of \"mock-model-b\" \
         here means thread_actor.rs's reset-on-release regressed"
    );

    // -- Matrix row 2: tear this session down for real (non-background),
    // pick a divergent override on the *next* session too, and confirm a
    // fresh acquire never reuses it -- the invalidate path has no idle
    // entry to leak through in the first place.
    thread_b
        .set_config_option("model", serde_json::json!("mock-model-b"))
        .await
        .expect("thread B set_config_option");
    let _ = wait_for_config_options(&mut events_b, Duration::from_secs(5)).await;
    thread_b
        .close_session(false)
        .await
        .expect("thread B non-background close_session (invalidate)");

    let mut thread_c =
        spawn_acpx_thread_with_gateway_and_pool(Arc::clone(&shared_gateway), pool.clone());
    let mut events_c = thread_c.take_events();
    let attach_c = thread_c
        .acquire_and_attach(
            key.clone(),
            "thread-c",
            None,
            project_dir.clone(),
            Vec::new(),
        )
        .await
        .expect("thread C acquire_and_attach");
    assert_ne!(
        attach_c.session_id, attach_b.session_id,
        "a non-background close must invalidate the entry, never hand it back for reuse"
    );
    let fresh_options = wait_for_config_options(&mut events_c, Duration::from_secs(5)).await;
    assert_eq!(
        model_value(&fresh_options).as_deref(),
        Some("mock-model-a"),
        "a genuinely fresh session must start at the real default regardless of any prior thread's picks"
    );
}

/// Matrix row 3: the pre-session capability *preview* path
/// (`AgentBridge::ensure_models_for_provider`, mirrored here directly
/// against the pool since `AgentBridge` needs a full panel fixture) is
/// read-only -- `pool.acquire`/`pool.release` with no `session/set_
/// config_option` in between. It reports whatever `SessionLease::
/// capabilities` was captured at that entry's *creation* time (pool.rs:
/// "carried into SessionLease::capabilities... a thread that acquires a
/// freshly-created or previously-warmed session can still populate its
/// model/mode/agent config options"), which this test proves is
/// immutable across repeated preview acquisitions even after a real
/// attached thread changes the live session's config in between -- i.e.
/// a not-yet-started thread's compose dropdown reflects a real,
/// unmodified default, never a stranger thread's live in-progress pick,
/// because the preview path never mutates or re-reads live state.
#[tokio::test]
async fn capability_preview_never_reflects_a_live_config_change_made_between_previews() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let gateway_process = GatewayProcess::spawn("codex", &db_dir.path().join("acpx.sqlite3")).await;
    let shared_gateway = Arc::new(Gateway::connect(gateway_process.base_url.clone()).await);

    let opener = GatewaySessionOpener::new(Arc::clone(&shared_gateway), serde_json::json!([]));
    let pool: SharedSessionPool = Arc::new(ProjectSessionPool::new(opener));

    let project_dir = std::env::current_dir().expect("cwd");
    let key = PoolKey::new(
        project_dir.to_string_lossy().into_owned(),
        "codex",
        provider_profile_key(Some(&gateway_process.profile_name)),
    );

    // Preview #1, before anything has touched this key: acquire/release
    // immediately, exactly like `ensure_models_for_provider`.
    let preview_one = pool
        .acquire(
            key.clone(),
            "preview:0".to_string(),
            acpx_client::pool::OpenSpec {
                saved_session_id: None,
            },
        )
        .await
        .expect("preview acquire #1");
    let preview_one_options = preview_one
        .capabilities
        .as_ref()
        .and_then(|value| value.get("configOptions"))
        .and_then(panel_rust::gateway_actor::parse_config_options)
        .unwrap_or_default();
    assert_eq!(
        model_value(&preview_one_options).as_deref(),
        Some("mock-model-a")
    );
    pool.release(&preview_one)
        .await
        .expect("preview release #1");

    // A real thread now attaches to that exact session and changes its
    // live config -- the condition that used to leak into a *different
    // real thread* per row 1 above.
    let mut thread_a =
        spawn_acpx_thread_with_gateway_and_pool(Arc::clone(&shared_gateway), pool.clone());
    let mut events_a = thread_a.take_events();
    let attach_a = thread_a
        .acquire_and_attach(
            key.clone(),
            "thread-a",
            None,
            project_dir.clone(),
            Vec::new(),
        )
        .await
        .expect("thread A acquire_and_attach");
    assert_eq!(
        attach_a.session_id, preview_one.session_id,
        "thread A must land on the same entry the preview just touched for this test to mean anything"
    );
    let _ = wait_for_config_options(&mut events_a, Duration::from_secs(5)).await;
    thread_a
        .set_config_option("model", serde_json::json!("mock-model-b"))
        .await
        .expect("thread A set_config_option");
    let _ = wait_for_config_options(&mut events_a, Duration::from_secs(5)).await;

    // Preview #2, while thread A's live session is still sitting at
    // "mock-model-b" (not yet released): a second, independent preview
    // acquire for the same key must never observe that live override --
    // it's still frozen at the entry's own creation-time snapshot (and
    // in this case gets a *different*, second warm/created entry, not
    // thread A's leased one at all, since A's is not idle).
    let preview_two = pool
        .acquire(
            key.clone(),
            "preview:1".to_string(),
            acpx_client::pool::OpenSpec {
                saved_session_id: None,
            },
        )
        .await
        .expect("preview acquire #2");
    let preview_two_options = preview_two
        .capabilities
        .as_ref()
        .and_then(|value| value.get("configOptions"))
        .and_then(panel_rust::gateway_actor::parse_config_options)
        .unwrap_or_default();
    assert_eq!(
        model_value(&preview_two_options).as_deref(),
        Some("mock-model-a"),
        "a capability preview must never surface a different thread's in-progress live config change"
    );
    pool.release(&preview_two)
        .await
        .expect("preview release #2");
}
