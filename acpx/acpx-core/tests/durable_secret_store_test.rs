//! End-to-end coverage for `durable_secret_and_configuration_store`
//! (`acp-gateway-daemon`'s `architecture_remediation` phase): profiles,
//! providers, MCP servers, and encrypted secret material must survive a
//! real process restart (a fresh `Router` reopening the same sqlite
//! file), and a persisted secret must actually be encrypted at rest, not
//! merely opaque-by-convention. Same synthetic `sh`-script-backend +
//! "echo back what it observed" trick as `profile_resolution_test.rs`
//! (see that file's doc comment) -- reused here so a restart round trip
//! can assert the *reloaded* profile's secret/provider/mcp-server config
//! actually re-injects correctly into a freshly spawned backend, not
//! just that `profiles/list` echoes the right-looking JSON back.

use acpx_conductor::SpawnSpec;
use acpx_core::provider::{ProviderConfig, ProviderKind};
use acpx_core::router::Router;
use acpx_core::PersistenceStore;
use serde_json::json;

const OBSERVING_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  saw_central=false
  echo "$line" | grep -q '"name":"central-fs"' && saw_central=true
  saw_base_url=false
  case "${CODEX_CONFIG:-}" in *"https://litellm.example.com/v1"*) saw_base_url=true ;; esac
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc","observedApiKey":"%s","observedConfigHasBaseUrl":%s,"sawCentralFs":%s}}\n' \
    "$id" "${CODEX_API_KEY:-}" "$saw_base_url" "$saw_central"
done
"#;

fn observing_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), OBSERVING_BACKEND_SCRIPT.to_string()],
    )
}

/// Build a router wired to `db_path`/`keyring_path`, with durable config
/// enabled and the observing stand-in backend registered -- the common
/// setup every test below needs both before and after its simulated
/// restart.
async fn durable_router(db_path: &std::path::Path, keyring_path: &std::path::Path) -> Router {
    let store = PersistenceStore::open(db_path).expect("open sqlite db");
    let mut router = Router::new("stand-in-agent").with_persistence(store);
    router
        .enable_durable_config(keyring_path.to_path_buf())
        .await
        .expect("enable_durable_config");
    router.register_agent("stand-in-agent", observing_backend_spec());
    router
}

#[tokio::test]
async fn profile_secret_and_provider_survive_a_simulated_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("acpx.sqlite");
    let keyring_path = dir.path().join("master.keyring");

    {
        let mut router = durable_router(&db_path, &keyring_path).await;
        router.register_provider(ProviderConfig {
            name: "litellm-proxy".to_string(),
            kind: ProviderKind::LiteLlm,
            base_url: Some("https://litellm.example.com/v1".to_string()),
        });
        // `register_provider`'s durability mirror is fire-and-forget
        // (`tokio::spawn`, see its doc comment) -- yield once so the
        // spawned write actually lands before this scope (and its
        // sqlite connection) closes.
        tokio::task::yield_now().await;

        router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 1, "method": "mcp_servers/create",
                "params": {"name": "central-fs", "command": "npx", "args": ["-y", "server-filesystem"]}
            }))
            .await
            .expect("mcp_servers/create");

        let create = router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 2, "method": "profiles/create",
                "params": {
                    "name": "work-litellm",
                    "agent_id": "stand-in-agent",
                    "provider": "litellm-proxy",
                    "secret": "sk-durable-secret",
                    "mcp_servers": ["central-fs"]
                }
            }))
            .await
            .expect("profiles/create");
        assert!(
            create["result"]["key_ref"].is_string(),
            "profiles/create should have minted a key_ref: {create:?}"
        );

        // Sanity check before ever restarting: the freshly created
        // profile resolves end to end against a live spawned backend.
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/new",
                "params": {"cwd": "/tmp", "_acpx": {"profile": "work-litellm"}}
            }))
            .await
            .expect("session/new before restart");
        assert_eq!(response["result"]["observedApiKey"], "sk-durable-secret");
        assert_eq!(response["result"]["observedConfigHasBaseUrl"], true);
        assert_eq!(response["result"]["sawCentralFs"], true);
    } // `router` (and its `PersistenceStore`'s sqlite connection) drops here.

    // Simulate a real daemon restart: a brand new `Router`, a brand new
    // `PersistenceStore::open` of the *same* file (not a clone of the
    // still-live connection above), reloading everything from disk.
    let mut router = durable_router(&db_path, &keyring_path).await;

    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 4, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list after restart");
    let profiles = list["result"]["profiles"]
        .as_array()
        .expect("profiles array");
    assert!(
        profiles.iter().any(|p| p["name"] == "work-litellm"),
        "persisted profile missing after restart: {profiles:?}"
    );

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "work-litellm"}}
        }))
        .await
        .expect("session/new after restart");
    assert_eq!(
        response["result"]["observedApiKey"], "sk-durable-secret",
        "decrypted secret must still inject correctly after restart: {response:?}"
    );
    assert_eq!(response["result"]["observedConfigHasBaseUrl"], true);
    assert_eq!(response["result"]["sawCentralFs"], true);
}

#[tokio::test]
async fn secret_ciphertext_at_rest_never_contains_the_plaintext() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("acpx.sqlite");
    let keyring_path = dir.path().join("master.keyring");

    let mut router = durable_router(&db_path, &keyring_path).await;
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "work",
                "agent_id": "stand-in-agent",
                "secret": "sk-should-never-appear-in-plaintext"
            }
        }))
        .await
        .expect("profiles/create");

    // Read the raw sqlite row directly -- bypassing every acpx
    // abstraction on purpose, to prove what is actually on disk, not
    // what a decrypting reader would hand back.
    let conn = rusqlite::Connection::open(&db_path).expect("open db directly");
    let ciphertext: Vec<u8> = conn
        .query_row("SELECT ciphertext FROM secrets LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("one secret row");
    let as_text = String::from_utf8_lossy(&ciphertext);
    assert!(
        !as_text.contains("sk-should-never-appear-in-plaintext"),
        "ciphertext must not contain the raw secret: {as_text:?}"
    );
    assert_ne!(
        ciphertext,
        b"sk-should-never-appear-in-plaintext".to_vec(),
        "ciphertext must differ from the plaintext bytes"
    );
}

#[tokio::test]
async fn rotate_master_key_reencrypts_every_secret_and_keeps_it_resolvable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("acpx.sqlite");
    let keyring_path = dir.path().join("master.keyring");

    let mut router = durable_router(&db_path, &keyring_path).await;
    router.register_provider(ProviderConfig {
        name: "openai-default".to_string(),
        kind: ProviderKind::OpenAi,
        base_url: None,
    });
    // See `register_provider`'s doc comment: its durability mirror is a
    // fire-and-forget `tokio::spawn`, so give it a chance to land before
    // this test's later restart reopens the same sqlite file.
    tokio::task::yield_now().await;
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "work",
                "agent_id": "stand-in-agent",
                "provider": "openai-default",
                "secret": "sk-rotate-me"
            }
        }))
        .await
        .expect("profiles/create");

    let (version_before, ciphertext_before): (i64, Vec<u8>) = {
        let conn = rusqlite::Connection::open(&db_path).expect("open db directly");
        conn.query_row(
            "SELECT key_version, ciphertext FROM secrets LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("one secret row")
    };

    let new_version = router.rotate_master_key().await.expect("rotate_master_key");
    assert!(new_version as i64 > version_before);

    let (version_after, ciphertext_after): (i64, Vec<u8>) = {
        let conn = rusqlite::Connection::open(&db_path).expect("open db directly");
        conn.query_row(
            "SELECT key_version, ciphertext FROM secrets LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("one secret row")
    };
    assert_eq!(version_after, new_version as i64);
    assert_ne!(
        ciphertext_before, ciphertext_after,
        "rotation must re-encrypt (different nonce/key), not just relabel the version"
    );

    // The secret must still resolve correctly through the live router
    // (in-memory `Keystore` re-encrypted in place)...
    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "work"}}
        }))
        .await
        .expect("session/new after rotation");
    assert_eq!(response["result"]["observedApiKey"], "sk-rotate-me");

    // ...and after a full restart against the now-rotated on-disk
    // keyring, proving the rotated keyring file (not just the in-memory
    // one) is what a real restart would load.
    let mut restarted = durable_router(&db_path, &keyring_path).await;
    let response = restarted
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "work"}}
        }))
        .await
        .expect("session/new after restart with rotated keyring");
    assert_eq!(response["result"]["observedApiKey"], "sk-rotate-me");
}

#[tokio::test]
async fn without_enable_durable_config_nothing_survives_a_restart() {
    // Back-compat guard: `with_persistence` alone (no `enable_durable_
    // config`) must keep behaving exactly as it always did -- sessions/
    // transcripts persist, but profiles/providers/mcp_servers/secrets
    // stay in-memory-only, matching every pre-existing caller's
    // expectations.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("acpx.sqlite");

    {
        let store = PersistenceStore::open(&db_path).expect("open sqlite db");
        let mut router = Router::new("stand-in-agent").with_persistence(store);
        router.register_agent("stand-in-agent", observing_backend_spec());
        router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
                "params": {"name": "work", "agent_id": "stand-in-agent", "secret": "sk-in-memory-only"}
            }))
            .await
            .expect("profiles/create");
    }

    let store = PersistenceStore::open(&db_path).expect("reopen sqlite db");
    let mut router = Router::new("stand-in-agent").with_persistence(store);
    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    let profiles = list["result"]["profiles"]
        .as_array()
        .expect("profiles array");
    assert!(
        !profiles.iter().any(|p| p["name"] == "work"),
        "a profile must not survive a restart unless enable_durable_config was called: {profiles:?}"
    );
}
