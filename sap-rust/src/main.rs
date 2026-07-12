//! sap-rust binary entrypoint. Reads connection config from the environment
//! (the real launch path: the daemon spawns Snapshot with `SNAPSHOT_SAP_SOCKET`/
//! `SNAPSHOT_SAP_TOKEN` set, per `08-lifecycle-and-cli.md`) with a `--socket`
//! CLI fallback so the server is runnable standalone for manual testing.

use std::path::PathBuf;

use sap_rust::backend::{Backend, MockBackend};
use sap_rust::mlt_backend::MltBackend;
use sap_rust::server::{self, ServerConfig};

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

    // Real launch path: snapshotd's procmgr sets SNAPSHOT_PROJECT_ROOT to the
    // bound project's sandbox root (per 09-project-folder-layout.md) before
    // spawning this binary -- when present, use the real MltBackend so
    // file.export actually shells out to melt and produces a playable video,
    // per 02-rust-embedding.md/doc 11's Phase A pass criteria. Falls back to
    // MockBackend (no real media) only for standalone/manual runs where no
    // project root was supplied, e.g. `cargo run -- --socket /tmp/x.sock`.
    //
    match std::env::var("SNAPSHOT_PROJECT_ROOT") {
        Ok(root) if !root.is_empty() => {
            eprintln!("sap-rust: using real MltBackend, fixed project root {root}");
            let backend = MltBackend::new_fixed_root(PathBuf::from(root));
            run(config, backend).await;
        }
        _ => {
            eprintln!("sap-rust: SNAPSHOT_PROJECT_ROOT not set, using MockBackend (no real media)");
            let backend = MockBackend::new();
            run(config, backend).await;
        }
    }
}

async fn run<B: Backend + 'static>(config: ServerConfig, backend: B) {
    if let Err(e) = server::serve(config, backend, None).await {
        eprintln!("sap-rust: server error: {e}");
        std::process::exit(1);
    }
}
