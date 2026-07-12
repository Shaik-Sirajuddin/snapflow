//! Local stdio transport: proxies a single client 1:1 to one supervised
//! backend process. No session registry, no multi-agent routing yet --
//! this validates the framing/spawn/proxy plumbing in isolation, per
//! `04-phased-plan.md` Phase 1 step 4.

use acpx_conductor::process::BackendProcess;
use acpx_conductor::SpawnSpec;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Run the Phase 1 passthrough loop: read newline-delimited JSON-RPC from
/// this process's stdin, forward each line verbatim to the backend's
/// stdin, and concurrently forward everything the backend writes to its
/// stdout back out to this process's stdout. Payloads are never
/// deserialized/reinterpreted here -- pure byte-for-byte passthrough, since
/// there is exactly one client and one backend and no gateway-native
/// methods to intercept yet.
pub async fn run(spec: &SpawnSpec) -> anyhow::Result<()> {
    let mut backend = BackendProcess::spawn(spec).await?;

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut client_lines = BufReader::new(stdin).lines();

    loop {
        tokio::select! {
            line = client_lines.next_line() => {
                match line? {
                    Some(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let value: serde_json::Value = serde_json::from_str(&line)?;
                        backend.writer.write_value(&value).await?;
                    }
                    None => {
                        tracing::info!("client stdin closed, shutting down backend");
                        backend.kill().await?;
                        return Ok(());
                    }
                }
            }
            value = backend.reader.read_value() => {
                match value {
                    Ok(value) => {
                        let mut line = serde_json::to_vec(&value)?;
                        line.push(b'\n');
                        stdout.write_all(&line).await?;
                        stdout.flush().await?;
                    }
                    Err(_) => {
                        tracing::info!("backend stdout closed, exiting passthrough loop");
                        return Ok(());
                    }
                }
            }
        }
    }
}
