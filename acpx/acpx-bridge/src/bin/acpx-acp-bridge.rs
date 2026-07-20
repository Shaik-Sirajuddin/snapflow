//! Stdio-to-WebSocket ACP adapter for one shared ACPX daemon.
//!
//! This process owns only its local stdio connection. EOF or a broken
//! WebSocket never sends `session/close` to ACPX, so retained daemon
//! sessions survive an editor/OpenHands subprocess restart.
//!
//! **Mid-stream WebSocket reconnect.** A dropped WebSocket (a network
//! blip, `acpx-server` restarting, ...) used to be fatal: the whole
//! process exited, forcing the editor to notice a crashed agent and
//! spawn a brand-new bridge process with no memory of anything. That new
//! process, plus the fact that nothing here ever populated ACPX's
//! `_acpx.resume` reconnect cursor, meant any `session/update`
//! notification published while the socket was down (routinely the
//! start of whatever the backend agent was mid-way through streaming)
//! was gone for good, even though `acpx-server`'s `NotificationHub` had
//! it buffered the whole time -- see `acpx_bridge::resume`'s module doc
//! comment for the full mechanics. This `main` instead reconnects in
//! place (bounded exponential backoff) and, via [`ResumeTracker`],
//! injects that cursor into the next outgoing frame for every session it
//! was mid-way through when the drop happened, so `acpx-server` replays
//! exactly what was missed. Only stdin EOF -- the editor actually
//! killing this child -- still ends the process.

use acpx_bridge::ResumeTracker;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);
/// Hard ceiling on one `connect_async` attempt (DNS + TCP + WS
/// handshake). Without this, a silently-dropping firewall or a DNS
/// resolver that never answers leaves this entire process wedged inside
/// the very first `.await` of the reconnect loop -- unable to retry,
/// back off, *or* keep forwarding stdin/stdout in the meantime, since
/// this is a single sequential loop, not a background task. A stuck
/// bridge here means the editor/OpenHands session it's attached to looks
/// dead with no retry ever happening, exactly the class of hang this
/// binary's whole reconnect-in-place design exists to avoid.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = bridge_url()?;
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut tracker = ResumeTracker::new();
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    // Only true from the *second* successful connect onward -- the first
    // one has no prior session to resume anything against.
    let mut is_reconnect = false;

    'connection: loop {
        let request = bridge_request(&url)?;
        eprintln!("acpx-acp-bridge: connecting to {url} (is_reconnect={is_reconnect})");
        let socket = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await
        {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(err)) => {
                eprintln!(
                    "acpx-acp-bridge: connect to {url} failed ({err}); retrying in {backoff:?}"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                continue 'connection;
            }
            Err(_elapsed) => {
                eprintln!(
                    "acpx-acp-bridge: connect to {url} timed out after {CONNECT_TIMEOUT:?}; \
                     retrying in {backoff:?}"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                continue 'connection;
            }
        };
        backoff = INITIAL_RECONNECT_BACKOFF;
        if is_reconnect {
            tracker.mark_all_for_resync();
            eprintln!(
                "acpx-acp-bridge: reconnected to {url}; resuming {} tracked session(s)",
                tracker.len()
            );
        }
        is_reconnect = true;
        let (mut write, mut read) = socket.split();

        loop {
            tokio::select! {
                line = stdin.next_line() => {
                    match line? {
                        Some(line) if !line.trim().is_empty() => {
                            let mut value: serde_json::Value = match serde_json::from_str(&line) {
                                Ok(value) => value,
                                Err(err) => {
                                    // An accidental non-JSON log line must not
                                    // corrupt the ACP wire connection.
                                    eprintln!("acpx-acp-bridge: ignoring non-JSON stdin line: {err}");
                                    continue;
                                }
                            };
                            tracker.observe_outgoing(&value);
                            // Only re-serialize (which, without serde_json's
                            // `preserve_order` feature, reorders keys) when a
                            // cursor was actually injected -- every ordinary
                            // frame is forwarded byte-for-byte untouched.
                            let text = if tracker.prepare_outgoing(&mut value) {
                                value.to_string()
                            } else {
                                line
                            };
                            if let Err(err) = write.send(Message::Text(text)).await {
                                eprintln!("acpx-acp-bridge: send failed ({err}); reconnecting");
                                continue 'connection;
                            }
                        }
                        Some(_) => {}
                        None => {
                            let _ = write.send(Message::Close(None)).await;
                            return Ok(());
                        }
                    }
                }
                frame = read.next() => {
                    let Some(frame) = frame else {
                        eprintln!("acpx-acp-bridge: server closed the connection; reconnecting");
                        continue 'connection;
                    };
                    let text = match frame {
                        Ok(Message::Text(text)) => text,
                        Ok(Message::Binary(bytes)) => match String::from_utf8(bytes) {
                            Ok(text) => text,
                            Err(err) => {
                                eprintln!("acpx-acp-bridge: ignoring non-UTF8 binary frame: {err}");
                                continue;
                            }
                        },
                        Ok(Message::Close(_)) => {
                            eprintln!("acpx-acp-bridge: server sent Close; reconnecting");
                            continue 'connection;
                        }
                        Ok(Message::Ping(payload)) => {
                            let _ = write.send(Message::Pong(payload)).await;
                            continue;
                        }
                        Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => continue,
                        Err(err) => {
                            eprintln!("acpx-acp-bridge: read error ({err}); reconnecting");
                            continue 'connection;
                        }
                    };
                    let value: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(err) => {
                            eprintln!("acpx-acp-bridge: ignoring non-JSON server frame: {err}");
                            continue;
                        }
                    };
                    tracker.observe_incoming(&value);
                    stdout.write_all(text.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }
    }
}

fn bridge_url() -> anyhow::Result<String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--url") => Ok(args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--url requires a WebSocket URL"))?),
        Some(argument) => Err(anyhow::anyhow!(
            "unknown argument {argument:?}; expected --url"
        )),
        None => std::env::var("ACPX_ACP_BRIDGE_URL")
            .map_err(|_| anyhow::anyhow!("set ACPX_ACP_BRIDGE_URL or pass --url ws://host/acp/ws")),
    }
}

/// Build the daemon-only WebSocket handshake. Credentials never enter ACP
/// frames or stdout; they are limited to the local bridge -> ACPX transport.
fn bridge_request(url: &str) -> anyhow::Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut request = url.into_client_request()?;
    if let Ok(token) = std::env::var("ACPX_ACP_BRIDGE_TOKEN") {
        if !token.is_empty() {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            request
                .headers_mut()
                .insert(HeaderName::from_static("authorization"), value);
        }
    }
    if let Ok(tenant) = std::env::var("ACPX_ACP_BRIDGE_TENANT") {
        if !tenant.is_empty() {
            request.headers_mut().insert(
                HeaderName::from_static("x-acpx-tenant"),
                HeaderValue::from_str(&tenant)?,
            );
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_request_has_no_sensitive_headers_by_default() {
        std::env::remove_var("ACPX_ACP_BRIDGE_TOKEN");
        std::env::remove_var("ACPX_ACP_BRIDGE_TENANT");
        let request = bridge_request("ws://127.0.0.1:8790/acp/ws").unwrap();
        assert!(request.headers().get("authorization").is_none());
        assert!(request.headers().get("x-acpx-tenant").is_none());
    }
}
