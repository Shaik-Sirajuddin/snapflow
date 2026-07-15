//! Builds every generated schema document acpx ships (the bundled
//! acpx-native wire schema, and -- via [`register_all_defs`] -- the
//! shared `$defs`/`components/schemas` registry the OpenRPC document
//! (`openrpc.rs`) also builds on). Lives in the library, not a binary,
//! so the drift-guard tests (`tests/schema_test.rs`, `tests/
//! openrpc_test.rs`) can call the exact same code each generator binary
//! uses, rather than re-implementing it or shelling out to `cargo run`.

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{json, Map, Value};

use crate::agent::{AgentListEntry, AgentStatus};
use crate::gateway::{
    AgentIdParams, AgentInstallResult, AgentStatusResult, AgentsListResult,
    GatewaySessionListEntry, GatewaySessionListResult, McpServerEntry, McpServersListResult,
    NameOnlyParams, NameOnlyResult, PermissionPolicySchema, ProfileSchema, ProfilesListResult,
};
use crate::jsonrpc::{Request, RequestId, Response, RpcError};
use crate::session::{AcpxExt, GatewaySessionId, NewSessionParams};

// Upstream raw-ACP types referenced by `methods.rs`'s `SchemaRef::
// UpstreamAcp` entries -- imported under their bare names so
// `register_all_defs` reads as a flat list matching that table.
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, CreateTerminalRequest, CreateTerminalResponse, DeleteSessionRequest,
    DeleteSessionResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, KillTerminalRequest, KillTerminalResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, LogoutRequest, LogoutResponse,
    NewSessionResponse, PromptRequest, PromptResponse, ReadTextFileRequest, ReadTextFileResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, ResumeSessionResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, TerminalOutputRequest, TerminalOutputResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};

/// Registers every acpx-native wire type *and* every upstream raw-ACP
/// type `methods.rs`'s [`METHODS`] table references into one generator,
/// so both land in the same `$defs`/`components/schemas` map --
/// `build_wire_schema_document` and `openrpc.rs`'s builder both call
/// this rather than maintaining two separate registration lists that
/// could drift apart. This module's own test
/// (`every_method_schema_ref_is_registered`) fails the moment a
/// `methods.rs` entry names a type this function doesn't also register.
pub(crate) fn register_all_defs(generator: &mut SchemaGenerator) {
    // acpx-native types. Registering every type through the same
    // generator means shared substructure (e.g. `RequestId` appearing
    // inside both `Request` and `Response`) is emitted once into
    // `$defs` and `$ref`-ed everywhere else, instead of being
    // duplicated inline per root type.
    generator.subschema_for::<Request>();
    generator.subschema_for::<Response>();
    generator.subschema_for::<RpcError>();
    generator.subschema_for::<RequestId>();
    generator.subschema_for::<AcpxExt>();
    generator.subschema_for::<NewSessionParams>();
    generator.subschema_for::<GatewaySessionId>();
    generator.subschema_for::<AgentStatus>();
    generator.subschema_for::<AgentListEntry>();
    generator.subschema_for::<AgentIdParams>();
    generator.subschema_for::<AgentsListResult>();
    generator.subschema_for::<AgentInstallResult>();
    generator.subschema_for::<AgentStatusResult>();
    generator.subschema_for::<GatewaySessionListEntry>();
    generator.subschema_for::<GatewaySessionListResult>();
    generator.subschema_for::<McpServerEntry>();
    generator.subschema_for::<McpServersListResult>();
    generator.subschema_for::<NameOnlyParams>();
    generator.subschema_for::<NameOnlyResult>();
    generator.subschema_for::<PermissionPolicySchema>();
    generator.subschema_for::<ProfileSchema>();
    generator.subschema_for::<ProfilesListResult>();

    // Upstream raw-ACP types (`SchemaRef::UpstreamAcp` in `methods.rs`).
    // Never re-authored -- these calls pull in `agent_client_protocol`'s
    // own `#[derive(JsonSchema)]` output unmodified.
    generator.subschema_for::<InitializeRequest>();
    generator.subschema_for::<InitializeResponse>();
    generator.subschema_for::<AuthenticateRequest>();
    generator.subschema_for::<AuthenticateResponse>();
    generator.subschema_for::<LogoutRequest>();
    generator.subschema_for::<LogoutResponse>();
    generator.subschema_for::<NewSessionResponse>();
    generator.subschema_for::<PromptRequest>();
    generator.subschema_for::<PromptResponse>();
    generator.subschema_for::<ResumeSessionRequest>();
    generator.subschema_for::<ResumeSessionResponse>();
    generator.subschema_for::<LoadSessionRequest>();
    generator.subschema_for::<LoadSessionResponse>();
    generator.subschema_for::<CloseSessionRequest>();
    generator.subschema_for::<CloseSessionResponse>();
    generator.subschema_for::<SetSessionModeRequest>();
    generator.subschema_for::<SetSessionModeResponse>();
    generator.subschema_for::<SetSessionConfigOptionRequest>();
    generator.subschema_for::<SetSessionConfigOptionResponse>();
    generator.subschema_for::<CancelNotification>();
    generator.subschema_for::<DeleteSessionRequest>();
    generator.subschema_for::<DeleteSessionResponse>();
    generator.subschema_for::<ForkSessionRequest>();
    generator.subschema_for::<ForkSessionResponse>();
    generator.subschema_for::<ListSessionsRequest>();
    generator.subschema_for::<ListSessionsResponse>();
    generator.subschema_for::<RequestPermissionRequest>();
    generator.subschema_for::<RequestPermissionResponse>();
    generator.subschema_for::<ReadTextFileRequest>();
    generator.subschema_for::<ReadTextFileResponse>();
    generator.subschema_for::<WriteTextFileRequest>();
    generator.subschema_for::<WriteTextFileResponse>();
    generator.subschema_for::<CreateTerminalRequest>();
    generator.subschema_for::<CreateTerminalResponse>();
    generator.subschema_for::<TerminalOutputRequest>();
    generator.subschema_for::<TerminalOutputResponse>();
    generator.subschema_for::<ReleaseTerminalRequest>();
    generator.subschema_for::<ReleaseTerminalResponse>();
    generator.subschema_for::<WaitForTerminalExitRequest>();
    generator.subschema_for::<WaitForTerminalExitResponse>();
    generator.subschema_for::<KillTerminalRequest>();
    generator.subschema_for::<KillTerminalResponse>();
}

/// Builds the bundled acpx-native wire-schema document (`docs/schema/
/// acpx-wire.schema.json`) as a [`serde_json::Value`]. See `src/bin/
/// gen_schema.rs`'s doc comment for the "why not raw ACP too" rationale
/// this document's own scope still follows -- it only documents
/// acpx-native additions, even though `register_all_defs` (as of the
/// `acpx-openrpc-schema` plan) also knows about upstream raw ACP types
/// for the *other* two generated documents (`openrpc.rs`/`openapi.rs`)
/// to use. Deliberately does not call `register_all_defs` -- registers
/// only the acpx-native subset directly, so this document's `$defs`
/// stays exactly what it was before this plan (no unrelated upstream
/// types suddenly appearing in it).
pub fn build_wire_schema_document() -> Value {
    // draft2020-12: matches what schemars 1.x emits by default for
    // `$schema` metadata and is the draft every mainstream JSON Schema
    // validator (ajv, jsonschema, etc.) understands today.
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    generator.subschema_for::<Request>();
    generator.subschema_for::<Response>();
    generator.subschema_for::<RpcError>();
    generator.subschema_for::<RequestId>();
    generator.subschema_for::<AcpxExt>();
    generator.subschema_for::<NewSessionParams>();
    generator.subschema_for::<GatewaySessionId>();
    generator.subschema_for::<AgentStatus>();
    generator.subschema_for::<AgentListEntry>();
    generator.subschema_for::<AgentIdParams>();
    generator.subschema_for::<AgentsListResult>();
    generator.subschema_for::<AgentInstallResult>();
    generator.subschema_for::<AgentStatusResult>();
    generator.subschema_for::<GatewaySessionListEntry>();
    generator.subschema_for::<GatewaySessionListResult>();
    generator.subschema_for::<McpServerEntry>();
    generator.subschema_for::<McpServersListResult>();
    generator.subschema_for::<NameOnlyParams>();
    generator.subschema_for::<NameOnlyResult>();
    generator.subschema_for::<PermissionPolicySchema>();
    generator.subschema_for::<ProfileSchema>();
    generator.subschema_for::<ProfilesListResult>();

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
             fs/*, etc.) are NOT redefined here -- see `docs/schema/ \
             acpx.openrpc.json` (the full method registry, covering \
             every method both this gateway-native document and raw ACP \
             define) or docs/schema/README.md for the upstream \
             agent-client-protocol-schema crate this pulls those `$ref`s \
             from at the exact version acpx's Cargo.toml pins."
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
    use crate::methods::{SchemaRef, METHODS};

    #[test]
    fn document_is_a_request_or_response_union_with_defs() {
        let doc = build_wire_schema_document();
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
            "AgentIdParams",
            "AgentsListResult",
            "AgentInstallResult",
            "AgentStatusResult",
            "GatewaySessionListEntry",
            "GatewaySessionListResult",
            "McpServerEntry",
            "McpServersListResult",
            "NameOnlyParams",
            "NameOnlyResult",
            "PermissionPolicySchema",
            "ProfileSchema",
        ] {
            assert!(defs.contains_key(name), "missing $defs entry: {name}");
        }
    }

    /// `register_all_defs`'s own doc comment promises: every
    /// `SchemaRef` name any `methods.rs` entry references must actually
    /// land in its `$defs` output. Catches a method being added to
    /// `METHODS` (or a type being renamed) without a matching
    /// `subschema_for` call in `register_all_defs`.
    #[test]
    fn every_method_schema_ref_is_registered() {
        let mut generator = SchemaSettings::draft2020_12().into_generator();
        register_all_defs(&mut generator);
        let defs = generator.take_definitions(true);

        for entry in METHODS {
            for schema_ref in [entry.params, entry.result, entry.alternate_result]
                .into_iter()
                .flatten()
            {
                let name = match schema_ref {
                    SchemaRef::Native(n) => n,
                    SchemaRef::UpstreamAcp(n) => n,
                };
                assert!(
                    defs.contains_key(name),
                    "method {} references unregistered schema {name}",
                    entry.method
                );
            }
        }
    }
}
