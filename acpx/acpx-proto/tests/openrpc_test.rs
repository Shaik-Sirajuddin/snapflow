//! Fails the moment `docs/schema/acpx.openrpc.json` drifts from what
//! `acpx_proto::openrpc::build_openrpc_document` currently generates --
//! same drift-guard pattern as `schema_test.rs`, see that file's doc
//! comment for the rationale (no process-spawn/PATH dependency, runs as
//! fast as any other unit test).

use std::path::Path;

#[test]
fn committed_openrpc_file_matches_current_method_registry() {
    let committed_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/schema/acpx.openrpc.json");
    let committed_raw = std::fs::read_to_string(&committed_path).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {e} -- run scripts/gen_openrpc.sh first",
            committed_path.display()
        )
    });
    let committed: serde_json::Value =
        serde_json::from_str(&committed_raw).expect("committed OpenRPC file is not valid JSON");

    let current = acpx_proto::openrpc::build_openrpc_document();

    assert_eq!(
        committed, current,
        "docs/schema/acpx.openrpc.json is stale -- a dispatched method or \
         referenced type changed without regenerating it. Run: bash \
         scripts/gen_openrpc.sh"
    );
}
