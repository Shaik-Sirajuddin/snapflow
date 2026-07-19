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
//! is skipped entirely (`Router::with_persistence` is optional). Setting
//! `ACPX_DB_PATH` also enables the durable secret/config store
//! (`Router::enable_durable_config`, see `acpx_core::keystore`'s module
//! doc comment): profiles, MCP servers, providers, and encrypted secret
//! material all survive a restart too, not just session metadata.
//! `ACPX_MASTER_KEYRING_PATH` overrides the encryption keyring's path
//! (default `<ACPX_DB_PATH>.keyring`); `ACPX_MASTER_KEYRING_ROTATE=1`
//! triggers a one-shot key rotation on that startup.
//!
//! One agent is registered today (`ServerConfig::default_agent_id`,
//! spawned via `ACPX_BACKEND_CMD`); Phase 3's profile store is what lets
//! `session/new`'s `_acpx.profile` select among more than one.

mod config;
mod provisioning;
mod transport;

use acpx_core::{
    recover_open_sessions_shared, router::Router, NotificationHub, StartupRecoveryPolicy,
};
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
        native_auth_method_id = ?config.native_auth_method_id,
        program = %config.backend.program,
        args = ?config.backend.args,
        http_bind_addr = ?config.http_bind_addr,
        admin_bind_addr = ?config.admin_bind_addr,
        acp_bridge_enabled = config.bridge.is_some(),
        startup_session_recovery_enabled = config.startup_session_recovery_enabled,
        startup_session_recovery_timeout_secs = config.startup_session_recovery_timeout.as_secs(),
        startup_session_recovery_concurrency = config.startup_session_recovery_concurrency,
        startup_session_recovery_fail_fast = config.startup_session_recovery_fail_fast,
        lifecycle_reaper_enabled = config.lifecycle_reaper_enabled,
        lifecycle_reaper_interval_secs = config.lifecycle_reaper_interval.as_secs(),
        max_sessions_total = config.lifecycle.max_sessions_total,
        max_sessions_per_tenant = config.lifecycle.max_sessions_per_tenant,
        session_idle_ttl_secs = config.lifecycle.idle_session_ttl.as_secs(),
        unbound_bridge_session_ttl_secs = config.lifecycle.unbound_bridge_session_ttl.as_secs(),
        session_absolute_ttl_secs = ?config.lifecycle.absolute_session_ttl.map(|ttl| ttl.as_secs()),
        max_pinned_sessions_per_tenant = ?config.lifecycle.max_pinned_sessions_per_tenant,
        connector_idle_shutdown_ttl_secs = ?config.lifecycle.connector_idle_shutdown_ttl.map(|ttl| ttl.as_secs()),
        active_turn_deadline_secs = ?config.lifecycle.active_turn_deadline.map(|ttl| ttl.as_secs()),
        max_subscribers_per_session = config.max_subscribers_per_session,
        stream_replay_buffer_size = config.stream_replay_buffer_size,
        stream_idle_retention_secs = config.stream_idle_retention.as_secs(),
        tenant_process_isolation = config.tenant_process_isolation,
        session_process_isolation = config.session_process_isolation,
        "starting acpx-server"
    );

    let mut router = Router::new(config.default_agent_id.clone())
        .with_native_auth_method_id(config.native_auth_method_id.clone())
        .with_lifecycle_config(config.lifecycle.clone())
        .with_tenant_process_isolation(config.tenant_process_isolation)
        .with_session_process_isolation(config.session_process_isolation)
        .with_process_reader_demux(config.process_reader_demux)
        .with_notification_hub(NotificationHub::with_stream_retention(
            256,
            config.max_subscribers_per_session,
            config.stream_replay_buffer_size,
            config.stream_idle_retention,
        ));
    router.register_agent(config.default_agent_id.clone(), config.backend.clone());

    if let Ok(db_path) = std::env::var("ACPX_DB_PATH") {
        match acpx_core::PersistenceStore::open(std::path::Path::new(&db_path)) {
            Ok(store) => {
                tracing::info!(%db_path, "session persistence enabled");
                router = router.with_persistence(store);

                // `durable_secret_and_configuration_store`. Piggybacks on
                // `ACPX_DB_PATH` being set rather than a separate opt-in
                // flag: an operator who already asked for durable session
                // persistence should not discover, only on the next
                // restart, that keys/profiles/mcp servers quietly stayed
                // in-memory-only the whole time. `ACPX_MASTER_KEYRING_PATH`
                // overrides where the encryption keyring lives; default is
                // `<db_path>.keyring`, created on first use (0600
                // permissions, see `keystore::MasterKeyring::save`).
                let keyring_path = std::env::var("ACPX_MASTER_KEYRING_PATH")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from(format!("{db_path}.keyring")));
                match router.enable_durable_config(keyring_path.clone()).await {
                    Ok(()) => {
                        tracing::info!(
                            keyring_path = %keyring_path.display(),
                            "durable secret/config store enabled"
                        );
                    }
                    Err(err) => {
                        panic!(
                            "ACPX_DB_PATH={db_path}: failed to enable durable secret/config \
                             store at {}: {err}",
                            keyring_path.display()
                        );
                    }
                }

                // One-shot operator-triggered key rotation -- re-encrypts
                // every persisted secret under a freshly-minted keyring
                // version. Not a schedule; unset (the default) never
                // rotates. See `Router::rotate_master_key`'s doc comment.
                if std::env::var("ACPX_MASTER_KEYRING_ROTATE").as_deref() == Ok("1") {
                    match router.rotate_master_key().await {
                        Ok(new_version) => {
                            tracing::info!(new_version, "master keyring rotated");
                        }
                        Err(err) => {
                            panic!("ACPX_MASTER_KEYRING_ROTATE=1: rotation failed: {err}");
                        }
                    }
                }
            }
            Err(err) => {
                tracing::error!(%err, %db_path, "failed to open ACPX_DB_PATH, continuing without persistence");
            }
        }
    }

    // Once, here, before any listener starts accepting connections -- see
    // `Router::warm_default_profiles`'s doc comment for why this must
    // never happen lazily inside a request's own critical section. Run
    // *after* the `ACPX_DB_PATH`/durable-config block above (not before
    // it, as this used to run): `ensure_default_profiles_seeded` only
    // fills in a profile name that is not already present
    // (`self.profiles.get(&agent.id).is_some()` short-circuits it), so
    // persisted profiles must be loaded first or a restart would
    // silently reseed and shadow an operator's own customization of one
    // of the auto-seeded default names (e.g. `codex-acp`) every time.
    router.warm_default_profiles().await;

    // The admin plane changes gateway-wide launch policy, so it is never
    // allowed to run with ephemeral state. Load one registry snapshot now
    // too: AdminOps owns the registry/custom-id namespace boundary.
    let admin_transport = if let Some(token) = config.admin_token.clone() {
        let store = router.persistence_store().ok_or_else(|| {
            anyhow::anyhow!("ACPX_ADMIN_TOKEN requires ACPX_DB_PATH for durable admin state")
        })?;
        let registry_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let registry = acpx_registry::fetch_registry_or_fallback(&registry_client).await;
        Some((
            config
                .admin_bind_addr
                .expect("admin token always sets an admin bind address"),
            token,
            store,
            registry,
        ))
    } else {
        None
    };

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

    let router: transport::SharedRouter = Arc::new(Mutex::new(router));
    if config.startup_session_recovery_enabled {
        #[cfg(feature = "startup-session-recovery")]
        {
            let report = recover_open_sessions_shared(
                &router,
                StartupRecoveryPolicy {
                    timeout: config.startup_session_recovery_timeout,
                    concurrency: config.startup_session_recovery_concurrency,
                    fail_fast: config.startup_session_recovery_fail_fast,
                },
            )
            .await?;
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

    if config.lifecycle_reaper_enabled {
        let lifecycle_router = Arc::clone(&router);
        let interval = config.lifecycle_reaper_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // Establish interval without delaying the first full tick.
            loop {
                ticker.tick().await;
                let mut guard = lifecycle_router.lock().await;
                let report = guard.reap_expired_sessions(std::time::Instant::now()).await;
                // **`connector_reference_lifecycle`.** Piggybacks on the
                // same reaper tick rather than a second timer -- a no-op
                // whenever `connector_idle_shutdown_ttl` is unset, and
                // otherwise stops shared backend processes that have had
                // zero referencing live sessions for at least that TTL.
                let stopped_backends = guard
                    .reap_unreferenced_backends(std::time::Instant::now())
                    .await;
                // **`active_turn_deadline`.** Same tick, same rationale as
                // `reap_unreferenced_backends` above -- a no-op whenever
                // `active_turn_deadline` is unset.
                let cancelled_turns = guard.cancel_stuck_turns(std::time::Instant::now()).await;
                drop(guard);
                if report.closed != 0 || report.failed != 0 {
                    tracing::info!(
                        closed = report.closed,
                        failed = report.failed,
                        skipped = report.skipped,
                        "completed ACPX lifecycle reaper pass"
                    );
                }
                if stopped_backends != 0 {
                    tracing::info!(
                        stopped_backends,
                        "stopped idle, unreferenced backend processes"
                    );
                }
                if cancelled_turns != 0 {
                    tracing::warn!(
                        cancelled_turns,
                        "cancelled turns that exceeded the active-turn deadline"
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
    let auth_tenant_tokens = config.auth_tenant_tokens.clone();
    let auth_tenant_allowlist = config.auth_tenant_allowlist.clone();

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
    let http_router = Arc::clone(&router);
    let http_task = http_listener.map(move |listener| {
        let bridge = bridge_config.clone();
        let router = Arc::clone(&http_router);
        tokio::spawn(async move {
            transport::serve_on_with_bridge_and_tenant_tokens(
                listener,
                router,
                auth_token,
                auth_tenant_tokens,
                auth_tenant_allowlist,
                bridge,
            )
            .await
        })
    });
    let admin_task = match admin_transport {
        Some((bind_addr, token, store, registry)) => {
            let listener = tokio::net::TcpListener::bind(bind_addr).await?;
            let router = Arc::clone(&router);
            Some(tokio::spawn(async move {
                transport::admin::serve_on_with_router(
                    listener,
                    token,
                    store,
                    registry,
                    Some(router),
                )
                .await
            }))
        }
        None => None,
    };

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
    match (http_task, admin_task) {
        (Some(mut http_task), Some(mut admin_task)) => {
            tokio::select! {
                result = stdio_task => {
                    result??;
                    (&mut http_task).await??;
                    (&mut admin_task).await??;
                }
                result = &mut http_task => {
                    result??;
                }
                result = &mut admin_task => {
                    result??;
                }
            }
        }
        (Some(mut http_task), None) => {
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
        (None, Some(mut admin_task)) => {
            tokio::select! {
                result = stdio_task => {
                    result??;
                    (&mut admin_task).await??;
                }
                result = &mut admin_task => {
                    result??;
                }
            }
        }
        // HTTP/WS disabled or unbindable: nothing to select against, just
        // run stdio to completion (its own EOF-vs-error distinction is
        // handled inside `transport::stdio::run` already).
        (None, None) => {
            stdio_task.await??;
        }
    }
    Ok(())
}
