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
mod provisioning;
mod transport;

use acpx_core::{router::Router, NotificationHub};
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
        http_bind_addr = ?config.http_bind_addr,
        acp_bridge_enabled = config.bridge.is_some(),
        startup_session_recovery_enabled = config.startup_session_recovery_enabled,
        lifecycle_reaper_enabled = config.lifecycle_reaper_enabled,
        lifecycle_reaper_interval_secs = config.lifecycle_reaper_interval.as_secs(),
        max_sessions_total = config.lifecycle.max_sessions_total,
        max_sessions_per_tenant = config.lifecycle.max_sessions_per_tenant,
        session_idle_ttl_secs = config.lifecycle.idle_session_ttl.as_secs(),
        unbound_bridge_session_ttl_secs = config.lifecycle.unbound_bridge_session_ttl.as_secs(),
        session_absolute_ttl_secs = ?config.lifecycle.absolute_session_ttl.map(|ttl| ttl.as_secs()),
        max_subscribers_per_session = config.max_subscribers_per_session,
        "starting acpx-server"
    );

    let mut router = Router::new(config.default_agent_id.clone())
        .with_lifecycle_config(config.lifecycle.clone())
        .with_notification_hub(NotificationHub::with_limits(
            256,
            config.max_subscribers_per_session,
        ));
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

    // Provisioning: providers/central-MCP-servers/profiles declared in a
    // JSON file, applied before either transport starts accepting
    // requests. See `provisioning.rs`'s doc comment -- closes the
    // COVERAGE.md-tracked "no provisioning surface" gap. Unset (the
    // default) leaves startup byte-for-byte unchanged from before this
    // was added. A malformed/rejected file fails startup outright rather
    // than booting a partially- or un-configured gateway silently.
    if let Ok(config_path) = std::env::var("ACPX_CONFIG_FILE") {
        let path = std::path::Path::new(&config_path);
        let file = provisioning::load(path)
            .unwrap_or_else(|err| panic!("ACPX_CONFIG_FILE={config_path}: {err}"));
        let summary = provisioning::apply(&mut router, file)
            .await
            .unwrap_or_else(|err| panic!("ACPX_CONFIG_FILE={config_path}: {err}"));
        tracing::info!(
            %config_path,
            providers = summary.providers,
            mcp_servers = summary.mcp_servers,
            profiles = summary.profiles,
            "applied startup provisioning file"
        );
    }

    if config.startup_session_recovery_enabled {
        #[cfg(feature = "startup-session-recovery")]
        {
            let report = router.recover_open_sessions().await?;
            tracing::info!(
                restored = report.restored,
                failed = report.failed,
                skipped = report.skipped,
                "completed startup session recovery"
            );
        }
        #[cfg(not(feature = "startup-session-recovery"))]
        {
            anyhow::bail!(
                "ACPX_STARTUP_SESSION_RECOVERY_ENABLED requests startup recovery, \
                 but this acpx-server build excludes the startup-session-recovery feature"
            );
        }
    } else {
        tracing::info!("startup session recovery disabled");
    }

    let router: transport::SharedRouter = Arc::new(Mutex::new(router));
    if config.lifecycle_reaper_enabled {
        let lifecycle_router = Arc::clone(&router);
        let interval = config.lifecycle_reaper_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // Establish interval without delaying the first full tick.
            loop {
                ticker.tick().await;
                let report = lifecycle_router
                    .lock()
                    .await
                    .reap_expired_sessions(std::time::Instant::now())
                    .await;
                if report.closed != 0 || report.failed != 0 {
                    tracing::info!(
                        closed = report.closed,
                        failed = report.failed,
                        skipped = report.skipped,
                        "completed ACPX lifecycle reaper pass"
                    );
                }
            }
        });
    }

    // stdio serves this process's own stdin/stdout (a single local
    // client); HTTP/WS serves remote clients. Both run against the same
    // `router` handle, concurrently, for the lifetime of the process.
    let stdio_router = router.clone();
    let stdio_task = tokio::spawn(async move { transport::stdio::run(stdio_router).await });
    let auth_token = config.auth_token.clone();

    // HTTP/WS bind is attempted here (rather than inside `transport::serve`)
    // so a bind failure -- or an explicit `ACPX_HTTP_BIND=off`/`none` -- can
    // fall back to a stdio-only process instead of killing the whole thing.
    // This matters for exactly the case documented on `ServerConfig::
    // http_bind_addr`: an ACP client that spawns `acpx-server` itself as a
    // per-conversation stdio subprocess (OpenHands's `ACPAgent` is one
    // concrete example) may launch several concurrent instances on one
    // host, all contending for the same fixed default port purely by
    // accident -- none of them need the HTTP/WS surface at all, so losing
    // that race must not break their (only actually used) stdio transport.
    let http_listener = match config.http_bind_addr {
        Some(bind_addr) => match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!(
                    %err,
                    %bind_addr,
                    "failed to bind HTTP/WS transport, continuing stdio-only -- \
                     see ServerConfig::http_bind_addr's doc comment (set \
                     ACPX_HTTP_BIND to a free port, or to \"off\"/\"none\" to \
                     silence this warning, if HTTP/WS is not needed)"
                );
                None
            }
        },
        None => {
            tracing::info!(
                "ACPX_HTTP_BIND=off/none: HTTP/WS transport disabled, serving stdio only"
            );
            None
        }
    };

    let bridge_config = config.bridge.clone();
    let http_task = http_listener.map(move |listener| {
        let bridge = bridge_config.clone();
        tokio::spawn(async move {
            transport::serve_on_with_bridge(listener, router, auth_token, bridge).await
        })
    });

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
    match http_task {
        Some(mut http_task) => {
            tokio::select! {
                result = stdio_task => {
                    result??;
                    (&mut http_task).await??;
                }
                result = &mut http_task => {
                    result??;
                }
            }
        }
        // HTTP/WS disabled or unbindable: nothing to select against, just
        // run stdio to completion (its own EOF-vs-error distinction is
        // handled inside `transport::stdio::run` already).
        None => {
            stdio_task.await??;
        }
    }
    Ok(())
}
