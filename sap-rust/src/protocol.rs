//! JSON-RPC 2.0 message types, per memory/head/gen/rust-fork/01-jsonrpc-spec.md.
//!
//! Wire framing is LSP-style `Content-Length` headers (see `framing.rs`), chosen
//! by the doc specifically to avoid newline-escaping issues in string params
//! like file paths.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), id, result: Some(result), error: None }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self { jsonrpc: "2.0".to_string(), id, result: None, error: Some(error) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<Value>,
}

/// SAP-specific error codes, layered on top of the standard JSON-RPC reserved
/// range (-32768..-32000). Standard codes below are used where they apply
/// (parse error, invalid request, method not found, invalid params); SAP adds
/// its own application codes above -32000 per the JSON-RPC 2.0 spec's
/// "implementation-defined server error" range.
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    /// Sent before `sap.hello` has completed the token handshake.
    pub const UNAUTHENTICATED: i64 = -32001;
    /// Sent for any project-scoped call before `project.select`, per 01's
    /// session-binding model.
    pub const NO_PROJECT_BOUND: i64 = -32002;
    /// Sent when `sap.hello`'s token does not match `SNAPSHOT_SAP_TOKEN`.
    pub const BAD_TOKEN: i64 = -32003;

    /// Sent when a resource referenced by a call (track index, project, etc.)
    /// does not exist. Maps from `backend::BackendError::NotFound`.
    pub const NOT_FOUND: i64 = -32004;

    /// Sent when `project.select` is called for a different project than the
    /// one this connection/session is already bound to, without an
    /// intervening `project.exit`. A same-project reselect is always allowed
    /// (idempotent no-op success), only a *different* target project trips
    /// this guard.
    pub const ALREADY_BOUND: i64 = -32005;

    pub fn message(code: i64) -> &'static str {
        match code {
            PARSE_ERROR => "Parse error",
            INVALID_REQUEST => "Invalid Request",
            METHOD_NOT_FOUND => "Method not found",
            INVALID_PARAMS => "Invalid params",
            INTERNAL_ERROR => "Internal error",
            UNAUTHENTICATED => "Unauthenticated: send sap.hello first",
            NO_PROJECT_BOUND => "No project bound: call project.select first",
            BAD_TOKEN => "Invalid token",
            NOT_FOUND => "Not found",
            ALREADY_BOUND => "Already bound to a different project: call project.exit first",
            _ => "Server error",
        }
    }
}

/// A fire-and-forget notification, per 01-jsonrpc-spec.md's "Notifications"
/// section (`edit.changed`, `notes.changed`, `project.dirty`, etc.) — no `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), method: method.into(), params }
    }
}

/// Outbound wire message: either a response to a specific request, or a
/// broadcast notification. Connections serialize either variant identically
/// over the wire (both are just JSON-RPC 2.0 objects); this enum only exists
/// to let the dispatcher/broadcast plumbing move them uniformly in-process.
#[derive(Debug, Clone)]
pub enum OutboundMessage {
    Response(RpcResponse),
    Notification(RpcNotification),
}

impl OutboundMessage {
    pub fn to_json(&self) -> Value {
        match self {
            OutboundMessage::Response(r) => serde_json::to_value(r).expect("response serializes"),
            OutboundMessage::Notification(n) => {
                serde_json::to_value(n).expect("notification serializes")
            }
        }
    }
}
