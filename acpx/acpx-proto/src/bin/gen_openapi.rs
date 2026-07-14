//! Generates `docs/schema/acpx-http.openapi.json`. See `src/openapi.rs`'s
//! doc comment for what this document covers and why it exists
//! separately from the OpenRPC document.
//!
//! Run via `cargo run -p acpx-proto --bin gen-openapi`, which writes the
//! document to stdout -- `scripts/gen_openapi.sh` redirects that into
//! the committed file and `acpx-proto/tests/openapi_test.rs` fails CI
//! if the committed file has drifted from what this binary would
//! currently generate.

fn main() {
    let doc = acpx_proto::openapi::build_openapi_document();
    println!("{}", serde_json::to_string_pretty(&doc).unwrap());
}
