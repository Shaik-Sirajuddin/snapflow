//! Generates `docs/schema/acpx-wire.schema.json`: a single JSON Schema
//! document describing every acpx-*native* wire type -- the JSON-RPC
//! envelope (`jsonrpc.rs`) plus the `_acpx` extension/agent-management
//! payloads (`session.rs`/`agent.rs`).
//!
//! **Deliberately does not re-derive raw ACP method shapes.** Per
//! `lib.rs`'s doc comment, `acpx-proto` re-exports `agent_client_protocol`
//! as the single source of truth for those, and that crate's own
//! `agent-client-protocol-schema` sibling already publishes a versioned
//! `schema.json` on GitHub releases -- duplicating that generation here
//! would just drift out of sync with whatever `agent-client-protocol`
//! version `[workspace.dependencies]` in `Cargo.toml` is pinned to.
//! `docs/schema/README.md` links to it by version instead. What acpx
//! *does* add on the wire (this file's job) is everything else a client
//! needs to talk to `acpx-server` specifically: the envelope every
//! transport frames (`stdio`/HTTP/WS all carry the same
//! `Request`/`Response` shape, see `acpx-server/src/transport/`), the
//! `_acpx` sibling field on `session/new`, and the gateway-native
//! `agents/*` methods that have no raw-ACP equivalent at all.
//!
//! Run via `cargo run -p acpx-proto --bin gen-schema`, which writes the
//! document to stdout -- `scripts/gen_schema.sh` redirects that into the
//! committed file and `acpx-proto/tests/schema_test.rs` fails CI if the
//! committed file has drifted from what this binary would currently
//! generate (i.e. someone changed a wire type without regenerating).

fn main() {
    let doc = acpx_proto::schema::build_wire_schema_document();
    println!("{}", serde_json::to_string_pretty(&doc).unwrap());
}
