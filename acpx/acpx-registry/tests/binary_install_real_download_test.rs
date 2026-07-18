//! Real download+extract coverage for the `binary` distribution path
//! (`release_validation` hardening item, `acp-gateway-daemon` plan --
//! "binary installation... not verified in the default suite"). Every
//! pre-existing test for this path (`install.rs`'s own `#[cfg(test)]`
//! module) only exercised host-platform-key derivation, opaque `cmd`
//! path joining, and negative/unsupported-platform errors -- never a
//! real `reqwest` download followed by real `tar`/`zip` extraction
//! landing real, readable files on disk. This test closes that gap
//! *without* depending on any external network resource (no link-rot
//! risk, no rate limiting, fully deterministic): it spins up a plain
//! `TcpListener`-based HTTP/1.0 responder on this machine serving an
//! in-memory-built `.tar.gz`, then points `install_into` at
//! `http://127.0.0.1:<port>/...` like any real registry-hosted archive
//! URL would be. Runs in the *default* (non-`#[ignore]`d) suite, and is
//! exactly the kind of check `acp-gateway-daemon`'s CI cross-platform
//! matrix (`.github/workflows/ci.yml`) now also runs on macOS/Windows
//! runners, since the extraction code itself (`tar`/`zip` crates, no
//! shelled-out `tar`/`unzip` binary) is meant to be platform-independent
//! by construction -- this is the test that actually proves it.

use acpx_registry::index::{Agent, BinaryDist, Distribution};
use acpx_registry::install::{host_platform_key, install_into, InstallOutcome};
use std::io::Write;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Builds a minimal, valid `.tar.gz` in memory containing one regular
/// file (`payload`, `contents`) at the archive root.
fn build_tar_gz(payload_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, payload_name, contents)
        .expect("append tar entry");
    let tar_bytes = builder.into_inner().expect("finish tar");

    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).expect("write tar into gzip");
    gz.finish().expect("finish gzip")
}

/// Serves exactly one HTTP/1.0 GET request with a fixed `body`, then
/// shuts down. No routing, no keep-alive -- the minimum needed to be a
/// real `reqwest::get`-able URL, avoiding a new `axum`/`hyper` dev
/// dependency for a single fixed response.
async fn serve_one_response(listener: TcpListener, body: Vec<u8>) {
    let (mut socket, _) = listener.accept().await.expect("accept one connection");
    let mut buf = [0u8; 1024];
    let _ = socket.read(&mut buf).await; // discard the request line/headers
    let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write status/headers");
    socket.write_all(&body).await.expect("write body");
    socket.shutdown().await.ok();
}

#[tokio::test]
async fn install_into_downloads_and_extracts_a_real_tar_gz_archive() {
    let archive_bytes = build_tar_gz("acpx-fake-adapter", b"#!/bin/sh\necho hi\n");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let serve_task = tokio::spawn(serve_one_response(listener, archive_bytes));

    let mut binaries = std::collections::HashMap::new();
    binaries.insert(
        host_platform_key(),
        BinaryDist {
            archive: format!("http://{addr}/acpx-fake-adapter.tar.gz"),
            cmd: "./acpx-fake-adapter".to_string(),
            args: vec![],
        },
    );
    let agent = Agent {
        id: "acpx-fake-binary-agent".to_string(),
        name: "ACPX Fake Binary Agent".to_string(),
        version: "0.0.0".to_string(),
        description: None,
        repository: None,
        website: None,
        authors: vec![],
        license: None,
        icon: None,
        distribution: Distribution {
            npx: None,
            uvx: None,
            binary: Some(binaries),
        },
    };

    let adapters_root = tempfile::tempdir().expect("tempdir");
    let outcome = install_into(&agent, adapters_root.path())
        .await
        .expect("real download+extract should succeed");
    serve_task.await.expect("server task");

    let InstallOutcome::Extracted { dir, cmd } = outcome else {
        panic!("expected Extracted, got {outcome:?}");
    };
    assert!(dir.starts_with(adapters_root.path()));
    assert!(cmd.exists(), "extracted binary must actually exist on disk");
    let extracted_contents = std::fs::read(&cmd).expect("read extracted file");
    assert_eq!(extracted_contents, b"#!/bin/sh\necho hi\n");
}
