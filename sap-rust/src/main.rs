//! sap-rust binary entrypoint. Reads connection config from the environment
//! (the real launch path: the daemon spawns Snapshot with `SNAPSHOT_SAP_SOCKET`/
//! `SNAPSHOT_SAP_TOKEN` set, per `08-lifecycle-and-cli.md`) with a `--socket`
//! CLI fallback so the server is runnable standalone for manual testing.

use sap_rust::backend::{Backend, MockBackend};
use sap_rust::server::{self, ServerConfig};
use std::path::PathBuf;

fn socket_path_from_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let socket_path = match std::env::var("SNAPSHOT_SAP_SOCKET") {
        Ok(path) => PathBuf::from(path),
        Err(_) => match socket_path_from_args() {
            Some(path) => path,
            None => {
                eprintln!(
                    "sap-rust: no socket path given (set SNAPSHOT_SAP_SOCKET, or pass --socket <path>)"
                );
                std::process::exit(2);
            }
        },
    };

    let token = std::env::var("SNAPSHOT_SAP_TOKEN").unwrap_or_default();
    if token.is_empty() {
        eprintln!(
            "sap-rust: warning: SNAPSHOT_SAP_TOKEN not set (or empty) — sap.hello will require an empty token string"
        );
    }

    let audio_enabled = matches!(
        std::env::var("SNAPSHOT_AUDIO_ENABLED").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True")
    );
    let config = ServerConfig { socket_path: socket_path.clone(), token, audio_enabled };
    println!("sap-rust: listening on {}", socket_path.display());

    // This standalone binary always runs MockBackend now (no real media,
    // no Qt/live-Shotcut-process dependency). Real editing/media requires
    // the real_ffi-featured Qt build (`shotcut` binary, built via corrosion
    // per shotcut/CMakeLists.txt), which links `FfiBackend` directly and
    // calls `sap_start_server` from `main.cpp` instead of running this
    // binary at all -- see 02-rust-embedding.md. This binary remains only
    // for manual/dev testing of the JSON-RPC wire protocol without a full
    // Qt build (e.g. `cargo run -- --socket /tmp/x.sock`).
    eprintln!("sap-rust: standalone binary, using MockBackend (no real media -- see main.rs doc comment)");
    let backend = MockBackend::new();
    run(config, backend).await;
}

async fn run<B: Backend + 'static>(config: ServerConfig, backend: B) {
    if let Err(e) = server::serve(config, backend, None).await {
        eprintln!("sap-rust: server error: {e}");
        std::process::exit(1);
    }
}
