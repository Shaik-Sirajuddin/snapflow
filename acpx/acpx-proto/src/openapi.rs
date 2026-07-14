//! Builds `docs/schema/acpx-http.openapi.json`: a thin OpenAPI 3.1
//! companion document describing the HTTP transport *envelope* around
//! acpx's JSON-RPC body -- `POST /rpc`, the `GET /ws` upgrade, and,
//! critically, the three header-level contracts every real acpx HTTP/WS
//! client depends on (`Authorization`, `X-Acpx-Tenant`, `X-Acpx-Profile`)
//! that `acpx-wire.schema.json` (a bare JSON Schema body document) and
//! `acpx.openrpc.json` (OpenRPC has no header-parameter concept either)
//! both structurally cannot represent. This is the piece that closes
//! phase 21's explicit deferral -- see that phase's `COVERAGE.md` entry
//! and `memory/acpx/gen/plans/acpx-openrpc-schema/00-goal.md`'s "Why
//! OpenRPC, not OpenAPI" section for why this is a separate, small
//! document rather than folded into the OpenRPC one.
//!
//! Hand-composed, not generated from a router walk -- there are exactly
//! two fixed HTTP paths (`acpx-server/src/transport/http.rs`'s own doc
//! comment: "Exposes two endpoints on one axum router"), not worth
//! building path-discovery tooling for. Still shares the same
//! `$defs`/`components/schemas` registry as the other two documents
//! (`schema.rs`'s `register_all_defs`) for the JSON-RPC envelope types
//! it `$ref`s, so a body shape is never described three different ways
//! in three different files.
//!
//! stdio has no headers at all (no per-request envelope headers exist
//! on that transport -- see `acpx-server/src/main.rs`), so it is
//! deliberately absent from this document; `acpx.openrpc.json`'s
//! `servers` array is still the place stdio itself is listed as a
//! reachable transport.

use schemars::generate::SchemaSettings;
use serde_json::{json, Value};

use crate::openrpc::rewrite_refs_to_components;
use crate::schema::register_all_defs;

/// The `Authorization` header parameter object, shared by both paths.
/// Optional on the wire (see `http.rs`'s `AuthConfig`: unset
/// `ACPX_AUTH_TOKEN` means auth is disabled entirely, the default every
/// pre-existing test in this workspace relies on) -- `required: false`
/// reflects that acpx-server-side default, not a recommendation to skip
/// it in any real deployment exposed beyond loopback.
fn authorization_header() -> Value {
    json!({
        "name": "Authorization",
        "in": "header",
        "required": false,
        "description": "Bearer token, checked in constant time against ACPX_AUTH_TOKEN when that env var is set on the server (acpx-server/src/transport/http.rs's AuthConfig). Unset ACPX_AUTH_TOKEN means auth is disabled entirely -- this header is then ignored if sent and not required if omitted. When ACPX_AUTH_TOKEN is set, a missing or mismatched header yields 401 Unauthorized with a JSON-RPC-shaped error body (see the shared errorResponse schema on this document's responses).",
        "schema": {"type": "string", "pattern": "^Bearer .+$"},
        "example": "Bearer sk-acpx-...",
    })
}

/// The `X-Acpx-Tenant` header parameter object, shared by both paths.
/// See `acpx-server/src/transport/http.rs`'s `resolve_tenant` doc
/// comment and `memory/acpx/gen/plans/acpx-tenant-isolation/` for the
/// full design -- this is a self-declared data-partition key, not an
/// authentication mechanism (deliberately not verified against
/// anything), so a malformed or absent value fails open to the default
/// tenant rather than rejecting the request.
fn tenant_header() -> Value {
    json!({
        "name": "X-Acpx-Tenant",
        "in": "header",
        "required": false,
        "description": "Self-declared tenant partition key (acpx-tenant-isolation plan) applied to every method on this request/connection, not just session/new. Absent or not valid UTF-8 is treated identically to the implicit default tenant (\"default\") -- this is a data partition, not an auth gate, so it fails open rather than rejecting the request. For WS, this header is read once at upgrade time and cached for the connection's entire lifetime (a WS client is one fixed tenant for its whole connection).",
        "schema": {"type": "string", "default": "default"},
        "example": "acme-corp",
    })
}

/// The `X-Acpx-Profile` header parameter object -- `POST /rpc` only, per
/// `http.rs`'s own doc comment ("WS has no per-message header
/// equivalent"). Highest-precedence profile signal, above an inline
/// `params._acpx.profile` field the client may have set on the same
/// `session/new` request.
fn profile_header() -> Value {
    json!({
        "name": "X-Acpx-Profile",
        "in": "header",
        "required": false,
        "description": "Explicit profile selection for a session/new request on this transport -- POST /rpc only (no WS equivalent: WS headers are only present at the initial upgrade, not per-frame, so a WS client that wants managed mode must instead set params._acpx.profile inline on its session/new frame). Highest precedence: overwrites any inline params._acpx.profile value on the same request. Ignored on every method other than session/new.",
        "schema": {"type": "string"},
        "example": "work-openai",
    })
}

/// Builds the full OpenAPI document as a [`serde_json::Value`].
pub fn build_openapi_document() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    register_all_defs(&mut generator);
    let mut defs = Value::Object(generator.take_definitions(true));
    rewrite_refs_to_components(&mut defs);

    let rpc_request_body = json!({
        "required": true,
        "content": {
            "application/json": {
                "schema": {"$ref": "#/components/schemas/Request"},
            }
        }
    });
    let rpc_response = json!({
        "description": "JSON-RPC response (success or error). Always 200 OK -- JSON-RPC errors are reported via the body's error field, not the HTTP status.",
        "content": {
            "application/json": {
                "schema": {"$ref": "#/components/schemas/Response"},
            }
        }
    });
    let unauthorized_response = json!({
        "description": "Missing or invalid Authorization header while ACPX_AUTH_TOKEN is set on the server.",
        "content": {
            "application/json": {
                "schema": {"$ref": "#/components/schemas/Response"},
            }
        }
    });

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "acpx gateway HTTP/WS transport envelope",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Describes the HTTP transport envelope acpx-server wraps its JSON-RPC body in -- the two fixed paths (POST /rpc, GET /ws) and the three header-level contracts (Authorization, X-Acpx-Tenant, X-Acpx-Profile) a JSON Schema/OpenRPC body document has no place to describe. For the JSON-RPC body's own shape (every method's params/result), see docs/schema/acpx.openrpc.json. stdio is a separate transport with no headers and is not described here -- see acpx.openrpc.json's servers list.",
        },
        "servers": [
            {"url": "http://127.0.0.1:8790", "description": "default ACPX_HTTP_BIND"},
        ],
        "paths": {
            "/rpc": {
                "post": {
                    "summary": "JSON-RPC-over-HTTP",
                    "description": "Body is a raw JSON-RPC 2.0 request; response body is the JSON-RPC response.",
                    "parameters": [
                        authorization_header(),
                        tenant_header(),
                        profile_header(),
                    ],
                    "requestBody": rpc_request_body,
                    "responses": {
                        "200": rpc_response,
                        "401": unauthorized_response,
                    },
                }
            },
            "/ws": {
                "get": {
                    "summary": "WebSocket upgrade",
                    "description": "Upgrades to a persistent, full-duplex JSON-RPC connection carrying the same Request/Response frames as POST /rpc, plus live session/update notification frames (acpx_core::notify::NotificationHub). Authorization and X-Acpx-Tenant are read once at upgrade time and apply for the connection's entire lifetime -- there is no per-message re-check after the upgrade succeeds. X-Acpx-Profile has no effect on this transport (see that parameter's own description).",
                    "parameters": [authorization_header(), tenant_header()],
                    "responses": {
                        "101": {"description": "Switching Protocols -- upgrade succeeded."},
                        "401": unauthorized_response,
                    },
                }
            },
        },
        "components": {"schemas": defs},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_has_both_paths_with_header_parameters() {
        let doc = build_openapi_document();
        assert_eq!(doc["openapi"], json!("3.1.0"));
        let rpc_params = doc["paths"]["/rpc"]["post"]["parameters"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = rpc_params
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Authorization", "X-Acpx-Tenant", "X-Acpx-Profile"]
        );

        let ws_params = doc["paths"]["/ws"]["get"]["parameters"].as_array().unwrap();
        let ws_names: Vec<&str> = ws_params
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(ws_names, vec!["Authorization", "X-Acpx-Tenant"]);
    }

    #[test]
    fn refs_point_into_components_schemas_not_defs() {
        let doc = build_openapi_document();
        let dumped = serde_json::to_string(&doc).unwrap();
        assert!(
            !dumped.contains("#/$defs/"),
            "a raw $defs ref leaked through"
        );
        assert!(dumped.contains("#/components/schemas/Request"));
        assert!(dumped.contains("#/components/schemas/Response"));
    }
}
