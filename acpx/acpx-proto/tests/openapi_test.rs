//! Fails the moment `docs/schema/acpx-http.openapi.json` drifts from
//! what `acpx_proto::openapi::build_openapi_document` currently
//! generates -- same drift-guard pattern as `schema_test.rs`/
//! `openrpc_test.rs`.

use std::path::Path;

#[test]
fn committed_openapi_file_matches_current_source() {
    let committed_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/schema/acpx-http.openapi.json");
    let committed_raw = std::fs::read_to_string(&committed_path).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {e} -- run scripts/gen_openapi.sh first",
            committed_path.display()
        )
    });
    let committed: serde_json::Value =
        serde_json::from_str(&committed_raw).expect("committed OpenAPI file is not valid JSON");

    let current = acpx_proto::openapi::build_openapi_document();

    assert_eq!(
        committed, current,
        "docs/schema/acpx-http.openapi.json is stale -- the HTTP \
         transport envelope or a referenced type changed without \
         regenerating it. Run: bash scripts/gen_openapi.sh"
    );
}
