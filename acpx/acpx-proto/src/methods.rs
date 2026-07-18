//! Static registry mapping every JSON-RPC method `acpx-server` actually
//! dispatches to its params/result schema references. Source of truth
//! for two things at once: cross-checked directly against
//! `acpx-core/src/router.rs`'s `classify()` (the client-to-agent-facing
//! methods acpx's own JSON-RPC surface dispatches) and against
//! `agent-client-protocol` 1.2.0's own
//! `impl_jsonrpc_request!`/`impl_jsonrpc_notification!` macro
//! invocations in `src/schema/{client_to_agent,agent_to_client}/
//! requests.rs` and `notifications.rs` (both read directly this
//! session, not taken from a secondary summary) for the exact upstream
//! Req/Resp type name behind every raw-ACP method.
//!
//! Backs both `acpx-proto/src/schema.rs`'s `register_all_defs` (which
//! `subschema_for`s every type this table's [`SchemaRef`]s name, so
//! nothing in here is a dangling reference -- see `schema.rs`'s own
//! test) and the OpenRPC document builder (`openrpc.rs`).

/// Which of acpx-server's two duplex roles calls a given method --
/// mirrors upstream's own `x-side` schemars extension tag
/// (`agent-client-protocol-schema`'s `#[schemars(extend("x-side" =
/// ...))]`), restated here because acpx-native methods (`agents/*`,
/// `profiles/*`, `mcp_servers/*`) have no upstream tag to inherit at
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// A real client (editor/IDE/CLI) connected to `acpx-server` calls
    /// this; acpx answers, playing the role ACP calls "agent". Every
    /// method a client sends first.
    ClientToAgent,
    /// `acpx-server` itself calls this *out* to whatever real client is
    /// connected, relaying a request its own spawned backend process
    /// made *to* acpx (acpx plays ACP's "client" role toward that
    /// backend, then forwards outward one hop further). A client
    /// connecting to acpx must implement handlers for these too, not
    /// just send the `ClientToAgent` methods -- see
    /// `acpx-core/src/router.rs`'s `handle_fs_request`/`terminal/*`/
    /// `session/request_permission` forwarding.
    AgentToClient,
}

/// Where a params/result type's schema is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaRef {
    /// `$ref`s a type this crate (`acpx-proto`) derives `JsonSchema` on
    /// directly (`gateway.rs`/`session.rs`/`jsonrpc.rs`/`agent.rs`).
    Native(&'static str),
    /// `$ref`s a type from `agent_client_protocol::schema::v1`,
    /// registered into the same generator as every `Native` entry via
    /// `subschema_for` (see `schema.rs`'s `register_all_defs`) -- acpx
    /// never re-authors these, only points at upstream's own
    /// `#[derive(JsonSchema)]`.
    UpstreamAcp(&'static str),
}

/// One method's full schema description.
#[derive(Debug, Clone, Copy)]
pub struct MethodSchema {
    pub method: &'static str,
    pub side: Side,
    /// `None` means the method takes no params at all (e.g.
    /// `agents/list`), not that params are optional.
    pub params: Option<SchemaRef>,
    /// `None` means this is a true JSON-RPC notification with no result
    /// at all (`session/cancel`), not that the result is `null`.
    pub result: Option<SchemaRef>,
    /// Set only for `session/list`: when the caller supplies a
    /// selector, `router.rs`'s `dispatch_session_list_real` proxies to
    /// the real backend's `ListSessionsResponse` instead of `result`'s
    /// gateway-native default shape -- both are real, so both are
    /// recorded rather than picking one and silently misdescribing the
    /// other (see `gateway.rs`'s `GatewaySessionListResult` doc
    /// comment).
    pub alternate_result: Option<SchemaRef>,
}

use SchemaRef::{Native, UpstreamAcp};
use Side::{AgentToClient, ClientToAgent};

/// Every method `acpx-server` dispatches, client-to-agent methods first
/// (in `router.rs`'s `classify()` order) then agent-to-client methods
/// (in `agent_to_client/requests.rs` order).
pub const METHODS: &[MethodSchema] = &[
    MethodSchema {
        method: "initialize",
        side: ClientToAgent,
        params: Some(UpstreamAcp("InitializeRequest")),
        result: Some(UpstreamAcp("InitializeResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "authenticate",
        side: ClientToAgent,
        params: Some(UpstreamAcp("AuthenticateRequest")),
        result: Some(UpstreamAcp("AuthenticateResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "logout",
        side: ClientToAgent,
        params: Some(UpstreamAcp("LogoutRequest")),
        result: Some(UpstreamAcp("LogoutResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/new",
        side: ClientToAgent,
        // Native, not upstream: carries the acpx-only `_acpx` sibling
        // field (profile selection) alongside the real ACP shape --
        // see `session.rs`'s own doc comment for why this is a
        // deliberate partial mirror, not a full replacement.
        params: Some(Native("NewSessionParams")),
        // The response body is upstream `NewSessionResponse`'s shape
        // byte-for-byte except `sessionId` is a gateway-minted id
        // (`GatewaySessionId`), not the backend's own -- still
        // string-shaped either way, so referencing the upstream result
        // type is accurate.
        result: Some(UpstreamAcp("NewSessionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/prompt",
        side: ClientToAgent,
        params: Some(UpstreamAcp("PromptRequest")),
        result: Some(UpstreamAcp("PromptResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/resume",
        side: ClientToAgent,
        params: Some(UpstreamAcp("ResumeSessionRequest")),
        result: Some(UpstreamAcp("ResumeSessionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/load",
        side: ClientToAgent,
        params: Some(UpstreamAcp("LoadSessionRequest")),
        result: Some(UpstreamAcp("LoadSessionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/close",
        side: ClientToAgent,
        params: Some(UpstreamAcp("CloseSessionRequest")),
        result: Some(UpstreamAcp("CloseSessionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/set_mode",
        side: ClientToAgent,
        params: Some(UpstreamAcp("SetSessionModeRequest")),
        result: Some(UpstreamAcp("SetSessionModeResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "session/set_config_option",
        side: ClientToAgent,
        params: Some(UpstreamAcp("SetSessionConfigOptionRequest")),
        result: Some(UpstreamAcp("SetSessionConfigOptionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        // Notification shape (`id` optional, no reply ever sent) --
        // see `router.rs`'s `dispatch_session_cancel` doc comment.
        method: "session/cancel",
        side: ClientToAgent,
        params: Some(UpstreamAcp("CancelNotification")),
        result: None,
        alternate_result: None,
    },
    MethodSchema {
        method: "session/delete",
        side: ClientToAgent,
        params: Some(UpstreamAcp("DeleteSessionRequest")),
        result: Some(UpstreamAcp("DeleteSessionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        // **ACP compatibility gap closed post-review.** Real, stable v1
        // ACP compatibility gap closed post-review.** Real (but, per
        // upstream's own `unstable_session_fork` Cargo feature, not yet
        // stabilized) v1 ACP method (`ForkSessionRequest`/
        // `ForkSessionResponse`, `x-side: agent`) previously entirely
        // unclassified in `router.rs`'s `classify()` -- see
        // `MethodClass::SessionFork`'s doc comment there for the full
        // story, including the real `claude-agent-acp` 0.58.1 adapter
        // verified to advertise `sessionCapabilities.fork` support
        // despite the draft status upstream. Response mints a new
        // session id (like `session/new`'s `NewSessionResponse`), so its
        // result type -- unlike every other `Proxied` method's -- is not
        // shaped identically to its request.
        method: "session/fork",
        side: ClientToAgent,
        params: Some(UpstreamAcp("ForkSessionRequest")),
        result: Some(UpstreamAcp("ForkSessionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        // Dual-shape: no selector -> gateway-native aggregate list
        // (`result`); selector present -> proxies to the real backend's
        // `ListSessionsResponse` (`alternate_result`). Params always
        // upstream-shaped since the selector fields themselves live in
        // `ListSessionsRequest`.
        method: "session/list",
        side: ClientToAgent,
        params: Some(UpstreamAcp("ListSessionsRequest")),
        result: Some(Native("GatewaySessionListResult")),
        alternate_result: Some(UpstreamAcp("ListSessionsResponse")),
    },
    MethodSchema {
        method: "agents/list",
        side: ClientToAgent,
        params: None,
        result: Some(Native("AgentsListResult")),
        alternate_result: None,
    },
    MethodSchema {
        method: "agents/install",
        side: ClientToAgent,
        params: Some(Native("AgentIdParams")),
        result: Some(Native("AgentInstallResult")),
        alternate_result: None,
    },
    MethodSchema {
        method: "agents/status",
        side: ClientToAgent,
        params: Some(Native("AgentIdParams")),
        result: Some(Native("AgentStatusResult")),
        alternate_result: None,
    },
    MethodSchema {
        method: "profiles/create",
        side: ClientToAgent,
        params: Some(Native("ProfileSchema")),
        result: Some(Native("ProfileSchema")),
        alternate_result: None,
    },
    MethodSchema {
        method: "profiles/update",
        side: ClientToAgent,
        params: Some(Native("ProfileSchema")),
        result: Some(Native("ProfileSchema")),
        alternate_result: None,
    },
    MethodSchema {
        method: "profiles/list",
        side: ClientToAgent,
        params: None,
        result: Some(Native("ProfilesListResult")),
        alternate_result: None,
    },
    MethodSchema {
        method: "profiles/delete",
        side: ClientToAgent,
        params: Some(Native("NameOnlyParams")),
        result: Some(Native("NameOnlyResult")),
        alternate_result: None,
    },
    MethodSchema {
        method: "mcp_servers/create",
        side: ClientToAgent,
        params: Some(Native("McpServerEntry")),
        result: Some(Native("McpServerEntry")),
        alternate_result: None,
    },
    MethodSchema {
        method: "mcp_servers/update",
        side: ClientToAgent,
        params: Some(Native("McpServerEntry")),
        result: Some(Native("McpServerEntry")),
        alternate_result: None,
    },
    MethodSchema {
        method: "mcp_servers/list",
        side: ClientToAgent,
        params: None,
        result: Some(Native("McpServersListResult")),
        alternate_result: None,
    },
    MethodSchema {
        method: "mcp_servers/delete",
        side: ClientToAgent,
        params: Some(Native("NameOnlyParams")),
        result: Some(Native("NameOnlyResult")),
        alternate_result: None,
    },
    // -- Agent-to-client methods below: acpx-server calls these OUT to
    // whatever real client is connected, relaying its own spawned
    // backend's request one hop further (see `Side::AgentToClient`'s
    // doc comment and `router.rs`'s `handle_fs_request`/`terminal/*`
    // handling).
    MethodSchema {
        method: "session/request_permission",
        side: AgentToClient,
        params: Some(UpstreamAcp("RequestPermissionRequest")),
        result: Some(UpstreamAcp("RequestPermissionResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "fs/read_text_file",
        side: AgentToClient,
        params: Some(UpstreamAcp("ReadTextFileRequest")),
        result: Some(UpstreamAcp("ReadTextFileResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "fs/write_text_file",
        side: AgentToClient,
        params: Some(UpstreamAcp("WriteTextFileRequest")),
        result: Some(UpstreamAcp("WriteTextFileResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "terminal/create",
        side: AgentToClient,
        params: Some(UpstreamAcp("CreateTerminalRequest")),
        result: Some(UpstreamAcp("CreateTerminalResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "terminal/output",
        side: AgentToClient,
        params: Some(UpstreamAcp("TerminalOutputRequest")),
        result: Some(UpstreamAcp("TerminalOutputResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "terminal/release",
        side: AgentToClient,
        params: Some(UpstreamAcp("ReleaseTerminalRequest")),
        result: Some(UpstreamAcp("ReleaseTerminalResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "terminal/wait_for_exit",
        side: AgentToClient,
        params: Some(UpstreamAcp("WaitForTerminalExitRequest")),
        result: Some(UpstreamAcp("WaitForTerminalExitResponse")),
        alternate_result: None,
    },
    MethodSchema {
        method: "terminal/kill",
        side: AgentToClient,
        params: Some(UpstreamAcp("KillTerminalRequest")),
        result: Some(UpstreamAcp("KillTerminalResponse")),
        alternate_result: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in METHODS {
            assert!(
                seen.insert(entry.method),
                "duplicate method entry: {}",
                entry.method
            );
        }
    }

    #[test]
    fn matches_router_classify_client_to_agent_method_count() {
        // Cross-checked directly against `acpx-core::router::classify`'s
        // match arms (25 client-to-agent methods total) -- see this
        // module's doc comment. Kept as a plain count (rather than
        // importing `acpx-core`, which would invert the crate-layering
        // rule `gateway.rs` documents) so this file still catches a
        // method being silently added/removed here without the count
        // being revisited.
        let client_to_agent = METHODS.iter().filter(|m| m.side == ClientToAgent).count();
        assert_eq!(client_to_agent, 25);
        let agent_to_client = METHODS.iter().filter(|m| m.side == AgentToClient).count();
        assert_eq!(agent_to_client, 8);
    }
}
