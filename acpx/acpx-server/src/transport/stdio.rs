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

use crate::transport::http::SharedRouter;
use acpx_core::router::dispatch_shared;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Run the stdio request/response loop against `router` until stdin closes.
pub async fn run(router: SharedRouter) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

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
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        stdout.write_all(&bytes).await?;
        stdout.flush().await?;
    }
    tracing::info!("client stdin closed, stdio transport exiting");
    Ok(())
}
