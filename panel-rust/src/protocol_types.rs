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
    /// Phase 2 step 3 addition: the wire's own `messageId` (for
    /// `agent_message_chunk`/`agent_thought_chunk`) or `toolCallId`
    /// (for `tool_call`/`tool_call_update`), when the backend provided
    /// one -- `None` for `user_message_chunk` (never carries an id in
    /// this crate's usage) and for every message cached before this
    /// field existed. Lets `AgentBridge`'s transcript reducer
    /// (`conversation::ConversationState`) merge streamed chunks/tool
    /// updates by id instead of treating every chunk as its own
    /// message -- see `agent_bridge.rs`'s ingestion logic for the
    /// synthetic-id fallback this crate uses when a real v1 backend
    /// omits `messageId` (an RFD, not required in v1 --
    /// agentclientprotocol.com/rfds/message-id). `#[serde(default)]`
    /// for the same old-cache-line compatibility reason as `status`.
    #[serde(default)]
    pub id: Option<String>,
    /// chat-items-redesign.md #9 (execution-view "api-call" variant):
    /// the tool call's own `rawInput`/`rawOutput` wire fields, when the
    /// backend provided them -- real structured payload data ACP already
    /// carries (`ToolCallUpdateFields`/`ToolCall` in `agent-client-
    /// protocol`), just not previously read by `classify_raw_update`.
    /// `None` for non-tool-call kinds and for every message cached
    /// before this field existed; `#[serde(default)]` for the same
    /// old-cache-line compatibility reason as `status`/`id`.
    #[serde(default)]
    pub raw_input: Option<serde_json::Value>,
    #[serde(default)]
    pub raw_output: Option<serde_json::Value>,
}

/// Server-owned queue snapshot delivered on the separate ACPX queue stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItemInfo {
    pub queue_entry_id: String,
    pub idempotency_key: String,
    pub text: String,
    pub state: String,
    pub position: u32,
}

/// Outcome of a relayed agent request. `selected=false` tells a connected
/// panel that another client won the approval race and its card is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResolutionEvent {
    pub relay_id: String,
    pub selected: bool,
    pub late: bool,
}

/// Longest single JSON string leaf kept inside a tool call's
/// `rawInput`/`rawOutput` payload.
///
/// A tool that returns an image (an image-generation skill, an MCP tool
/// answering with an `{"type":"image","data":"<base64>"}` content block)
/// puts megabytes of base64 into exactly one string leaf. That string is
/// re-serialized into the transcript on every ingested chunk
/// (`conversation::rebuild_from_chat_messages`), deep-cloned out of the
/// bridge on every 60-90fps poll tick
/// (`external_snapshot::collect_thread_snapshot_for`), and finally handed
/// to Slint as the `raw-output` of a wrapping, uncapped, read-only
/// `TextInput` (`ui/base/terminal_log_block.slint`) whose height *is* its
/// own `preferred-height`. The software renderer's physical coordinate
/// space is `i16` (`i-slint-renderer-software`'s `PhysicalRect =
/// euclid::Rect<i16, PhysicalPx>`), and euclid's `cast()` is
/// `try_cast().unwrap()` -- so once that text wraps past 32767 physical
/// pixels of height, rendering it panics, and this crate builds with
/// `panic = "abort"`.
pub const MAX_RAW_PAYLOAD_STRING_BYTES: usize = 4 * 1024;

/// Cap on the whole serialized payload, applied after
/// [`elide_large_payload_strings`] so a payload that is large by breadth
/// (thousands of small fields) is bounded too.
pub const MAX_RAW_PAYLOAD_TOTAL_BYTES: usize = 32 * 1024;

/// Truncates `s` to at most `max` bytes on a char boundary, appending a
/// marker naming how much was dropped. No-op when already within `max`.
fn truncate_with_marker(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let dropped = s.len() - max;
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(&format!("\u{2026} <{dropped} more bytes elided>"));
}

/// Replaces every oversized string leaf in `value` with a truncated
/// preview, in place, leaving the JSON *structure* (and therefore every
/// small field, e.g. the `skill` key `models::classify_tool_call_kind`
/// probes for) intact. See [`MAX_RAW_PAYLOAD_STRING_BYTES`].
pub fn elide_large_payload_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => truncate_with_marker(s, MAX_RAW_PAYLOAD_STRING_BYTES),
        serde_json::Value::Array(items) => items.iter_mut().for_each(elide_large_payload_strings),
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(elide_large_payload_strings)
        }
        _ => {}
    }
}

/// The display string for a tool payload: `value` serialized, bounded by
/// both caps above. Every path that turns a stored `raw_input`/
/// `raw_output` `Value` into the `SharedString` Slint renders goes
/// through here, so a payload cached to jsonl before the ingestion-side
/// bound existed is bounded on the way out too.
pub fn bounded_payload_display_string(value: &serde_json::Value) -> String {
    // Scan before cloning: on the live path the payload was already
    // bounded at ingestion, and this runs once per stored tool row on
    // every transcript rebuild (i.e. once per streamed chunk).
    let mut out = if has_oversized_string(value) {
        let mut owned = value.clone();
        elide_large_payload_strings(&mut owned);
        owned.to_string()
    } else {
        value.to_string()
    };
    truncate_with_marker(&mut out, MAX_RAW_PAYLOAD_TOTAL_BYTES);
    out
}

fn has_oversized_string(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => s.len() > MAX_RAW_PAYLOAD_STRING_BYTES,
        serde_json::Value::Array(items) => items.iter().any(has_oversized_string),
        serde_json::Value::Object(map) => map.values().any(has_oversized_string),
        _ => false,
    }
}

/// Events flowing out of a bound thread's gateway actor, consumed from
/// `AcpxThreadHandle::take_events`.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Message(ChatMessage),
    HistoryPage {
        messages: Vec<ChatMessage>,
        next_cursor: Option<String>,
    },
    QueueChanged {
        items: Vec<QueueItemInfo>,
        paused: bool,
    },
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
    AgentResolution(AgentResolutionEvent),
    SessionSteer(SessionSteerEvent),
    /// A live output-buffer push from a `terminal/create`d command, via
    /// the gateway's `acpx/terminal_output` notification (see
    /// `acpx_core::router::spawn_terminal_output_stream`'s doc comment
    /// on the server side). Carries the *whole current buffer*, not a
    /// byte delta -- a client displaying this is expected to simply
    /// replace its shown contents each time, not append.
    TerminalOutput(TerminalOutputEvent),
    /// A one-shot `acpx/terminal_created` notification (background-
    /// terminals-ui plan, PUI-002b) -- the command/args/start-time a
    /// `terminal/create` request carried, which is otherwise never seen
    /// again (`terminal/output`/`acpx/terminal_output` only ever carry
    /// `{terminalId, output, truncated, exitStatus}`). Fired exactly once
    /// per terminal, right after creation succeeds.
    TerminalCreated(TerminalCreatedEvent),
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
    /// Phase 18: live `usage_update` session/update (used/size tokens)
    /// -- streams DURING a turn so the compose context ring updates
    /// actively, not only at turn end.
    UsageUpdate {
        used: i64,
        size: i64,
    },
    /// PUI-003: the agent's own built-in slash commands, from an ACP
    /// `available_commands_update` session/update (schema
    /// `AvailableCommandsUpdate { available_commands: Vec<AvailableCommand> }`).
    /// Like [`AgentEvent::ConfigOptions`], the notification always carries
    /// the *complete* current command set -- replace, don't append.
    AvailableCommands(Vec<AvailableCommandInfo>),
    /// PROF-11: a live ACP v1 `plan` session/update (schema `Plan {
    /// entries: Vec<PlanEntry> }`) -- an agent's self-reported execution
    /// plan/todo list. Per agentclientprotocol.com, each occurrence is
    /// the *complete* current plan, same "replace, don't append" contract
    /// as [`AgentEvent::ConfigOptions`]/[`AgentEvent::AvailableCommands`]
    /// -- NOT the unstable `plan_update`/`plan_removed` partial-mutation
    /// variants (those live behind ACP's `unstable_plan_operations`
    /// feature, which this crate does not enable; see
    /// `agent-client-protocol-schema`'s `Plan` doc comment: "the client
    /// replaces the entire plan with each update").
    PlanUpdate(Vec<PlanEntryInfo>),
    /// PROF-11: a live ACP v1 `session_info_update` session/update
    /// (schema `SessionInfoUpdate { title?, updatedAt? }`). Both fields
    /// are `MaybeUndefined` on the wire (present-with-value /
    /// present-null / field-absent are three distinct wire states) but
    /// collapsed to a plain `Option<String>` here -- "field absent" and
    /// "explicitly null" both read as `None`, i.e. this can't yet
    /// distinguish "no change" from "agent cleared the title". No real
    /// backend observed doing the latter, so that distinction is
    /// deliberately not carried further than this parse for now.
    SessionInfoUpdate {
        title: Option<String>,
        updated_at: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSteerEvent {
    pub session_id: String,
    pub state: String,
    pub queue_entry_id: Option<String>,
}

/// One entry of a live [`AgentEvent::PlanUpdate`] -- `{content, priority,
/// status}` per ACP v1's `PlanEntry` schema. `priority`/`status` are kept
/// as plain `String`s (not closed enums) round-tripping the wire's
/// snake_case tag verbatim, same "the panel has no independent opinion
/// about what a real backend's own values mean" posture
/// [`crate::protocol_types::AgentStatus`]'s doc comment documents for its
/// own `Unknown(String)` fallback -- an unrecognized future priority/
/// status string still displays as literal text instead of being dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntryInfo {
    pub content: String,
    pub priority: String,
    pub status: String,
}

/// One mode an ACP agent advertises as selectable for a session. See
/// [`AgentEvent::SessionModes`]'s doc comment for the wire origin and
/// why this crate still tracks the older `modes` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// The full `modes` advertisement from a `session/new`/`session/load`/
/// `session/resume` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModesEvent {
    pub current_mode_id: String,
    pub available: Vec<SessionModeInfo>,
}

/// PUI-003: one ACP-agent built-in slash command from an
/// `available_commands_update` (schema `AvailableCommand { name,
/// description, input? }`). Only `name`/`description` are surfaced to the
/// `/` menu today; `input` is accepted-but-ignored so an agent supplying
/// it doesn't drop the whole command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommandInfo {
    pub name: String,
    pub description: String,
}

/// One selectable value inside a `select`-kind [`ConfigOptionInfo::
/// options`] list -- `{value, name, description?}` per
/// agentclientprotocol.com/protocol/session-config-options's documented
/// example response shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// See [`AgentEvent::TerminalCreated`]'s doc comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCreatedEvent {
    pub terminal_id: String,
    pub command: String,
    pub args: Vec<String>,
    /// RFC 3339, as emitted by `acpx_core::router`'s `now_rfc3339()`.
    pub started_at: String,
}

/// A pending interactive decision the UI must render and answer. Carries
/// the *raw* backend-native ACP request verbatim (`raw_request`) so a
/// panel reducer can pull out method-specific detail (permission
/// options + tool-call summary; `fs/*`'s `path`/`content`; `terminal/
/// create`'s `command`/`args`) without needing a bespoke typed field per
/// request kind -- consistent with `gateway_actor::classify_raw_update`'s
/// "operate on the raw JSON shape, don't re-derive a typed ACP schema"
/// convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
/// list`. Re-exported directly from `acpx-client` (not re-typed here)
/// since `panel-rust` already depends on that crate for every other
/// gateway call -- see `acpx_client::mcp`'s own module doc comment for
/// why the full `command`/`args`/`env`/`url`/`headers`/`timeout`/`oauth`
/// shape lives there rather than being narrowed down to the two fields a
/// settings-gear list view happens to render today, which is what this
/// type used to do (and is exactly the "incomplete data" this replaced).
pub use acpx_client::mcp::{
    McpAuthStatus, McpServerConfig, McpServerEntry, McpToolCatalog, McpToolInfo,
    OAuthClientConfig,
};

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
/// id). `website` is the registry's official public landing page, if one
/// is supplied; it is kept separate from any ACPX gateway URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCatalogEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub website: String,
    pub status: AgentStatus,
    // setup-followups plan, agent_settings_ordering_and_install_enable_
    // flow: `agents/list` itself carries no enablement info (that's an
    // admin-plane-only concept, see `AgentBridge::agent_enablement_map`);
    // this is merged in client-side afterward. Defaults `true` (assume
    // enabled) rather than `false` so a panel with no admin token
    // configured at all -- the common case today -- never looks like
    // every agent is silently disabled.
    pub enabled: bool,
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
        let website = value
            .get("website")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
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
            website,
            status,
            enabled: true,
        })
    }
}

#[cfg(test)]
mod raw_payload_bound_tests {
    use super::*;
    use serde_json::json;

    /// Stand-in for a real image-generation tool result: one ~4 MB
    /// base64 string leaf inside an otherwise ordinary content block.
    fn image_tool_output() -> serde_json::Value {
        json!({
            "content": [{
                "type": "image",
                "mimeType": "image/png",
                "data": "A".repeat(4 * 1024 * 1024),
            }],
            "isError": false,
        })
    }

    #[test]
    fn oversized_string_leaf_is_elided_in_place() {
        let mut value = image_tool_output();
        elide_large_payload_strings(&mut value);
        let data = value["content"][0]["data"].as_str().expect("data string");
        assert!(
            data.len() < MAX_RAW_PAYLOAD_STRING_BYTES + 64,
            "elided leaf still {} bytes",
            data.len()
        );
        assert!(data.contains("more bytes elided"));
        // Structure and small sibling fields survive.
        assert_eq!(value["content"][0]["mimeType"], "image/png");
        assert_eq!(value["isError"], false);
    }

    #[test]
    fn display_string_of_an_image_payload_stays_under_the_total_cap() {
        let rendered = bounded_payload_display_string(&image_tool_output());
        assert!(
            rendered.len() <= MAX_RAW_PAYLOAD_TOTAL_BYTES + 64,
            "rendered payload is {} bytes",
            rendered.len()
        );
    }

    #[test]
    fn breadth_heavy_payload_is_capped_even_without_a_long_leaf() {
        let map: serde_json::Map<String, serde_json::Value> = (0..20_000)
            .map(|i| (format!("k{i}"), json!("short")))
            .collect();
        let rendered = bounded_payload_display_string(&serde_json::Value::Object(map));
        assert!(rendered.len() <= MAX_RAW_PAYLOAD_TOTAL_BYTES + 64);
        assert!(rendered.ends_with("more bytes elided>"));
    }

    #[test]
    fn small_payload_is_passed_through_unchanged() {
        let value = json!({"skill": "trailer-writer", "ok": true});
        assert_eq!(bounded_payload_display_string(&value), value.to_string());
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_char() {
        let mut value = serde_json::Value::String("\u{1f600}".repeat(MAX_RAW_PAYLOAD_STRING_BYTES));
        elide_large_payload_strings(&mut value);
        // Round-tripping proves the truncated string is still valid UTF-8
        // (a `String` that split a char boundary could not exist).
        assert!(value.as_str().expect("string").contains("more bytes elided"));
    }
}

#[cfg(test)]
mod parsing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_server_entry_parses_name_and_command() {
        let value = json!({"name": "central-fs", "type": "stdio", "command": "mcp-central-fs"});
        let entry: McpServerEntry = serde_json::from_value(value).expect("entry");
        assert_eq!(entry.name, "central-fs");
        assert_eq!(entry.command(), Some("mcp-central-fs"));
    }

    #[test]
    fn mcp_server_entry_parses_url_only_http_entry() {
        let value = json!({"name": "url-only", "type": "http", "url": "https://example.com/mcp"});
        let entry: McpServerEntry = serde_json::from_value(value).expect("entry");
        assert_eq!(entry.command(), None);
        assert_eq!(entry.url(), Some("https://example.com/mcp"));
    }

    #[test]
    fn mcp_server_entry_is_err_without_a_name() {
        let value = json!({"type": "stdio", "command": "x"});
        assert!(serde_json::from_value::<McpServerEntry>(value).is_err());
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
            "website": "https://example.com/codex",
            "status": "installed"
        });
        let entry = AgentCatalogEntry::from_json(&value).expect("entry");
        assert_eq!(entry.id, "codex-acp");
        assert_eq!(entry.name, "Codex Agent");
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(entry.website, "https://example.com/codex");
        assert_eq!(entry.status, AgentStatus::Installed);
    }

    #[test]
    fn agent_catalog_entry_is_none_without_an_id() {
        assert!(AgentCatalogEntry::from_json(&json!({"name": "x"})).is_none());
    }
}
