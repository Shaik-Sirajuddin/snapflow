//! Stdio-to-WebSocket ACP adapter for one shared ACPX daemon.
//!
//! This process owns only its local stdio connection. EOF or a broken
//! WebSocket never sends `session/close` to ACPX, so retained daemon
//! sessions survive an editor/OpenHands subprocess restart.

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = bridge_url()?;
    let request = bridge_request(&url)?;
    let (socket, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = socket.split();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                match line? {
                    Some(line) if !line.trim().is_empty() => {
                        // Validate locally so an accidental non-JSON log line
                        // cannot corrupt the ACP wire connection.
                        let _: serde_json::Value = serde_json::from_str(&line)?;
                        write.send(Message::Text(line)).await?;
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
                    return Ok(());
                };
                match frame? {
                    Message::Text(text) => {
                        let _: serde_json::Value = serde_json::from_str(&text)?;
                        stdout.write_all(text.as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                    Message::Binary(bytes) => {
                        let text = String::from_utf8(bytes)?;
                        let _: serde_json::Value = serde_json::from_str(&text)?;
                        stdout.write_all(text.as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(payload) => write.send(Message::Pong(payload)).await?,
                    Message::Pong(_) | Message::Frame(_) => {}
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
