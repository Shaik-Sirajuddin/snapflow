//! `acpx-server` daemon entrypoint.
//!
//! Phase 1: stdio-only single-agent passthrough (see
//! `transport::stdio::run`). Multi-agent routing, the gateway API, and
//! HTTP/WS transports land in Phase 2.

mod config;
mod transport;

use config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr) // stdout is the ACP wire in Phase 1
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = ServerConfig::from_env();
    tracing::info!(program = %config.backend.program, args = ?config.backend.args, "starting acpx-server (Phase 1 stdio passthrough)");

    transport::stdio::run(&config.backend).await
}
