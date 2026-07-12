//! Phase 1 throwaway single-agent passthrough spike test, per
//! `03-crate-and-folder-layout.md`'s note that this shouldn't become
//! permanent scaffolding other crates depend on.
//!
//! This drives `acpx-conductor::BackendProcess` directly against `cat` (a
//! trivial stand-in "backend") to prove the newline-delimited JSON-RPC
//! framing round-trips correctly, without depending on a real ACP adapter
//! being installed in CI.

use acpx_conductor::process::BackendProcess;
use acpx_conductor::SpawnSpec;
use serde_json::json;

#[tokio::test]
async fn framed_roundtrip_through_a_stand_in_backend() {
    let spec = SpawnSpec::new("cat", vec![]);
    let mut backend = BackendProcess::spawn(&spec).await.expect("spawn cat");

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    backend
        .writer
        .write_value(&request)
        .await
        .expect("write request");

    let echoed = backend
        .reader
        .read_value()
        .await
        .expect("read echoed value");
    assert_eq!(echoed, request);

    backend.kill().await.expect("kill backend");
}
