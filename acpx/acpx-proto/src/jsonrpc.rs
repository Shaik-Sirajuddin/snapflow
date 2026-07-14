//! JSON-RPC 2.0 envelope framing shared by every acpx transport
//! (stdio/HTTP/WebSocket). This is transport-agnostic on purpose: transports
//! in `acpx-server` decide how bytes become one of these values (newline
//! framing over stdio, message framing over WebSocket, a request body over
//! HTTP), but the envelope shape itself is defined once, here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 request or notification (notifications omit `id`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    pub jsonrpc: JsonRpcVersion,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response: exactly one of `result`/`error` is present.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC ids are either a string or a number on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// Always `"2.0"`; a distinct type (not a bare `String`) so a malformed
/// envelope fails to deserialize instead of silently round-tripping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpcVersion;

use std::borrow::Cow;

// `JsonSchema` is implemented by hand (rather than derived) since this
// type has a custom `Serialize`/`Deserialize` pair that encodes/decodes it
// as the literal string `"2.0"`, not as a unit struct -- the schema must
// describe that wire shape, not the Rust shape.
impl JsonSchema for JsonRpcVersion {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("JsonRpcVersion")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed("acpx_proto::jsonrpc::JsonRpcVersion")
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "const": "2.0",
            "description": "Always the literal string \"2.0\"."
        })
    }
}

impl Serialize for JsonRpcVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s == "2.0" {
            Ok(JsonRpcVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported jsonrpc version: {s}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = Request {
            jsonrpc: JsonRpcVersion,
            id: Some(RequestId::Number(1)),
            method: "session/new".to_string(),
            params: Some(serde_json::json!({"cwd": "/tmp"})),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "session/new");
    }
}
