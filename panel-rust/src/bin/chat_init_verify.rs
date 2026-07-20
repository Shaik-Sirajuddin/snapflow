//! Minimal init script: opens a session against a live acpx gateway and
//! sends two `session/prompt` messages, so a daemon operator can verify
//! (via the acpx-server's own logs, which `snapshotd` pipes to its own
//! stderr -- see `snapshotd/internal/acpxmgr`'s `cmd.Stdout/Stderr =
//! os.Stderr`) that a real chat conversation reaches the gateway
//! end-to-end. Not part of the panel-rust library API; a standalone
//! verification tool, same spirit as `rui-mock-agent`.
//!
//! Usage: `chat_init_verify [gateway_url]` (defaults to
//! `http://127.0.0.1:8790`, the daemon's bundled gateway's default bind).

use acpx_client::raw::GatewayClient;

#[tokio::main]
async fn main() {
    let base_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8790".to_string());
    let client = GatewayClient::new(base_url.clone());

    eprintln!("chat_init_verify: opening session against {base_url}");
    let new_result = client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": std::env::temp_dir().to_string_lossy(),
                "mcpServers": [],
            }),
            None,
        )
        .await
        .unwrap_or_else(|err| {
            eprintln!("chat_init_verify: session/new failed: {err}");
            std::process::exit(1);
        });
    let session_id = new_result
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or_else(|| {
            eprintln!("chat_init_verify: session/new returned no sessionId: {new_result}");
            std::process::exit(1);
        })
        .to_string();
    eprintln!("chat_init_verify: session opened, sessionId={session_id}");

    for (n, text) in ["hello from chat_init_verify, message one", "this is message two"]
        .into_iter()
        .enumerate()
    {
        let outcome = acpx_client::ext::prompt::send(
            &client,
            &session_id,
            serde_json::json!([{"type": "text", "text": text}]),
        )
        .await;
        match outcome {
            Ok(outcome) => eprintln!(
                "chat_init_verify: message {} sent, reply={:?}",
                n + 1,
                outcome.message_text
            ),
            Err(err) => eprintln!("chat_init_verify: message {} failed: {err}", n + 1),
        }
    }

    if let Err(err) = client
        .call("session/close", serde_json::json!({"sessionId": session_id}), None)
        .await
    {
        eprintln!("chat_init_verify: session/close failed (non-fatal): {err}");
    }
    eprintln!("chat_init_verify: done");
}
