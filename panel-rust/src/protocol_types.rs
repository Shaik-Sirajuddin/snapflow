//! Shared ACP/ACPX-facing data model -- ported directly into
//! `panel-rust` (Phase 2 of `chat-panel-production-ui/execution-plan.md`:
//! "every gateway call flows through `acpx-client`", plus this plan's
//! own stated goal of deleting the `rui-acp-client`/`rui-acpx-client`
//! wrapper crates once nothing still needs their non-dead-code surface).
//!
//! These types used to live in `rui-acp-client::session_client` (the
//! direct-ACP-subprocess crate) and were re-exported, unchanged, through
//! `rui-acpx-client`'s own `lib.rs` so both client crates' actors could
//! share one event vocabulary. `rui-acp-client`'s own direct-ACP
//! `SessionClient`/`spawn_thread`/`ThreadHandle` machinery (the actual
//! reason that crate depended on `agent-client-protocol` directly) was
//! dead code at runtime -- `AgentBridge` has only ever routed through
//! `rui-acpx-client`'s gateway actor (see `execution-plan.md`'s Phase 2
//! note) -- so only this plain-data subset, which has zero dependency on
//! `agent-client-protocol`'s own wire types, needed to survive the port.
//! `crate::gateway_actor` (this crate's own port of `rui-acpx-client`'s
//! actor) and `crate::jsonl_store` (this crate's own port of
//! `rui-acp-client`'s jsonl cache) both build on these types directly.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    User,
    Agent,
    Thinking,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub kind: MessageKind,
    pub text: String,
    /// Tool-call execution status, rendered as an uppercased mono-font
    /// text badge in the UI. `None` for non-tool-call kinds and for
    /// every message cached before this field existed.
    /// `#[serde(default)]` so old `.jsonl` cache lines (written before
    /// this field existed) still deserialize without error.
    #[serde(default)]
    pub status: Option<String>,
}

/// Events flowing out of a bound thread's gateway actor, consumed from
/// `AcpxThreadHandle::take_events`.
#[derive(Debug)]
pub enum AgentEvent {
    Message(ChatMessage),
    /// A prompt turn finished; carries the ACP stop reason as a
    /// human-readable tag (`"end_turn"`, `"cancelled"`, etc.) rather
    /// than re-exporting the wire enum.
    TurnEnded(String),
    Error(String),
    /// A live agent-initiated request needing an interactive client
    /// decision -- `session/request_permission`, `fs/read_text_file`,
    /// `fs/write_text_file`, or `terminal/create`, relayed live over the
    /// acpx gateway's WS transport (see `acpx_core::agent_relay`'s
    /// module doc comment).
    PermissionRequest(AgentRequestEvent),
    /// A live output-buffer push from a `terminal/create`d command, via
    /// the gateway's `acpx/terminal_output` notification (see
    /// `acpx_core::router::spawn_terminal_output_stream`'s doc comment
    /// on the server side). Carries the *whole current buffer*, not a
    /// byte delta -- a client displaying this is expected to simply
    /// replace its shown contents each time, not append.
    TerminalOutput(TerminalOutputEvent),
    /// Session modes advertised by a `session/new`/`session/load`/
    /// `session/resume` response's `modes` field. Per
    /// agentclientprotocol.com's "Session Config Options" doc, `modes`
    /// is a legacy, superseded-by-`configOptions` shape that real
    /// backends still emit during the ACP ecosystem's transition
    /// period, so this is tracked as a real, currently-exercised
    /// capability rather than dead protocol surface.
    SessionModes(SessionModesEvent),
    /// A live `current_mode_update` notification's new `currentModeId`
    /// -- narrower than [`AgentEvent::SessionModes`]: this notification
    /// carries only the new id, not a refreshed `availableModes` list,
    /// so it is kept as its own event rather than folded into a
    /// re-sent `SessionModesEvent` with a guessed/stale `available`
    /// list.
    CurrentModeChanged(String),
    /// Session config options advertised by a `session/new`/`session/
    /// load`/`session/resume` response's `configOptions` field, or the
    /// *complete* replacement list carried by a live `config_option_
    /// update` notification or a `session/set_config_option` response
    /// (per agentclientprotocol.com: always the full current
    /// configuration state, never a delta -- so a consumer should
    /// simply replace its previously-held list on every occurrence of
    /// this variant, same "replace, don't append" contract
    /// [`AgentEvent::TerminalOutput`] documents for its own buffer).
    ConfigOptions(Vec<ConfigOptionInfo>),
}

/// One mode an ACP agent advertises as selectable for a session. See
/// [`AgentEvent::SessionModes`]'s doc comment for the wire origin and
/// why this crate still tracks the older `modes` shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// The full `modes` advertisement from a `session/new`/`session/load`/
/// `session/resume` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModesEvent {
    pub current_mode_id: String,
    pub available: Vec<SessionModeInfo>,
}

/// One selectable value inside a `select`-kind [`ConfigOptionInfo::
/// options`] list -- `{value, name, description?}` per
/// agentclientprotocol.com/protocol/session-config-options's documented
/// example response shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOptionValue {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// One entry of a `configOptions[]` list -- `{id, name, description?,
/// category?, type, currentValue?, options?}` per the real ACP spec
/// (verified against agentclientprotocol.com/protocol/session-config-
/// options directly). `kind` is `"select"` for every option type with
/// real backend coverage today; a `"boolean"` kind exists as an
/// accepted-but-not-yet-stable ACP RFD, so `kind` is kept as a plain
/// `String` (not a closed enum) to accept it or any future kind without
/// a parse failure -- a UI that doesn't recognize a `kind` can still
/// fall back to a generic read-only display of `current_value` rather
/// than dropping the option silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOptionInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub kind: String,
    pub current_value: Option<String>,
    pub options: Vec<ConfigOptionValue>,
}

/// See [`AgentEvent::TerminalOutput`]'s doc comment.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalOutputEvent {
    pub terminal_id: String,
    pub output: String,
    pub truncated: bool,
    /// `Some((exit_code, signal))` once the command has exited -- both
    /// inner fields individually optional per real ACP `ExitStatus`
    /// semantics (a signal-killed process has no exit code and vice
    /// versa).
    pub exit_status: Option<(Option<i32>, Option<i32>)>,
}

/// A pending interactive decision the UI must render and answer. Carries
/// the *raw* backend-native ACP request verbatim (`raw_request`) so a
/// panel reducer can pull out method-specific detail (permission
/// options + tool-call summary; `fs/*`'s `path`/`content`; `terminal/
/// create`'s `command`/`args`) without needing a bespoke typed field per
/// request kind -- consistent with `gateway_actor::classify_raw_update`'s
/// "operate on the raw JSON shape, don't re-derive a typed ACP schema"
/// convention.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRequestEvent {
    /// Echoed back unchanged to whichever `respond_*` call answers this
    /// request -- the relay's own correlation id, distinct from
    /// `raw_request`'s own JSON-RPC `id` (which belongs to the backend).
    pub relay_id: String,
    /// The relayed request's own ACP method name (`session/request_
    /// permission`, `fs/read_text_file`, `fs/write_text_file`, or
    /// `terminal/create`) -- the discriminator a reducer switches on to
    /// choose which request-card UI to render.
    pub method: String,
    /// Verbatim backend-native JSON-RPC request frame (`method`,
    /// `params`, and the backend's own `id`).
    pub raw_request: serde_json::Value,
}

/// One centrally-registered MCP server, as returned by `mcp_servers/
/// list` -- typed narrowly to the two fields a settings-gear list view
/// actually renders (`McpServerOption` in `panel-rust::models`), per
/// Phase 2 step 3's "no Slint-adjacent code sees raw JSON" goal.
/// `extra` retains the full original entry (env/args/url/whatever else
/// a real MCP server entry carries) as an opaque JSON object for a
/// future settings-sheet edit dialog that needs the complete payload --
/// `acpx-core::McpServerStore` itself never interprets more than
/// `"name"`, so this crate has no more reason to hand-type every field
/// than the server does.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerEntry {
    pub name: String,
    pub command: Option<String>,
    pub extra: serde_json::Value,
}

impl McpServerEntry {
    /// Parses one `mcp_servers/list` array entry. `None` only for an
    /// entry missing the required `"name"` field -- `acpx-core::
    /// McpServerStore::create`/`update` both reject such an entry
    /// server-side, so a well-behaved gateway never actually returns
    /// one, but this stays tolerant (skip, don't panic) rather than
    /// assuming that invariant holds forever.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let name = value.get("name")?.as_str()?.to_string();
        let command = value
            .get("command")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        Some(Self {
            name,
            command,
            extra: value.clone(),
        })
    }
}

/// Registry-reported install/detection status for one agent catalog
/// entry (`agents/list`/`agents/status`) -- mirrors `acpx_proto::
/// AgentStatus`'s own four-variant snake_case wire tag exactly
/// (`not_installed`/`installed`/`installed_no_session`/`runtime_
/// missing`, see that type's own doc comment for what each means).
/// Kept as this crate's own type (not a dependency on `acpx-proto`,
/// which `panel-rust` has no other reason to depend on) with an
/// `Unknown(String)` fallback so an unrecognized future status string
/// still displays as literal text instead of being dropped or causing
/// a parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    NotInstalled,
    Installed,
    InstalledNoSession,
    RuntimeMissing,
    Unknown(String),
}

impl AgentStatus {
    pub fn from_str(raw: &str) -> Self {
        match raw {
            "not_installed" => Self::NotInstalled,
            "installed" => Self::Installed,
            "installed_no_session" => Self::InstalledNoSession,
            "runtime_missing" => Self::RuntimeMissing,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// The same snake_case wire tag `from_str` accepts -- round-trips
    /// verbatim through this type rather than a UI-invented label, same
    /// "the panel has no independent opinion about what a real
    /// gateway's detection means" posture the pre-typed version of this
    /// data documented.
    pub fn as_wire_str(&self) -> &str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Installed => "installed",
            Self::InstalledNoSession => "installed_no_session",
            Self::RuntimeMissing => "runtime_missing",
            Self::Unknown(s) => s,
        }
    }
}

/// One agent-registry catalogue entry, as returned by `agents/list`
/// (each entry) or `agents/status` (one entry, keyed by the requested
/// id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCatalogEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub status: AgentStatus,
}

impl AgentCatalogEntry {
    /// `None` only for an entry missing the required `"id"` field --
    /// `acpx-registry`'s own schema requires it on every entry
    /// (verified against `registry.fallback.json`), so a well-behaved
    /// gateway never actually returns one, but this stays tolerant
    /// rather than assuming that invariant holds forever.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let name = value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let version = value
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let status = value
            .get("status")
            .and_then(|v| v.as_str())
            .map(AgentStatus::from_str)
            .unwrap_or(AgentStatus::Unknown(String::new()));
        Some(Self {
            id,
            name,
            version,
            status,
        })
    }
}

#[cfg(test)]
mod parsing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_server_entry_parses_name_and_command() {
        let value = json!({"name": "central-fs", "command": "mcp-central-fs"});
        let entry = McpServerEntry::from_json(&value).expect("entry");
        assert_eq!(entry.name, "central-fs");
        assert_eq!(entry.command.as_deref(), Some("mcp-central-fs"));
    }

    #[test]
    fn mcp_server_entry_none_without_command_is_still_valid() {
        let value = json!({"name": "url-only"});
        let entry = McpServerEntry::from_json(&value).expect("entry");
        assert_eq!(entry.command, None);
    }

    #[test]
    fn mcp_server_entry_is_none_without_a_name() {
        assert!(McpServerEntry::from_json(&json!({"command": "x"})).is_none());
    }

    #[test]
    fn agent_status_round_trips_every_known_wire_tag() {
        for tag in [
            "not_installed",
            "installed",
            "installed_no_session",
            "runtime_missing",
        ] {
            assert_eq!(AgentStatus::from_str(tag).as_wire_str(), tag);
        }
    }

    #[test]
    fn agent_status_unknown_tag_round_trips_as_literal_text() {
        let status = AgentStatus::from_str("future_status");
        assert_eq!(status, AgentStatus::Unknown("future_status".to_string()));
        assert_eq!(status.as_wire_str(), "future_status");
    }

    #[test]
    fn agent_catalog_entry_parses_full_shape() {
        let value = json!({
            "id": "codex-acp",
            "name": "Codex Agent",
            "version": "1.0.0",
            "status": "installed"
        });
        let entry = AgentCatalogEntry::from_json(&value).expect("entry");
        assert_eq!(entry.id, "codex-acp");
        assert_eq!(entry.name, "Codex Agent");
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(entry.status, AgentStatus::Installed);
    }

    #[test]
    fn agent_catalog_entry_is_none_without_an_id() {
        assert!(AgentCatalogEntry::from_json(&json!({"name": "x"})).is_none());
    }
}
