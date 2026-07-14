//! Generates `docs/schema/acpx.openrpc.json`. See `src/openrpc.rs`'s
//! doc comment for what this document covers and why OpenRPC (not
//! OpenAPI) is the primary format for acpx's JSON-RPC method surface.
//!
//! Run via `cargo run -p acpx-proto --bin gen-openrpc`, which writes
//! the document to stdout -- `scripts/gen_openrpc.sh` redirects that
//! into the committed file and `acpx-proto/tests/openrpc_test.rs` fails
//! CI if the committed file has drifted from what this binary would
//! currently generate.

fn main() {
    let doc = acpx_proto::openrpc::build_openrpc_document();
    println!("{}", serde_json::to_string_pretty(&doc).unwrap());
}
