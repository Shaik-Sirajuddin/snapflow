//! `acpx-server` daemon entrypoint.
//!
//! Phase 2: one shared `acpx_core::router::Router` (behind
//! `transport::SharedRouter = Arc<Mutex<Router>>`) backs every transport --
//! stdio (this process's own stdin/stdout, one local client) and HTTP/WS
//! (many concurrent remote clients) run concurrently against the same
//! router, so they share one session registry and one set of supervised
//! backend processes regardless of which transport a client used. If
//! `ACPX_DB_PATH` is set, session metadata + transcripts are persisted to
//! that sqlite file (see `acpx_core::persistence`); otherwise persistence
//! is skipped entirely (`Router::with_persistence` is optional).
//!
//! One agent is registered today (`ServerConfig::default_agent_id`,
//! spawned via `ACPX_BACKEND_CMD`); Phase 3's profile store is what lets
//! `session/new`'s `_acpx.profile` select among more than one.

mod config;
mod transport;

use acpx_core::router::Router;
use config::ServerConfig;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr) // stdout is the ACP wire for the stdio transport
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = ServerConfig::from_env();
    tracing::info!(
        default_agent_id = %config.default_agent_id,
        program = %config.backend.program,
        args = ?config.backend.args,
        http_bind_addr = %config.http_bind_addr,
        "starting acpx-server"
    );

    let mut router = Router::new(config.default_agent_id.clone());
    router.register_agent(config.default_agent_id.clone(), config.backend.clone());

    if let Ok(db_path) = std::env::var("ACPX_DB_PATH") {
        match acpx_core::PersistenceStore::open(std::path::Path::new(&db_path)) {
            Ok(store) => {
                tracing::info!(%db_path, "session persistence enabled");
                router = router.with_persistence(store);
            }
            Err(err) => {
                tracing::error!(%err, %db_path, "failed to open ACPX_DB_PATH, continuing without persistence");
            }
        }
    }

    let router: transport::SharedRouter = Arc::new(Mutex::new(router));

    // stdio serves this process's own stdin/stdout (a single local
    // client); HTTP/WS serves remote clients. Both run against the same
    // `router` handle, concurrently, for the lifetime of the process.
    let stdio_router = router.clone();
    let stdio_task = tokio::spawn(async move { transport::stdio::run(stdio_router).await });
    let mut http_task =
        tokio::spawn(async move { transport::serve(router, config.http_bind_addr).await });

    // Bug fix (discovered driving the real-adapter e2e test with a
    // Stdio::null()/closed-stdin child, the same shape any daemonized
    // deployment -- systemd, nohup, a supervisor that doesn't attach a
    // local stdio client at all -- uses): stdio hitting EOF is a normal,
    // expected event (no local client, or a local client that
    // disconnected) and must NOT tear down the HTTP/WS transport, which
    // may still be serving remote clients. Only a real stdio *error*
    // should end the whole process early; clean stdio completion instead
    // falls through to just waiting on `http_task` alone (selected here
    // by `&mut` -- std's blanket `impl Future for &mut F where F: Future
    // + Unpin` -- so the same handle can still be awaited again below).
    tokio::select! {
        result = stdio_task => {
            result??;
            (&mut http_task).await??;
        }
        result = &mut http_task => {
            result??;
        }
    }
    Ok(())
}
