//! Local stdio transport: reads newline-delimited JSON-RPC requests from
//! this process's stdin, dispatches each through the shared `Router`
//! (multi-agent aware as of Phase 2 -- gateway-native/proxied/hybrid
//! classification, session registry, agent spawn/reuse via the
//! conductor), and writes the JSON-RPC response back to stdout.
//!
//! Takes the same `SharedRouter` handle (`Arc<Mutex<Router>>`) that
//! `transport::http::serve` uses, so a single `acpx-server` process can run
//! both transports concurrently against one `Router` -- one session
//! registry and one set of supervised backend processes, regardless of
//! whether a client connects over stdio, HTTP, or WS. Requests on this
//! transport are still processed sequentially (dispatch one, write its
//! response, read the next) since stdio is a single duplex stream for one
//! local client; the HTTP/WS transports serve many connections
//! concurrently against the same shared lock.
//!
//! **Live `session/update` streaming (ACP compatibility phase 14).** Like
//! `ws.rs`, this transport subscribes to `acpx_core::notify::
//! NotificationHub` for every gateway session it touches -- see
//! `transport::live`'s module doc comment for the shared subscribe/
//! unsubscribe decision logic. Stdout is wrapped in an `Arc<Mutex<..>>`
//! so a live forwarder task can write standalone notification frames to
//! it concurrently with this loop's own request/response writes, without
//! either side's bytes interleaving mid-frame.
//!
//! **Tenant isolation (`acpx-tenant-isolation` Phase B).** No per-message
//! header concept exists on this transport at all (it's a raw
//! newline-delimited stream, not HTTP/WS). Resolved once, at process
//! startup, from the optional `ACPX_STDIO_TENANT` env var (or
//! [`acpx_core::TenantId::default_tenant`] if unset) and fixed for this
//! process's entire stdio lifetime -- multi-tenant stdio use means
//! launching separate `acpx-server` processes per tenant, not a
//! mid-stream switch.

use crate::transport::http::SharedRouter;
use crate::transport::live::{session_id_to_forget, session_id_to_watch, take_resume_cursor};
use acpx_core::router::{dispatch_shared_for_tenant, stream_resume_state_shared};
use acpx_core::{InteractionBinding, StreamResumeState, TenantId, INTERACTION_QUEUE_CAPACITY};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

/// Run the stdio request/response loop against `router` until stdin closes.
pub async fn run(router: SharedRouter) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut lines = BufReader::new(stdin).lines();
    let hub = { router.lock().await.notification_hub() };
    let interaction_hub = { router.lock().await.interaction_hub() };
    let (interaction_tx, mut interaction_rx) = mpsc::channel(INTERACTION_QUEUE_CAPACITY);
    let interaction_stdout = Arc::clone(&stdout);
    tokio::spawn(async move {
        while let Some(request) = interaction_rx.recv().await {
            let Ok(mut bytes) = serde_json::to_vec(&request) else {
                continue;
            };
            bytes.push(b'\n');
            let mut out = interaction_stdout.lock().await;
            if out.write_all(&bytes).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let watched = Arc::new(Mutex::new(HashSet::<String>::new()));
    let interaction_bindings = Arc::new(Mutex::new(HashMap::<String, InteractionBinding>::new()));
    let tenant_id = std::env::var("ACPX_STDIO_TENANT")
        .ok()
        .filter(|t| !t.is_empty())
        .map(TenantId::from)
        .unwrap_or_default();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let mut request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(%err, "dropping malformed JSON-RPC line on stdin");
                continue;
            }
        };
        let resume_cursor = take_resume_cursor(&mut request);

        // Response-only JSON-RPC frames are replies to agent-initiated
        // requests. They are correlated directly and must not be routed as
        // ordinary client-to-agent calls.
        if request.get("method").is_none() && request.get("id").is_some() {
            interaction_hub.resolve(request).await;
            continue;
        }

        // A prompt can block while its backend asks the client for input.
        // Bind and dispatch it independently so this read loop remains able
        // to receive that correlated response over the same stdio stream.
        if let Some(session_id) = request
            .pointer("/params/sessionId")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        {
            // Resume subscriptions attach before dispatch for the same
            // reason as WS: a backend can emit updates while a slow
            // `session/load`/`session/resume` request is still executing.
            let resumed_before_dispatch = if resume_cursor.is_some()
                && watched.lock().await.insert(session_id.clone())
            {
                let state = stream_resume_state_shared(&router, &tenant_id, &session_id).await;
                match hub
                    .subscribe_resuming(
                        &tenant_id,
                        session_id.clone(),
                        resume_cursor.clone(),
                        StreamResumeState {
                            backend_session_id: state.backend_session_id,
                            durable_state_changed: state.durable_state_changed,
                        },
                    )
                    .await
                {
                    Ok(mut rx) => {
                        let forwarder_stdout = Arc::clone(&stdout);
                        tokio::spawn(async move {
                            loop {
                                let update = match rx.recv().await {
                                    Ok(update) => update.into_value(),
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                        skipped,
                                    )) => {
                                        tracing::warn!(%skipped, "ACPX notification subscriber lagged");
                                        continue;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                };
                                let Ok(mut bytes) = serde_json::to_vec(&update) else {
                                    continue;
                                };
                                bytes.push(b'\n');
                                let mut out = forwarder_stdout.lock().await;
                                if out.write_all(&bytes).await.is_err()
                                    || out.flush().await.is_err()
                                {
                                    break;
                                }
                            }
                        });
                        true
                    }
                    Err(error) => {
                        watched.lock().await.remove(&session_id);
                        let mut bytes = serde_json::to_vec(
                            &crate::transport::http::json_rpc_subscribe_error(&request, error),
                        )?;
                        bytes.push(b'\n');
                        let mut out = stdout.lock().await;
                        out.write_all(&bytes).await?;
                        out.flush().await?;
                        continue;
                    }
                }
            } else {
                false
            };
            let binding = interaction_hub
                .bind(
                    tenant_id.clone(),
                    session_id.clone(),
                    interaction_tx.clone(),
                )
                .await;
            let previous = interaction_bindings
                .lock()
                .await
                .insert(session_id.clone(), binding);
            if let Some(previous) = previous {
                interaction_hub.unbind(&previous).await;
            }
            let subscribe_after_response =
                !resumed_before_dispatch && watched.lock().await.insert(session_id.clone());

            let router = Arc::clone(&router);
            let tenant_id = tenant_id.clone();
            let stdout = Arc::clone(&stdout);
            let hub = hub.clone();
            let watched = Arc::clone(&watched);
            tokio::spawn(async move {
                let mut response =
                    match dispatch_shared_for_tenant(&router, &tenant_id, request.clone()).await {
                        Ok(response) => response,
                        Err(error) => crate::transport::http::json_rpc_error(&request, error),
                    };
                if subscribe_after_response && response.get("error").is_none() {
                    let state = stream_resume_state_shared(&router, &tenant_id, &session_id).await;
                    match hub
                        .subscribe_resuming(
                            &tenant_id,
                            session_id.clone(),
                            resume_cursor.clone(),
                            StreamResumeState {
                                backend_session_id: state.backend_session_id,
                                durable_state_changed: state.durable_state_changed,
                            },
                        )
                        .await
                    {
                        Ok(mut rx) => {
                            let forwarder_stdout = Arc::clone(&stdout);
                            tokio::spawn(async move {
                                loop {
                                    let update = match rx.recv().await {
                                        Ok(update) => update.into_value(),
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                            skipped,
                                        )) => {
                                            tracing::warn!(%skipped, "ACPX notification subscriber lagged");
                                            continue;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                            break
                                        }
                                    };
                                    let Ok(mut bytes) = serde_json::to_vec(&update) else {
                                        continue;
                                    };
                                    bytes.push(b'\n');
                                    let mut out = forwarder_stdout.lock().await;
                                    if out.write_all(&bytes).await.is_err()
                                        || out.flush().await.is_err()
                                    {
                                        break;
                                    }
                                }
                            });
                        }
                        Err(error) => {
                            watched.lock().await.remove(&session_id);
                            response =
                                crate::transport::http::json_rpc_subscribe_error(&request, error);
                        }
                    }
                } else if subscribe_after_response {
                    watched.lock().await.remove(&session_id);
                }
                let Ok(mut bytes) = serde_json::to_vec(&response) else {
                    return;
                };
                bytes.push(b'\n');
                let mut out = stdout.lock().await;
                let _ = out.write_all(&bytes).await;
                let _ = out.flush().await;
            });
            continue;
        }

        let mut response = {
            match dispatch_shared_for_tenant(&router, &tenant_id, request.clone()).await {
                Ok(response) => response,
                Err(err) => {
                    tracing::warn!(%err, "dispatch error, returning JSON-RPC error response");
                    crate::transport::http::json_rpc_error(&request, err)
                }
            }
        };

        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        if let Some(forget) = session_id_to_forget(&request, &response, method) {
            if watched.lock().await.remove(&forget) {
                hub.remove_stream(&tenant_id, &forget).await;
            }
        } else if let Some(watch) = session_id_to_watch(&request, &response, method) {
            if watched.lock().await.insert(watch.clone()) {
                let state = stream_resume_state_shared(&router, &tenant_id, &watch).await;
                match hub
                    .subscribe_resuming(
                        &tenant_id,
                        watch.clone(),
                        resume_cursor.clone(),
                        StreamResumeState {
                            backend_session_id: state.backend_session_id,
                            durable_state_changed: state.durable_state_changed,
                        },
                    )
                    .await
                {
                    Ok(mut rx) => {
                        let forwarder_stdout = Arc::clone(&stdout);
                        tokio::spawn(async move {
                            loop {
                                let update = match rx.recv().await {
                                    Ok(update) => update.into_value(),
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                        skipped,
                                    )) => {
                                        tracing::warn!(%skipped, "ACPX notification subscriber lagged");
                                        continue;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                };
                                let Ok(mut bytes) = serde_json::to_vec(&update) else {
                                    continue;
                                };
                                bytes.push(b'\n');
                                let mut out = forwarder_stdout.lock().await;
                                if out.write_all(&bytes).await.is_err()
                                    || out.flush().await.is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                    Err(error) => {
                        watched.lock().await.remove(&watch);
                        response =
                            crate::transport::http::json_rpc_subscribe_error(&request, error);
                    }
                }
            }
        }

        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        {
            let mut out = stdout.lock().await;
            out.write_all(&bytes).await?;
            out.flush().await?;
        }
    }
    for (_, binding) in interaction_bindings.lock().await.drain() {
        interaction_hub.unbind(&binding).await;
    }
    tracing::info!("client stdin closed, stdio transport exiting");
    Ok(())
}
