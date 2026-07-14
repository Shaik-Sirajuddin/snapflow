//! Fails the moment `docs/schema/acpx-wire.schema.json` drifts from what
//! `acpx_proto::schema::build_schema_document` currently generates -- i.e.
//! someone changed a `#[derive(JsonSchema)]`-annotated wire type (added a
//! field, renamed a variant, ...) and forgot to re-run
//! `scripts/gen_schema.sh`. Reads the committed file directly rather than
//! shelling out to `cargo run --bin gen-schema` so this test has no
//! process-spawn/PATH dependency and runs as fast as any other unit test.

use std::path::Path;

#[test]
fn committed_schema_file_matches_current_wire_types() {
    let committed_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/schema/acpx-wire.schema.json");
    let committed_raw = std::fs::read_to_string(&committed_path).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {e} -- run scripts/gen_schema.sh first",
            committed_path.display()
        )
    });
    let committed: serde_json::Value =
        serde_json::from_str(&committed_raw).expect("committed schema file is not valid JSON");

    let current = acpx_proto::schema::build_wire_schema_document();

    assert_eq!(
        committed, current,
        "docs/schema/acpx-wire.schema.json is stale -- a wire type changed \
         without regenerating it. Run: bash scripts/gen_schema.sh"
    );
}
