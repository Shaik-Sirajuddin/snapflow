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

use crate::transport::http::SharedRouter;
use crate::transport::live::{session_id_to_forget, session_id_to_watch};
use acpx_core::router::dispatch_shared;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Run the stdio request/response loop against `router` until stdin closes.
pub async fn run(router: SharedRouter) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut lines = BufReader::new(stdin).lines();
    let hub = { router.lock().await.notification_hub() };
    let mut watched: HashSet<String> = HashSet::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(%err, "dropping malformed JSON-RPC line on stdin");
                continue;
            }
        };
        let response = {
            match dispatch_shared(&router, request.clone()).await {
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
            if watched.remove(&forget) {
                hub.unsubscribe(&forget).await;
            }
        } else if let Some(watch) = session_id_to_watch(&request, &response, method) {
            if watched.insert(watch.clone()) {
                let mut rx = hub.subscribe(watch).await;
                let forwarder_stdout = Arc::clone(&stdout);
                tokio::spawn(async move {
                    while let Some(update) = rx.recv().await {
                        let Ok(mut bytes) = serde_json::to_vec(&update) else {
                            continue;
                        };
                        bytes.push(b'\n');
                        let mut out = forwarder_stdout.lock().await;
                        if out.write_all(&bytes).await.is_err() || out.flush().await.is_err() {
                            break;
                        }
                    }
                });
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
    for session_id in watched {
        hub.unsubscribe(&session_id).await;
    }
    tracing::info!("client stdin closed, stdio transport exiting");
    Ok(())
}
