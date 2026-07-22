//! Real, headless (no VNC, no manual step) end-to-end proof for
//! `acpx-startup-recovery-unbounded`: bulk startup session recovery must
//! never touch an unbounded pile of stale, never-gracefully-closed session
//! rows (the panel-rust-spawned-gateway shape, recovery fully disabled),
//! and even when recovery IS enabled (the bundled-daemon shape), it must
//! only ever attempt sessions within `startup_recovery_max_age`, not every
//! row ever written to the database.

use std::process::Stdio;
use std::time::Duration;

use acpx_core::persistence::sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus};
use acpx_core::PersistenceStore;
use tokio::process::Command;

struct BinaryGuard {
    child: tokio::process::Child,
    database: std::path::PathBuf,
}

impl Drop for BinaryGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.database);
    }
}

fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    );
    std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

fn unix_time_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos() as i64
}

/// Same "read the id (string or number) out of the request line, reply
/// with a bare sessionId" shape `admin_test.rs`'s stand-in backends use,
/// generalized to also handle startup recovery's *string* request ids
/// (`"acpx-startup-recovery:<gateway_session_id>"`, not the small
/// integers a real client-driven `session/new` uses) -- a naive
/// `grep -o '"id":[0-9]*'` silently drops the whole recovery request.
fn write_stand_in_backend_script(path: &std::path::Path) {
    std::fs::write(
        path,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(echo \"$line\" | sed -n 's/.*\"id\":\\(\"[^\"]*\"\\|[0-9][0-9]*\\).*/\\1/p')\n  printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"recovered\"}}\\n' \"$id\"\ndone\n",
    )
    .expect("write stand-in backend script");
}

async fn seed_session(
    store: &PersistenceStore,
    gateway_session_id: &str,
    age: Duration,
) {
    let created_at_unix_nanos = unix_time_nanos() - age.as_nanos() as i64;
    store
        .record_session_with_recovery(
            gateway_session_id,
            "codex-acp",
            format!("backend-{gateway_session_id}"),
            None,
            "2026-07-21T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: None,
                recovery_params: None,
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Load,
                last_recovery_error: None,
                created_at_unix_nanos: Some(created_at_unix_nanos),
                last_activity_at_unix_nanos: Some(created_at_unix_nanos),
                pinned: false,
                bridge_session_id: None,
                bridge_model_alias: None,
                bridge_config_options: None,
            },
        )
        .await
        .expect("seed session record");
}

async fn wait_for_ready(admin_address: std::net::SocketAddr, admin_token: &str) {
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if let Ok(response) = client
            .get(format!(
                "http://{admin_address}/admin/sessions/count?tenant=default"
            ))
            .bearer_auth(admin_token)
            .send()
            .await
        {
            if response.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("acpx-server admin transport never became ready at {admin_address}");
}

async fn session_count(admin_address: std::net::SocketAddr, admin_token: &str) -> u64 {
    let response = reqwest::Client::new()
        .get(format!(
            "http://{admin_address}/admin/sessions/count?tenant=default"
        ))
        .bearer_auth(admin_token)
        .send()
        .await
        .expect("GET session count")
        .json::<serde_json::Value>()
        .await
        .expect("parse session count response");
    response["count"].as_u64().expect("count field")
}

/// The panel-rust-spawned-gateway shape: `ACPX_STARTUP_SESSION_RECOVERY_
/// ENABLED=0` (the actual fix in `agent_bridge.rs::spawn_gateway_process`)
/// against a database with thousands-worth-of-pattern stale rows (using a
/// handful here, standing in for the real 4367 confirmed live) must
/// recover exactly none of them.
#[tokio::test]
async fn recovery_disabled_spawn_shape_recovers_nothing_from_a_stale_database() {
    let database = unique_temp_path("acpx-recovery-disabled-test.sqlite");
    {
        let store = PersistenceStore::open(&database).expect("seed database");
        for i in 0..5 {
            seed_session(
                &store,
                &format!("stale-session-{i}"),
                Duration::from_secs(2 * 24 * 60 * 60),
            )
            .await;
        }
    }

    let admin_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind admin probe");
        let address = listener.local_addr().expect("admin probe address");
        drop(listener);
        address
    };
    let client_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind client probe");
        let address = listener.local_addr().expect("client probe address");
        drop(listener);
        address
    };
    let script_path = unique_temp_path("acpx-recovery-disabled-backend.sh");
    write_stand_in_backend_script(&script_path);

    let mut command = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    command
        .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_DEFAULT_AGENT_ID", "codex-acp")
        .env("ACPX_HTTP_BIND", client_address.to_string())
        .env("ACPX_ADMIN_TOKEN", "admin-secret")
        .env("ACPX_ADMIN_BIND", admin_address.to_string())
        .env("ACPX_DB_PATH", database.display().to_string())
        .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().expect("spawn real acpx-server");
    let _server = BinaryGuard { child, database };

    wait_for_ready(admin_address, "admin-secret").await;
    // Give a real, disabled-recovery startup a moment to prove it does
    // *not* eventually recover anything either -- not just that it
    // hasn't finished yet.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        session_count(admin_address, "admin-secret").await,
        0,
        "ACPX_STARTUP_SESSION_RECOVERY_ENABLED=0 must recover zero sessions, \
         regardless of how many stale rows sit in the database"
    );
}

/// The bundled-daemon shape: recovery enabled, but bounded by
/// `startup_recovery_max_age`. A database mixing an old and a recent
/// session must only ever recover the recent one.
#[tokio::test]
async fn recovery_enabled_with_max_age_only_recovers_the_recent_session() {
    let database = unique_temp_path("acpx-recovery-bounded-test.sqlite");
    {
        let store = PersistenceStore::open(&database).expect("seed database");
        seed_session(
            &store,
            "old-session",
            Duration::from_secs(2 * 24 * 60 * 60),
        )
        .await;
        seed_session(&store, "recent-session", Duration::from_secs(5 * 60)).await;
    }

    let admin_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind admin probe");
        let address = listener.local_addr().expect("admin probe address");
        drop(listener);
        address
    };
    let client_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind client probe");
        let address = listener.local_addr().expect("client probe address");
        drop(listener);
        address
    };
    let script_path = unique_temp_path("acpx-recovery-bounded-backend.sh");
    write_stand_in_backend_script(&script_path);

    let mut command = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    command
        .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_DEFAULT_AGENT_ID", "codex-acp")
        .env("ACPX_HTTP_BIND", client_address.to_string())
        .env("ACPX_ADMIN_TOKEN", "admin-secret")
        .env("ACPX_ADMIN_BIND", admin_address.to_string())
        .env("ACPX_DB_PATH", database.display().to_string())
        .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "1")
        .env("ACPX_STARTUP_RECOVERY_MAX_AGE_SECONDS", "86400")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().expect("spawn real acpx-server");
    let _server = BinaryGuard { child, database };

    wait_for_ready(admin_address, "admin-secret").await;
    // Recovery races startup readiness -- poll for the count to settle
    // rather than assuming a fixed-length sleep always wins that race.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut count = session_count(admin_address, "admin-secret").await;
    while count == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        count = session_count(admin_address, "admin-secret").await;
    }

    assert_eq!(
        count, 1,
        "a 24h startup-recovery bound must recover only the 5-minute-old \
         session, not the 2-day-old one, out of the 2 seeded"
    );
}
