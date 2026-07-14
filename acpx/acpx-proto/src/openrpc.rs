//! Builds `docs/schema/acpx.openrpc.json`: an OpenRPC 1.3.2 document
//! covering every method `methods.rs`'s [`METHODS`] table describes --
//! the full 32-method surface `acpx-server` dispatches across all three
//! transports (stdio/HTTP/WS), including raw ACP methods `$ref`-ed
//! straight from upstream `agent_client_protocol`'s own
//! `#[derive(JsonSchema)]` output (see `schema.rs`'s
//! `register_all_defs`), not just acpx-native additions -- this is the
//! document that actually answers "why is the JSON Schema so little",
//! not `acpx-wire.schema.json` (which is deliberately scoped to
//! acpx-native additions only, see that document's own description).
//!
//! Chose OpenRPC over OpenAPI as the primary format because acpx-server
//! is JSON-RPC method-dispatch over one logical endpoint per transport
//! -- the method name embedded in the body selects the shape, not a URL
//! path -- which is exactly OpenRPC's `methods: []` object model and
//! not what OpenAPI's per-path-per-verb model was built to describe.
//! See `memory/acpx/gen/plans/acpx-openrpc-schema/00-goal.md` for the
//! full rationale. The HTTP transport's own envelope (paths, headers)
//! gets a separate, thin OpenAPI companion document instead
//! (`openapi.rs`) -- deliberately not folded into this one.

use schemars::generate::SchemaSettings;
use serde_json::{json, Map, Value};

use crate::methods::{MethodSchema, SchemaRef, Side, METHODS};
use crate::schema::register_all_defs;

/// Rewrites every `"$ref": "#/$defs/Name"` string in `value` to
/// `"#/components/schemas/Name"` in place -- `schemars` always emits
/// the former (JSON Schema's own convention); OpenRPC's `components.
/// schemas` map (borrowed from OpenAPI 3) is addressed the latter way.
/// Recurses into every object/array since a `$ref` can appear at any
/// nesting depth (inside `properties`, `items`, `anyOf`, ...).
pub(crate) fn rewrite_refs_to_components(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get_mut("$ref") {
                if let Some(rest) = s.strip_prefix("#/$defs/") {
                    *s = format!("#/components/schemas/{rest}");
                }
            }
            for v in map.values_mut() {
                rewrite_refs_to_components(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                rewrite_refs_to_components(v);
            }
        }
        _ => {}
    }
}

/// One method's `SchemaRef` resolved to a `{"$ref": "#/components/schemas/Name"}` value.
fn schema_ref_value(schema_ref: SchemaRef) -> Value {
    let name = match schema_ref {
        SchemaRef::Native(n) => n,
        SchemaRef::UpstreamAcp(n) => n,
    };
    json!({"$ref": format!("#/components/schemas/{name}")})
}

/// Builds one OpenRPC Method Object for a `methods.rs` entry.
fn method_object(entry: &MethodSchema) -> Value {
    let mut obj: Map<String, Value> = Map::new();
    obj.insert("name".to_string(), json!(entry.method));

    let params: Vec<Value> = match entry.params {
        Some(schema_ref) => vec![json!({
            "name": "params",
            "schema": schema_ref_value(schema_ref),
        })],
        // OpenRPC's `params` is a required array; empty means "no
        // params" (`agents/list`, `profiles/list`, `mcp_servers/list`).
        None => Vec::new(),
    };
    obj.insert("params".to_string(), json!(params));

    match entry.result {
        Some(schema_ref) => {
            obj.insert(
                "result".to_string(),
                json!({"name": "result", "schema": schema_ref_value(schema_ref)}),
            );
        }
        // `session/cancel` only: a true JSON-RPC notification, never
        // answered. OpenRPC has no first-class "this is a notification"
        // flag on a Method Object (its `x-*` extension convention is
        // the sanctioned escape hatch for exactly this), so acpx uses
        // its own extension key instead of omitting `result` silently
        // (which OpenRPC tooling would likely treat as a schema-author
        // mistake, not a deliberate no-reply method).
        None => {
            obj.insert("x-acpx-notification".to_string(), json!(true));
        }
    }

    if let Some(alternate) = entry.alternate_result {
        // `session/list` only, documented on that entry in `methods.rs`.
        obj.insert(
            "x-acpx-alternate-result".to_string(),
            schema_ref_value(alternate),
        );
    }

    obj.insert(
        "x-acpx-side".to_string(),
        json!(match entry.side {
            Side::ClientToAgent => "client-to-agent",
            Side::AgentToClient => "agent-to-client",
        }),
    );

    Value::Object(obj)
}

/// Builds the full OpenRPC document as a [`serde_json::Value`].
pub fn build_openrpc_document() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    register_all_defs(&mut generator);
    let mut defs = Value::Object(generator.take_definitions(true));
    rewrite_refs_to_components(&mut defs);

    let methods: Vec<Value> = METHODS.iter().map(method_object).collect();

    json!({
        "openrpc": "1.3.2",
        "info": {
            "title": "acpx gateway",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Full JSON-RPC method registry acpx-server dispatches \
                across every transport it exposes (stdio, HTTP POST /rpc, WS \
                GET /ws -- all three share one dispatch surface, see \
                docs/schema/README.md). Covers both directions: \
                `x-acpx-side: client-to-agent` methods a connecting client \
                sends to acpx, and `x-acpx-side: agent-to-client` methods \
                acpx itself calls back out to that same connected client \
                (relaying a request acpx's own spawned backend made to acpx \
                one hop further). Raw ACP method shapes are $ref'd directly \
                from the upstream agent-client-protocol crate's own derived \
                schema, never re-authored -- see components.schemas for the \
                merged acpx-native + upstream-referenced type registry.",
        },
        "servers": [
            {"name": "stdio", "url": "stdio://acpx-server"},
            {"name": "http", "url": "http://127.0.0.1:8790/rpc"},
            {"name": "ws", "url": "ws://127.0.0.1:8790/ws"},
        ],
        "methods": methods,
        "components": {"schemas": defs},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_has_every_method_with_a_valid_shape() {
        let doc = build_openrpc_document();
        assert_eq!(doc["openrpc"], json!("1.3.2"));
        let methods = doc["methods"].as_array().unwrap();
        assert_eq!(methods.len(), METHODS.len());

        for (entry, value) in METHODS.iter().zip(methods) {
            assert_eq!(value["name"], json!(entry.method));
            assert!(value["params"].is_array());
            if entry.result.is_some() {
                assert!(
                    value.get("result").is_some(),
                    "{} missing result",
                    entry.method
                );
            } else {
                assert_eq!(value["x-acpx-notification"], json!(true));
            }
        }
    }

    #[test]
    fn refs_point_into_components_schemas_not_defs() {
        let doc = build_openrpc_document();
        let dumped = serde_json::to_string(&doc).unwrap();
        assert!(
            !dumped.contains("#/$defs/"),
            "a raw $defs ref leaked through"
        );
        assert!(dumped.contains("#/components/schemas/"));
    }

    #[test]
    fn session_list_carries_its_alternate_result() {
        let doc = build_openrpc_document();
        let session_list = doc["methods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == json!("session/list"))
            .unwrap();
        assert!(session_list.get("x-acpx-alternate-result").is_some());
    }
}
