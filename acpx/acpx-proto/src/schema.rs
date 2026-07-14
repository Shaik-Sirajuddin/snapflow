//! Builds the bundled acpx-native wire-schema document (see
//! `src/bin/gen_schema.rs`'s doc comment for the "why not raw ACP too"
//! rationale). Lives in the library, not the `gen-schema` binary, so
//! `tests/schema_test.rs` can call the exact same code the binary uses to
//! detect drift, rather than re-implementing it or shelling out to
//! `cargo run`.

use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde_json::{json, Map, Value};

use crate::agent::{AgentListEntry, AgentStatus};
use crate::jsonrpc::{Request, RequestId, Response, RpcError};
use crate::session::{AcpxExt, GatewaySessionId, NewSessionParams};

/// Builds the full bundled schema document as a [`serde_json::Value`].
pub fn build_schema_document() -> Value {
    // draft2020-12: matches what schemars 1.x emits by default for
    // `$schema` metadata and is the draft every mainstream JSON Schema
    // validator (ajv, jsonschema, etc.) understands today.
    let mut generator = SchemaSettings::draft2020_12().into_generator();

    // Registering every type through the same generator means shared
    // substructure (e.g. `RequestId` appearing inside both `Request` and
    // `Response`) is emitted once into `$defs` and `$ref`-ed everywhere
    // else, instead of being duplicated inline per root type.
    generator.subschema_for::<Request>();
    generator.subschema_for::<Response>();
    generator.subschema_for::<RpcError>();
    generator.subschema_for::<RequestId>();
    generator.subschema_for::<AcpxExt>();
    generator.subschema_for::<NewSessionParams>();
    generator.subschema_for::<GatewaySessionId>();
    generator.subschema_for::<AgentStatus>();
    generator.subschema_for::<AgentListEntry>();

    let defs = generator.take_definitions(true);

    // Root-level `oneOf` documents the one true invariant of every acpx
    // transport (stdio/HTTP/WS): a framed message is always either a
    // `Request` (covers requests *and* notifications -- `id` is merely
    // optional, see `jsonrpc.rs`) or a `Response`. Everything else in
    // `$defs` (the `_acpx` extension shapes, `agents/*` payloads) is
    // reachable by `$ref` from tooling but isn't itself framed alone on
    // the wire, so it isn't part of this top-level union.
    let mut root: Map<String, Value> = Map::new();
    root.insert(
        "$schema".to_string(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    root.insert(
        "$id".to_string(),
        json!("https://acpx.dev/schema/acpx-wire.schema.json"),
    );
    root.insert(
        "title".to_string(),
        json!("acpx gateway wire schema (acpx-native additions)"),
    );
    root.insert(
        "description".to_string(),
        json!(
            "Covers the JSON-RPC 2.0 envelope every acpx-server transport \
             (stdio/HTTP/WS) frames messages in, plus acpx-native \
             extensions layered on top of raw ACP: the `_acpx` sibling \
             field on `session/new` and the `agents/*` gateway-management \
             methods. Raw ACP method param/result shapes (session/prompt, \
             fs/*, etc.) are NOT redefined here -- see \
             docs/schema/README.md for where to get those from the \
             upstream agent-client-protocol-schema crate at the exact \
             version acpx's Cargo.toml pins."
        ),
    );
    root.insert(
        "oneOf".to_string(),
        json!([
            {"$ref": format!("#/$defs/{}", Request::schema_name())},
            {"$ref": format!("#/$defs/{}", Response::schema_name())},
        ]),
    );
    root.insert("$defs".to_string(), Value::Object(defs));
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_is_a_request_or_response_union_with_defs() {
        let doc = build_schema_document();
        assert_eq!(doc["oneOf"].as_array().unwrap().len(), 2);
        let defs = doc["$defs"].as_object().unwrap();
        for name in [
            "Request",
            "Response",
            "RpcError",
            "RequestId",
            "AcpxExt",
            "NewSessionParams",
            "GatewaySessionId",
            "AgentStatus",
            "AgentListEntry",
            "JsonRpcVersion",
        ] {
            assert!(defs.contains_key(name), "missing $defs entry: {name}");
        }
    }
}
