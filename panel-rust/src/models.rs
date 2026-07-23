//! Conversions between `rui-acp-client`'s ACP-facing types and the
//! generated Slint `ThreadItem`/`MessageItem` structs, kept apart from
//! `agent_bridge.rs`'s actual ACP/jsonl orchestration logic and from
//! `lib.rs`'s FFI/event-wiring glue (modularity requirement,
//! chat-panel-ui-theme-parity.md). Pure data transforms only -- nothing
//! here touches the Slint runtime beyond the generated struct types
//! themselves, so it's straightforward to unit test without a live
//! `ChatPanel` component.

use crate::agent_bridge::TerminalBuffer;
use crate::markdown::{self, LineKind};
use crate::protocol_types::{ChatMessage, ConfigOptionInfo, MessageKind, SessionModesEvent};
use crate::skills_state::SkillEntry;
use crate::{
    AgentCatalogEntry, DropdownEntry, LocalTerminalItem, MarkdownLine, MarkdownRun,
    McpServerOption, McpToolOption, MessageItem, ProfileOption, SkillOption, TerminalItem,
    ThreadItem,
};
use slint::platform::Key;
use slint::{ModelRc, VecModel};

/// Maps `markdown::LineKind` to tags used by `base/markdown_view.slint`.
fn line_kind_str(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Heading(1) => "h1",
        LineKind::Heading(2) => "h2",
        LineKind::Heading(3) => "h3",
        LineKind::Heading(4) => "h4",
        LineKind::Heading(5) => "h5",
        LineKind::Heading(_) => "h6",
        LineKind::Paragraph => "p",
        LineKind::Code => "code",
        LineKind::Quote => "quote",
        LineKind::ListItem => "li",
        LineKind::OrderedListItem => "li-ordered",
        LineKind::Rule => "hr",
        LineKind::Table => "table",
        LineKind::Blank => "blank",
    }
}

fn lines_to_slint_model(lines: Vec<markdown::Line>) -> ModelRc<MarkdownLine> {
    let rows: Vec<MarkdownLine> = lines
        .into_iter()
        .map(|line| MarkdownLine {
            kind: line_kind_str(line.kind).into(),
            runs: ModelRc::new(VecModel::from(
                line.runs
                    .into_iter()
                    .map(|r| MarkdownRun {
                        text: r.text.into(),
                        bold: r.bold,
                        italic: r.italic,
                        code: r.code,
                        strike: r.strike,
                        link: r.link.into(),
                    })
                    .collect::<Vec<_>>(),
            )),
            indent: line.indent as i32,
            ordinal: line.ordinal as i32,
            code_block_id: line.code_block_id,
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// Agent rows get full markdown parse; other kinds leave lines empty so
/// MarkdownView falls back to plain `text`.
fn markdown_lines_for(kind: &str, text: &str) -> ModelRc<MarkdownLine> {
    if kind != "agent" {
        return ModelRc::new(VecModel::from(Vec::<MarkdownLine>::new()));
    }
    lines_to_slint_model(markdown::render_document(text, markdown::DEFAULT_WRAP_COLS))
}

/// Incremental render for an in-flight agent message.
pub fn streaming_markdown_model(
    renderer: &mut markdown::StreamingMarkdownRenderer,
) -> ModelRc<MarkdownLine> {
    lines_to_slint_model(renderer.render())
}

/// Finalize a completed streamed agent message.
pub fn finished_streaming_markdown_model(
    renderer: &mut markdown::StreamingMarkdownRenderer,
) -> ModelRc<MarkdownLine> {
    lines_to_slint_model(renderer.finish())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    Idle,
    Loading,
    Cancelling,
    Error,
}

impl ThreadState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadState::Idle => "idle",
            ThreadState::Loading => "loading",
            ThreadState::Cancelling => "cancelling",
            ThreadState::Error => "error",
        }
    }
}

fn message_kind_str(
    kind: &MessageKind,
    title: &str,
    raw_input: Option<&serde_json::Value>,
) -> &'static str {
    match kind {
        MessageKind::User => "user",
        MessageKind::Agent => "agent",
        MessageKind::Thinking => "thinking",
        MessageKind::ToolCall => classify_tool_call_kind(title, raw_input),
    }
}

/// chat-items-redesign.md #5/#6 tool-event taxonomy classifier, wired
/// into `message_kind_str` below. `chat_area.slint` must route on the
/// new `"tool_use"`/`"mcp_server_call"` strings (not just
/// `message_card.slint`'s old `item.kind == "tool-call"` check) for this
/// to render correctly -- see that file's own routing change.
///
/// Title-string matching plus an optional `raw_input` JSON probe --
/// `agent-client-protocol`'s own `ToolKind` enum has no MCP/skill
/// variant (confirmed against `agent-client-protocol-schema`'s
/// `tool_call.rs`). MCP detection mirrors Zed's title-string convention
/// (`Run MCP tool \``…). Skill detection uses the Claude-Code lead from
/// chat-items-redesign.md (tool titled `"Skill"` and/or `raw_input`
/// carrying a `"skill"` key) -- still a client-side heuristic, not an
/// ACP-spec guarantee, but confirmed enough to drive first-use tracking.
fn classify_tool_call_kind(title: &str, raw_input: Option<&serde_json::Value>) -> &'static str {
    if title.starts_with("Run MCP tool `") || title.starts_with("mcp__") {
        return "mcp_server_call";
    }
    let has_skill_key = raw_input
        .and_then(|v| v.get("skill"))
        .and_then(|s| s.as_str())
        .is_some();
    let skillish = has_skill_key
        || title.eq_ignore_ascii_case("Skill")
        || title.starts_with("Skill:")
        || title.starts_with("Skill ")
        || title.to_ascii_lowercase().starts_with("skill:");
    if skillish {
        if title.to_ascii_lowercase().contains("load") {
            return "skill_load";
        }
        return "skill_use";
    }
    "tool_use"
}

/// Display / tracking name for a skill tool row -- prefers the
/// `raw_input.skill` string when present, else the title itself.
fn skill_tracking_name(title: &str, raw_input: Option<&serde_json::Value>) -> String {
    raw_input
        .and_then(|v| v.get("skill"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| title.to_string())
}

#[cfg(test)]
mod classify_tool_call_kind_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_title_prefix_classifies_as_mcp_server_call() {
        assert_eq!(
            classify_tool_call_kind("Run MCP tool `search_docs`", None),
            "mcp_server_call"
        );
    }

    #[test]
    fn plain_tool_title_classifies_as_tool_use() {
        assert_eq!(
            classify_tool_call_kind("edit.add_transition(...)", None),
            "tool_use"
        );
        assert_eq!(classify_tool_call_kind("", None), "tool_use");
    }

    #[test]
    fn skill_title_and_raw_input_classify_as_skill_use() {
        assert_eq!(classify_tool_call_kind("Skill", None), "skill_use");
        assert_eq!(
            classify_tool_call_kind("some tool", Some(&json!({"skill": "trailer-writer"}))),
            "skill_use"
        );
    }

    #[test]
    fn skill_load_title_classifies_as_skill_load() {
        assert_eq!(
            classify_tool_call_kind("Skill load trailer-writer", None),
            "skill_load"
        );
    }
}

/// Builds the message-list model shown by `ChatArea`/`MessageCard`.
/// `expanded` is Rust-side, UI-only collapse state for tool-call log
/// bodies (Phase 3), parallel to `msgs` by index -- out-of-range/missing
/// entries default to collapsed (`false`), matching the HTML source's
/// "new tool_use items default to collapsed" convention (see
/// `PanelSingleton::expanded` in `lib.rs` for how the vec is kept in
/// sync as history grows).
pub fn to_message_model(msgs: Vec<ChatMessage>, expanded: &[bool]) -> ModelRc<MessageItem> {
    // First-use skill tracking: walk the list in order, mark a skill_use
    // row first-use only the first time its tracking name appears.
    let mut seen_skills = std::collections::HashSet::<String>::new();
    let items: Vec<MessageItem> = msgs
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let kind = message_kind_str(&m.kind, &m.text, m.raw_input.as_ref());
            let first_use = if kind == "skill_use" {
                let name = skill_tracking_name(&m.text, m.raw_input.as_ref());
                seen_skills.insert(name)
            } else {
                false
            };
            MessageItem {
                kind: kind.into(),
                // Slint side uppercases nothing itself -- source HTML always
                // renders `item.status.toUpperCase()`, so this crate does the
                // same once here rather than duplicating casing logic in
                // `.slint` markup.
                status: m
                    .status
                    .map(|s| s.to_uppercase())
                    .unwrap_or_default()
                    .into(),
                expanded: expanded.get(i).copied().unwrap_or(false),
                index: i as i32,
                raw_input: m
                    .raw_input
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default()
                    .into(),
                raw_output: m
                    .raw_output
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default()
                    .into(),
                text: m.text.clone().into(),
                markdown_lines: markdown_lines_for(kind, &m.text),
                // Send-queue state is not modelled by the raw `ChatMessage`
                // feed -- a message reaching here has already been dispatched.
                queued: false,
                can_edit: false,
                sending: false,
                first_use,
            }
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// Builds the message-list model from the *merged* transcript view
/// (Phase 2 step 3, `AgentBridge::transcript`) rather than the raw
/// per-chunk `ChatMessage` feed -- streamed chunks already merged by
/// message id, tool-call status updates already replacing their row
/// instead of duplicating it (see `crate::conversation::
/// ConversationState`'s own doc comment). This is the function real
/// call sites (`lib.rs::render_messages`) use; [`to_message_model`]
/// above stays available for the raw-feed case and is still covered by
/// its own unit tests, since `ChatMessage`'s shape hasn't changed.
///
/// `Terminal`/`Notice` transcript items are silently skipped -- no
/// production code path constructs either variant yet (`rebuild_from_
/// chat_messages` only ever emits `User`/`Assistant`/`Thought`/`Tool`
/// from a `ChatMessage` feed, which has no terminal/notice kind of its
/// own), so this is a forward-compatible no-op today, not a silent
/// data loss; a future `ConversationEvent::TerminalCreated`/`Notice`
/// producer would need its own dedicated Slint row type anyway, not a
/// `MessageItem` reuse.
pub fn to_message_model_from_transcript(
    items: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
) -> ModelRc<MessageItem> {
    ModelRc::new(VecModel::from(to_message_rows_from_transcript(
        items, expanded,
    )))
}

/// Stable identity for a rendered transcript row. Include the row kind
/// namespace because different reducer row kinds can carry related ids.
pub fn transcript_row_key(item: &crate::conversation::TranscriptItem) -> String {
    use crate::conversation::TranscriptItem;
    match item {
        TranscriptItem::User { message_id, .. } => format!("user:{message_id}"),
        TranscriptItem::Assistant { message_id, .. } => format!("assistant:{message_id}"),
        TranscriptItem::Thought { message_id, .. } => format!("thought:{message_id}"),
        TranscriptItem::Tool { tool_call_id, .. } => format!("tool:{tool_call_id}"),
        TranscriptItem::Terminal { terminal_id, .. } => format!("terminal:{terminal_id}"),
        TranscriptItem::Notice { text } => format!("notice:{text}"),
    }
}

/// Returns stable keys for the rows the Slint message projection renders.
/// Notices stay omitted; terminals are included (wire_terminal_view).
pub fn transcript_row_keys(items: &[crate::conversation::TranscriptItem]) -> Vec<String> {
    use crate::conversation::TranscriptItem;
    items
        .iter()
        .filter(|item| !matches!(item, TranscriptItem::Notice { .. }))
        .map(transcript_row_key)
        .collect()
}

/// Builds concrete message rows for the persistent message `VecModel`.
pub fn to_message_rows_from_transcript(
    items: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
) -> Vec<MessageItem> {
    use crate::conversation::TranscriptItem;

    let mut index = 0i32;
    let mut seen_skills = std::collections::HashSet::<String>::new();
    let rows: Vec<MessageItem> = items
        .into_iter()
        .filter_map(|item| {
            // Live tool details: raw_input/raw_output flow from
            // ChatMessage → TranscriptItem::Tool → MessageItem (UI
            // expand/hide payload). Skill/MCP kind uses raw_input JSON
            // when present.
            let (kind, text, status, raw_input, raw_output): (
                &str,
                String,
                String,
                String,
                String,
            ) = match item {
                TranscriptItem::User { text, .. } => {
                    ("user", text, String::new(), String::new(), String::new())
                }
                TranscriptItem::Assistant { text, .. } => {
                    ("agent", text, String::new(), String::new(), String::new())
                }
                TranscriptItem::Thought { text, .. } => (
                    "thinking",
                    text,
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                TranscriptItem::Tool {
                    title,
                    status,
                    raw_input,
                    raw_output,
                    ..
                } => {
                    let raw_in = raw_input.unwrap_or_default();
                    let raw_out = raw_output.unwrap_or_default();
                    let raw_val = serde_json::from_str(&raw_in).ok();
                    let kind = classify_tool_call_kind(&title, raw_val.as_ref());
                    (
                        kind,
                        title,
                        status.map(|s| s.to_uppercase()).unwrap_or_default(),
                        raw_in,
                        raw_out,
                    )
                }
                // audit-fixes wire_terminal_view: surface terminal
                // transcript items as tool-event-shaped rows so ToolEventRow
                // can mount TerminalView (title = command, output body).
                TranscriptItem::Terminal {
                    title,
                    output,
                    exit_code,
                    ..
                } => (
                    "terminal",
                    title,
                    String::new(),
                    // raw_input carries exit code as decimal text for TerminalView.
                    exit_code.map(|c| c.to_string()).unwrap_or_default(),
                    output,
                ),
                TranscriptItem::Notice { .. } => return None,
            };
            let first_use = if kind == "skill_use" {
                seen_skills.insert(text.clone())
            } else {
                false
            };
            let row = MessageItem {
                kind: kind.into(),
                markdown_lines: markdown_lines_for(kind, &text),
                text: text.into(),
                status: status.into(),
                expanded: expanded.get(index as usize).copied().unwrap_or(false),
                index,
                raw_input: raw_input.into(),
                raw_output: raw_output.into(),
                // Transcript items are always already-dispatched; the send
                // queue lives outside the merged transcript view.
                queued: false,
                can_edit: false,
                sending: false,
                first_use,
            };
            index += 1;
            Some(row)
        })
        .collect();
    rows
}

/// Append per-thread send-queue entries as trailing `queued` user rows
/// (audit-fixes wire_queued_message_bar). Mutates `rows` and returns keys
/// for the appended entries (`queue:{id}`).
///
/// `generation_in_flight`: when true, the front queue entry is marked
/// `sending` so QueuedMessageBar shows Stop (cancel the blocking turn)
/// instead of Cancel on that row.
pub fn append_send_queue_rows(
    rows: &mut Vec<MessageItem>,
    keys: &mut Vec<String>,
    queue: &crate::send_queue::SendQueue,
    generation_in_flight: bool,
) {
    let last = queue.len().saturating_sub(1);
    for (i, entry) in queue.iter().enumerate() {
        let index = rows.len() as i32;
        keys.push(format!("queue:{}", entry.id.0));
        rows.push(MessageItem {
            kind: "user".into(),
            markdown_lines: ModelRc::new(VecModel::from(Vec::<MarkdownLine>::new())),
            text: entry.text.clone().into(),
            status: "".into(),
            expanded: false,
            index,
            raw_input: "".into(),
            raw_output: "".into(),
            queued: true,
            can_edit: i == last && !(generation_in_flight && i == 0),
            // Front entry while a turn is in flight: Stop cancels that turn
            // (and pauses auto-drain). Other entries stay cancel/edit.
            sending: generation_in_flight && i == 0,
            first_use: false,
        });
    }
}

/// Full projection for a thread: transcript + send queue.
pub fn message_rows_for_thread(
    transcript: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
    queue: &crate::send_queue::SendQueue,
) -> (Vec<MessageItem>, Vec<String>) {
    message_rows_for_thread_with_state(transcript, expanded, queue, false)
}

/// Like [`message_rows_for_thread`], but marks the front queue row as
/// `sending` when a turn is currently in flight.
pub fn message_rows_for_thread_with_state(
    transcript: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
    queue: &crate::send_queue::SendQueue,
    generation_in_flight: bool,
) -> (Vec<MessageItem>, Vec<String>) {
    let mut keys = transcript_row_keys(&transcript);
    let mut rows = to_message_rows_from_transcript(transcript, expanded);
    append_send_queue_rows(&mut rows, &mut keys, queue, generation_in_flight);
    // Re-index after append so Slint toggle-expanded still matches.
    for (i, row) in rows.iter_mut().enumerate() {
        row.index = i as i32;
    }
    (rows, keys)
}

// ---------------------------------------------------------------------------
// Compose slash-token helpers (layout-redesign.md Phase 4) -- also installed
// as `TextUtil` callbacks from `lib.rs`.
// ---------------------------------------------------------------------------

fn token_bounds(text: &str, cursor: usize) -> Option<(usize, usize)> {
    if text.is_empty() {
        return None;
    }
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let start = match text[..cursor].rfind(|c: char| c.is_whitespace()) {
        Some(i) => {
            let ch = text[i..].chars().next()?;
            i + ch.len_utf8()
        }
        None => 0,
    };
    let end = text[cursor..]
        .find(|c: char| c.is_whitespace())
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    if start >= end {
        return None;
    }
    Some((start, end))
}

/// Leading trigger char of the whitespace-delimited token at `cursor`
/// when it is `/`, `#`, or `@`; otherwise empty.
pub fn active_token_prefix(text: &str, cursor: i32) -> String {
    let cursor = (cursor.max(0) as usize).min(text.len());
    let Some((start, end)) = token_bounds(text, cursor) else {
        return String::new();
    };
    match text[start..end].chars().next() {
        Some(c @ ('/' | '#' | '@')) => c.to_string(),
        _ => String::new(),
    }
}

/// Token text after the leading `/`/`#`/`@` (may be empty right after the
/// trigger is typed).
pub fn active_token_query(text: &str, cursor: i32) -> String {
    let cursor = (cursor.max(0) as usize).min(text.len());
    let Some((start, end)) = token_bounds(text, cursor) else {
        return String::new();
    };
    let token = &text[start..end];
    match token.chars().next() {
        Some('/' | '#' | '@') => token.chars().skip(1).collect(),
        _ => String::new(),
    }
}

/// Replace the full active token with `replacement` (typically includes a
/// trailing space). When no token is active, appends `replacement`.
pub fn replace_active_token(text: &str, cursor: i32, replacement: &str) -> String {
    let cursor = (cursor.max(0) as usize).min(text.len());
    if let Some((start, end)) = token_bounds(text, cursor) {
        let mut out = String::with_capacity(text.len() + replacement.len());
        out.push_str(&text[..start]);
        out.push_str(replacement);
        out.push_str(&text[end..]);
        out
    } else {
        let mut out = text.to_string();
        out.push_str(replacement);
        out
    }
}

#[cfg(test)]
mod slash_token_tests {
    use super::*;

    #[test]
    fn detects_slash_prefix_and_query() {
        assert_eq!(active_token_prefix("hello /he", 9), "/");
        assert_eq!(active_token_query("hello /he", 9), "he");
        assert_eq!(active_token_prefix("plain", 5), "");
    }

    #[test]
    fn replaces_active_token() {
        assert_eq!(
            replace_active_token("run /he now", 7, "/help "),
            "run /help  now"
        );
    }
}

/// The display name of the thread's currently active mode, for the compose
/// bar's mode-selector trigger label. Empty when no modes are advertised or
/// the current id has no matching entry (the Slint side falls back to a
/// generic label then).
pub fn current_mode_name(modes: &Option<SessionModesEvent>) -> String {
    modes
        .as_ref()
        .and_then(|m| {
            m.available
                .iter()
                .find(|mode| mode.id == m.current_mode_id)
                .map(|mode| mode.name.clone())
        })
        .unwrap_or_default()
}

/// The mode selector's dropdown model -- the thread's `session_modes`
/// advertisement mapped into the domain-neutral `DropdownEntry` the compose
/// bar's `SearchableDropdown` consumes. `None` (no modes advertised, or
/// `session/new` unresolved) yields an empty model, which capability-gates
/// the selector out. `is_current` is resolved against the advertisement's
/// own `current_mode_id`.
pub fn to_mode_dropdown_entries(modes: Option<SessionModesEvent>) -> ModelRc<DropdownEntry> {
    let items: Vec<DropdownEntry> = modes
        .map(|m| {
            let current = m.current_mode_id.clone();
            m.available
                .into_iter()
                .map(|mode| DropdownEntry {
                    is_current: mode.id == current,
                    id: mode.id.into(),
                    label: mode.name.into(),
                    value: String::new().into(),
                    is_header: false,
                })
                .collect()
        })
        .unwrap_or_default();
    ModelRc::new(VecModel::from(items))
}

/// True when this config option is the binary "fast mode" tradeoff that
/// the compose bar surfaces as a dedicated toggle (not a dropdown group).
pub fn is_fast_mode_option_id(id: &str) -> bool {
    matches!(
        id.to_ascii_lowercase().replace('-', "_").as_str(),
        "fastmode" | "fast_mode" | "fast"
    )
}

/// True when this config option is reasoning effort (dedicated compose
/// dropdown, not mixed into the model selector).
pub fn is_reasoning_option_id(id: &str) -> bool {
    matches!(
        id.to_ascii_lowercase().replace('-', "_").as_str(),
        "reasoning"
            | "reasoning_effort"
            | "reasoningeffort"
            | "effort"
            | "think"
            | "thinking"
            | "thinking_level"
    )
}

fn option_id_norm(id: &str) -> String {
    id.to_ascii_lowercase().replace('-', "_")
}

/// Flatten one config option into header + value `DropdownEntry` rows.
fn append_option_entries(items: &mut Vec<DropdownEntry>, option: ConfigOptionInfo) {
    items.push(DropdownEntry {
        id: option.id.clone().into(),
        label: option.name.into(),
        value: String::new().into(),
        is_header: true,
        is_current: false,
    });
    for value in option.options {
        let is_current = option.current_value.as_deref() == Some(value.value.as_str());
        items.push(DropdownEntry {
            is_current,
            id: option.id.clone().into(),
            label: value.name.into(),
            value: value.value.into(),
            is_header: false,
        });
    }
}

fn looks_on_value(value: &str, name: &str) -> bool {
    let v = value.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    matches!(
        v.as_str(),
        "on" | "true" | "1" | "yes" | "enabled" | "fast"
    ) || matches!(n.as_str(), "on" | "true" | "yes" | "enabled" | "fast")
}

fn looks_off_value(value: &str, name: &str) -> bool {
    let v = value.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    matches!(
        v.as_str(),
        "off" | "false" | "0" | "no" | "disabled" | "slow" | "quality"
    ) || matches!(n.as_str(), "off" | "false" | "no" | "disabled" | "slow" | "quality")
}

/// UI projection for the compose-bar Fast toggle. Empty/unavailable when
/// the attached backend does not advertise a fast-mode-shaped option.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FastModeUi {
    pub available: bool,
    pub enabled: bool,
    pub option_id: String,
    pub on_value: String,
    pub off_value: String,
}

/// Extract a binary fast-mode option from ACP `configOptions[]` for the
/// dedicated compose-bar toggle. Prefers common on/off value shapes;
/// with exactly two values falls back to first=off, second=on.
pub fn fast_mode_from_config(options: &[ConfigOptionInfo]) -> FastModeUi {
    let Some(option) = options.iter().find(|o| is_fast_mode_option_id(&o.id)) else {
        return FastModeUi::default();
    };
    if option.options.len() < 2 {
        return FastModeUi::default();
    }
    let on = option
        .options
        .iter()
        .find(|v| looks_on_value(&v.value, &v.name))
        .or_else(|| option.options.get(1));
    let off = option
        .options
        .iter()
        .find(|v| looks_off_value(&v.value, &v.name))
        .or_else(|| option.options.first());
    let (Some(on), Some(off)) = (on, off) else {
        return FastModeUi::default();
    };
    if on.value == off.value {
        return FastModeUi::default();
    }
    let enabled = option
        .current_value
        .as_deref()
        .map(|cur| cur == on.value.as_str() || looks_on_value(cur, cur))
        .unwrap_or(false);
    FastModeUi {
        available: true,
        enabled,
        option_id: option.id.clone(),
        on_value: on.value.clone(),
        off_value: off.value.clone(),
    }
}

/// Model selector rows: **only** the ACP `"model"` option (not reasoning,
/// not fast-mode). When `provider_agent_id` is set and at least one value
/// is namespaced to that agent (`agent/model`, etc.), only those values
/// are kept; if none are namespaced, the full session model list is
/// shown (session ads are already per-agent).
pub fn to_config_dropdown_entries(options: Vec<ConfigOptionInfo>) -> ModelRc<DropdownEntry> {
    to_config_dropdown_entries_for_provider(options, "")
}

pub fn to_config_dropdown_entries_for_provider(
    options: Vec<ConfigOptionInfo>,
    provider_agent_id: &str,
) -> ModelRc<DropdownEntry> {
    let mut items: Vec<DropdownEntry> = Vec::new();
    for option in options {
        if option_id_norm(&option.id) != "model" {
            continue;
        }
        let option = filter_model_option_for_provider(option, provider_agent_id);
        if option.options.is_empty() {
            continue;
        }
        append_option_entries(&mut items, option);
    }
    ModelRc::new(VecModel::from(items))
}

fn filter_model_option_for_provider(
    mut option: ConfigOptionInfo,
    provider_agent_id: &str,
) -> ConfigOptionInfo {
    if provider_agent_id.is_empty() || option.options.is_empty() {
        return option;
    }
    let agent = provider_agent_id.to_ascii_lowercase();
    let any_namespaced = option
        .options
        .iter()
        .any(|v| model_value_looks_namespaced(&v.value));
    if !any_namespaced {
        // Bare model ids (gpt-5, sonnet) — already session-scoped.
        return option;
    }
    let filtered: Vec<_> = option
        .options
        .iter()
        .filter(|v| model_value_matches_provider(&v.value, &v.name, &agent))
        .cloned()
        .collect();
    if filtered.is_empty() {
        return option;
    }
    if let Some(cur) = option.current_value.as_ref() {
        if !filtered.iter().any(|v| &v.value == cur) {
            option.current_value = filtered.first().map(|v| v.value.clone());
        }
    }
    option.options = filtered;
    option
}

fn model_value_looks_namespaced(value: &str) -> bool {
    value.contains('/') || value.contains(':')
}

fn model_value_matches_provider(value: &str, name: &str, agent_lower: &str) -> bool {
    let v = value.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    if v.starts_with(agent_lower) {
        return true;
    }
    if let Some((prefix, _)) = v.split_once('/') {
        if prefix == agent_lower || agent_lower.contains(prefix) || prefix.contains(agent_lower) {
            return true;
        }
    }
    if let Some((prefix, _)) = v.split_once(':') {
        if prefix == agent_lower {
            return true;
        }
    }
    let stem = agent_lower.split('-').next().unwrap_or(agent_lower);
    stem.len() >= 3 && (v.contains(stem) || n.contains(stem))
}

/// Reasoning-effort selector rows (dedicated compose dropdown).
pub fn to_reasoning_dropdown_entries(options: Vec<ConfigOptionInfo>) -> ModelRc<DropdownEntry> {
    let mut items: Vec<DropdownEntry> = Vec::new();
    for option in options {
        if is_reasoning_option_id(&option.id) {
            append_option_entries(&mut items, option);
        }
    }
    ModelRc::new(VecModel::from(items))
}

/// Trigger label for the reasoning dropdown (current value name, or "").
pub fn current_reasoning_trigger_label(options: &[ConfigOptionInfo]) -> String {
    for option in options.iter().filter(|o| is_reasoning_option_id(&o.id)) {
        let Some(cur) = option.current_value.as_ref() else {
            continue;
        };
        if let Some(v) = option.options.iter().find(|v| &v.value == cur) {
            return v.name.clone();
        }
        return cur.clone();
    }
    String::new()
}

/// One-line preview text for a thread's sidebar card, synthesized from
/// its latest message -- matches index.html's static `t.desc` field
/// (Phase 2/3 note: no separate "thread description" concept exists in
/// the data model, so this is derived, not stored). Empty string for a
/// thread with no messages yet. Newlines are flattened to spaces and the
/// result is truncated to `max_chars` with a trailing ellipsis so a long
/// first line can't blow out the fixed-height thread card.
pub fn describe_thread(msgs: &[ChatMessage], max_chars: usize) -> String {
    let Some(last) = msgs.last() else {
        return String::new();
    };
    let flattened: String = last.text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= max_chars {
        flattened
    } else {
        let truncated: String = flattened
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{truncated}\u{2026}") // "…"
    }
}

/// One row of the (possibly filtered) sidebar list, paired with its
/// real index into `names`/`state`/the agent bridge -- callers must
/// carry `real_index` alongside the row so a later Slint-side selection
/// (`thread-selected(filtered_idx)`) can be translated back to the
/// actual thread the bridge/`thread_state` know about. See
/// `PanelSingleton::visible_indices`/`real_index` in `lib.rs`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VisibleThreadItem {
    pub real_index: usize,
    /// Durable panel-local identity used for list reconciliation.
    pub thread_id: String,
    pub item: ThreadItem,
}

/// Builds the sidebar's thread-list items from `names`/`state`
/// (parallel slices, same convention as `PanelSingleton::thread_state`),
/// optionally narrowed by a case-insensitive substring `query` --
/// Phase 2's real client-side search filter. An empty query returns
/// every thread, in original order (no re-sort) -- this deliberately
/// does not reorder by match quality, only filters. Each returned row
/// carries its real index (see `VisibleThreadItem`) since filtering
/// changes the *displayed* position of a thread without changing its
/// identity.
pub fn build_thread_items<N: AsRef<str>>(
    names: &[N],
    state: &[ThreadState],
    descriptions: &[String],
    background_sessions: &[bool],
    closed: &[bool],
    archived: &[bool],
    query: &str,
) -> Vec<VisibleThreadItem> {
    let query_lower = query.trim().to_lowercase();
    names
        .iter()
        .enumerate()
        .zip(state.iter())
        .filter(|((_, name), _)| {
            query_lower.is_empty() || name.as_ref().to_lowercase().contains(&query_lower)
        })
        .map(|((real_index, name), st)| VisibleThreadItem {
            real_index,
            thread_id: format!("thread:{real_index}"),
            item: ThreadItem {
                name: name.as_ref().into(),
                // Archived takes precedence over closed: it is the final,
                // explicitly-chosen state, whereas closed can still precede
                // an archive action on the same thread.
                status: if archived.get(real_index).copied().unwrap_or(false) {
                    "archived"
                } else if closed.get(real_index).copied().unwrap_or(false) {
                    "closed"
                } else {
                    st.as_str()
                }
                .into(),
                busy: matches!(st, ThreadState::Loading),
                open: true,
                background: background_sessions
                    .get(real_index)
                    .copied()
                    .unwrap_or(false),
                description: descriptions
                    .get(real_index)
                    .cloned()
                    .unwrap_or_default()
                    .into(),
                closed: closed.get(real_index).copied().unwrap_or(false),
                archived: archived.get(real_index).copied().unwrap_or(false),
                // Provider/model are not part of the name/state slices this
                // filter operates on -- `lib.rs` post-populates them by
                // `real_index` after filtering, so they default empty here.
                provider: String::new().into(),
                model: String::new().into(),
                // Post-populated by `real_index` in lib.rs, same reason
                // as provider/model above.
                project_path: String::new().into(),
                project_name: String::new().into(),
                profile_name: String::new().into(),
                has_session: false,
            },
        })
        .collect()
}

/// The current value of a thread's `"model"` config option, or "" when the
/// backend advertises no such option (or no current value) -- the sidebar's
/// Phase 8 model label. Reads the same `configOptions[]` feed the compose
/// bar's model selector uses.
pub fn model_name_from_config(options: &[ConfigOptionInfo]) -> String {
    options
        .iter()
        .find(|o| o.id == "model")
        .and_then(|o| o.current_value.clone())
        .unwrap_or_default()
}

/// Display label for the compose-bar model/config trigger — prefers the
/// human-readable option `name` for the current value, falls back to the
/// raw `currentValue`. Empty when nothing is advertised (Slint falls back
/// to a generic "Model" label). Skips fast-mode (compose toggle) and
/// prefers the `"model"` option when present.
pub fn current_config_trigger_label(options: &[ConfigOptionInfo]) -> String {
    let prefer = options
        .iter()
        .find(|o| option_id_norm(&o.id) == "model")
        .into_iter()
        .chain(options.iter().filter(|o| {
            option_id_norm(&o.id) != "model"
                && !is_fast_mode_option_id(&o.id)
                && !is_reasoning_option_id(&o.id)
        }));
    for option in prefer {
        let Some(cur) = option.current_value.as_ref() else {
            continue;
        };
        if let Some(v) = option.options.iter().find(|v| &v.value == cur) {
            return v.name.clone();
        }
        return cur.clone();
    }
    String::new()
}

/// Builds the terminal-card row model for the active thread --
/// `entries` is `(terminal_id, buffer)` pairs in the same first-seen
/// order `AgentBridge::active_terminals` returns, paired with whatever
/// `AgentBridge::terminal_buffer` currently knows for each id (`None`
/// only in the narrow window between the id first appearing in
/// `active_terminals` and its first `TerminalOutput` snapshot landing --
/// rendered as an empty/still-running placeholder rather than skipped,
/// so the card appears the moment the terminal is created, not only
/// once output exists).
pub fn to_terminal_items(entries: Vec<(String, Option<TerminalBuffer>)>) -> ModelRc<TerminalItem> {
    ModelRc::new(VecModel::from(to_terminal_item_rows(entries)))
}

/// Builds concrete terminal rows for a reducer-owned thread snapshot.
pub fn to_terminal_item_rows(entries: Vec<(String, Option<TerminalBuffer>)>) -> Vec<TerminalItem> {
    entries
        .into_iter()
        .map(|(terminal_id, buffer)| match buffer {
            Some(buffer) => TerminalItem {
                terminal_id: terminal_id.into(),
                output: buffer.output.into(),
                truncated: buffer.truncated,
                has_exited: buffer.exit_status.is_some(),
                exit_code: buffer
                    .exit_status
                    .and_then(|(code, _signal)| code)
                    .unwrap_or_default(),
            },
            None => TerminalItem {
                terminal_id: terminal_id.into(),
                output: String::new().into(),
                truncated: false,
                has_exited: false,
                exit_code: 0,
            },
        })
        .collect()
}

/// Builds the settings sheet's profile-picker row model from a real
/// `profiles/list` result (`AgentBridge::list_profiles`).
pub fn to_profile_options(
    profiles: Vec<crate::gateway_actor::ProfileSummary>,
) -> ModelRc<ProfileOption> {
    ModelRc::new(VecModel::from(to_profile_option_rows(profiles)))
}

pub fn to_profile_option_rows(
    profiles: Vec<crate::gateway_actor::ProfileSummary>,
) -> Vec<ProfileOption> {
    profiles
        .into_iter()
        .map(|p| ProfileOption {
            name: p.name.into(),
            agent_id: p.agent_id.into(),
            terminal_enabled: p.allow_terminal_access,
            fs_enabled: p.allow_fs_access,
        })
        .collect()
}

/// Compose-bar **Provider** picker: one row per distinct `agent_id`
/// (provider), not one row per profile name. Selecting a provider still
/// dispatches the representative profile `name` as `id` (so
/// `ProfileSelected` / session open keep working); `value` carries the
/// agent/provider id for model filtering. Label prefers `agent_id`.
/// `current` is the thread's `profile_name` (maps to that profile's agent).
pub fn to_profile_dropdown_entries(
    profiles: &[ProfileOption],
    current: &str,
) -> ModelRc<DropdownEntry> {
    let current_agent = profiles
        .iter()
        .find(|p| !current.is_empty() && p.name.as_str() == current)
        .map(|p| p.agent_id.to_string())
        .unwrap_or_default();

    let mut seen_agents = std::collections::HashSet::<String>::new();
    let mut items: Vec<DropdownEntry> = Vec::new();
    for p in profiles {
        let agent = p.agent_id.to_string();
        let key = if agent.is_empty() {
            p.name.to_string()
        } else {
            agent.clone()
        };
        if !seen_agents.insert(key.clone()) {
            continue;
        }
        let label = if agent.is_empty() {
            p.name.to_string()
        } else {
            agent.clone()
        };
        let is_current = if !current_agent.is_empty() {
            agent == current_agent || (agent.is_empty() && p.name.as_str() == current)
        } else {
            !current.is_empty() && p.name.as_str() == current
        };
        items.push(DropdownEntry {
            is_current,
            id: p.name.clone(),
            label: label.into(),
            value: agent.into(),
            is_header: false,
        });
    }
    ModelRc::new(VecModel::from(items))
}

/// Trigger label for the Provider control: selected provider/agent id,
/// or empty so the UI falls back to `"Provider ›"`.
pub fn current_provider_trigger_label(profiles: &[ProfileOption], current_profile: &str) -> String {
    if current_profile.is_empty() {
        return String::new();
    }
    profiles
        .iter()
        .find(|p| p.name.as_str() == current_profile)
        .map(|p| {
            if p.agent_id.is_empty() {
                p.name.to_string()
            } else {
                p.agent_id.to_string()
            }
        })
        .unwrap_or_else(|| current_profile.to_owned())
}

/// Agent/provider id for the thread's selected profile (empty if unknown).
pub fn provider_agent_id_for_profile(profiles: &[ProfileOption], current_profile: &str) -> String {
    if current_profile.is_empty() {
        return String::new();
    }
    profiles
        .iter()
        .find(|p| p.name.as_str() == current_profile)
        .map(|p| p.agent_id.to_string())
        .unwrap_or_default()
}

/// Builds the settings sheet's MCP-server list row model from a real
/// `mcp_servers/list` result (`AgentBridge::list_mcp_servers`). Each
/// entry is an opaque JSON object on the Rust side (`acpx-core::
/// McpServerStore` never interprets more than `"name"`) -- this only
/// extracts the two fields the list view shows, `"command"` falling
/// back to an empty string for an entry that omits it (still a valid
/// MCP server entry per ACP's own schema, e.g. a URL-based server with
/// no `command` field at all).
pub fn to_mcp_server_options(
    servers: Vec<crate::protocol_types::McpServerEntry>,
) -> ModelRc<McpServerOption> {
    ModelRc::new(VecModel::from(to_mcp_server_option_rows(servers)))
}

pub fn to_mcp_server_option_rows(
    servers: Vec<crate::protocol_types::McpServerEntry>,
) -> Vec<McpServerOption> {
    servers
        .into_iter()
        .map(|entry| {
            let enabled = entry
                .extra
                .get("enabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            let url = entry
                .extra
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            // Prefer explicit transport; fall back to type: remote/http or
            // presence of a url field (opencode remote servers).
            let transport = entry
                .extra
                .get("transport")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| {
                    entry
                        .extra
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|t| match t {
                            "remote" | "http" | "sse" | "streamable_http" => "http".to_owned(),
                            "local" | "stdio" => "stdio".to_owned(),
                            other => other.to_owned(),
                        })
                })
                .unwrap_or_else(|| {
                    if !url.is_empty() {
                        "http".to_owned()
                    } else {
                        String::new()
                    }
                });
            let status = entry
                .extra
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let auth = entry
                .extra
                .get("auth_status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let needs_auth = entry
                .extra
                .get("needs_auth")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| {
                    // Remote HTTP servers that are not yet authenticated.
                    transport == "http" && auth != "authenticated"
                });
            let tools = mcp_tools_from_extra(&entry.extra);
            // Pre-format status subtitle in Rust (audit §4.3) so Slint
            // does not concatenate nested ternaries.
            let mut parts: Vec<&str> = Vec::new();
            if !transport.is_empty() {
                parts.push(transport.as_str());
            }
            if !status.is_empty() {
                parts.push(status.as_str());
            }
            if !auth.is_empty() {
                parts.push(auth.as_str());
            }
            if !enabled {
                parts.push("disabled");
            }
            let status_line = parts.join(" · ");
            McpServerOption {
                name: entry.name.into(),
                command: entry.command.unwrap_or_default().into(),
                status_line: status_line.into(),
                transport: transport.into(),
                url: url.into(),
                enabled,
                status: status.into(),
                needs_auth,
                auth_status: auth.into(),
                tools: ModelRc::new(VecModel::from(tools)),
            }
        })
        .collect()
}

/// Parse a persisted `tools` array from an MCP server registry entry.
fn mcp_tools_from_extra(extra: &serde_json::Value) -> Vec<McpToolOption> {
    let Some(arr) = extra.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_owned();
            let enabled = tool
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let deferred = tool
                .get("deferred")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let token_usage = tool
                .get("token_usage")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            Some(McpToolOption {
                name: name.into(),
                enabled,
                deferred,
                token_usage,
            })
        })
        .collect()
}

/// Builds the skill-manager sidebar/settings row model from discovered
/// `skills_state::SkillEntry` values (both global and project-local
/// scans, already merged/sorted by the caller).
pub fn to_skill_options(entries: Vec<SkillEntry>) -> ModelRc<SkillOption> {
    ModelRc::new(VecModel::from(to_skill_option_rows(entries)))
}

/// Builds concrete skill rows for the persistent skill `VecModel`.
pub fn to_skill_option_rows(entries: Vec<SkillEntry>) -> Vec<SkillOption> {
    entries
        .into_iter()
        .map(|entry| SkillOption {
            name: entry.name.into(),
            description: entry.description.into(),
            scope: entry.scope.as_str().into(),
            path: entry.path.to_string_lossy().into_owned().into(),
            started_from: entry.started_from.unwrap_or_default().into(),
        })
        .collect()
}

/// Builds the recovery/import sheet's row model from a real
/// `AgentBridge::recoverable_sessions` result (Coverage Matrix
/// `session/list` row).
pub fn to_remote_session_options(
    sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    provider: &str,
) -> ModelRc<crate::RemoteSessionOption> {
    ModelRc::new(VecModel::from(to_remote_session_option_rows(
        sessions, provider,
    )))
}

pub fn to_remote_session_option_rows(
    sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    provider: &str,
) -> Vec<crate::RemoteSessionOption> {
    sessions
        .into_iter()
        .map(|session| crate::RemoteSessionOption {
            session_id: session.acp_session_id.into(),
            provider: provider.into(),
            title: session.title.unwrap_or_default().into(),
            updated_at: session.updated_at.unwrap_or_default().into(),
        })
        .collect()
}

/// Builds the settings sheet's agent-catalog row model from a real
/// `agents/list` result (`AgentBridge::list_agents`). `status` is
/// forwarded verbatim as the registry's own snake_case detection tag
/// (see `AgentCatalogEntry`'s doc comment) rather than re-mapped to a
/// UI-specific string -- the panel has no independent opinion about
/// what a real gateway's detection means.
pub fn to_agent_catalog_entries(
    agents: Vec<crate::protocol_types::AgentCatalogEntry>,
) -> ModelRc<AgentCatalogEntry> {
    ModelRc::new(VecModel::from(to_agent_catalog_entry_rows(agents)))
}

/// setup-followups plan, agent_settings_ordering_and_install_enable_flow:
/// detected/usable agents first, least-usable last. `agents/list`'s wire
/// order reflects the registry's own listing order (alphabetical-ish,
/// unrelated to detection), and Slint 1.17.1 has no array-sort primitive
/// of its own -- the settings view's "connected-first" grouping only
/// ever worked when the backend happened to already send rows in that
/// order. This is the real Rust-side sort that was missing. A stable
/// sort (Rust's `Vec::sort_by_key`) so agents sharing a status keep
/// their original registry-relative order, not an arbitrary re-shuffle.
fn agent_status_sort_priority(status: &crate::protocol_types::AgentStatus) -> u8 {
    match status {
        crate::protocol_types::AgentStatus::Installed
        | crate::protocol_types::AgentStatus::InstalledNoSession => 0,
        crate::protocol_types::AgentStatus::RuntimeMissing => 1,
        crate::protocol_types::AgentStatus::NotInstalled => 2,
        crate::protocol_types::AgentStatus::Unknown(_) => 3,
    }
}

pub fn to_agent_catalog_entry_rows(
    mut agents: Vec<crate::protocol_types::AgentCatalogEntry>,
) -> Vec<AgentCatalogEntry> {
    agents.sort_by_key(|entry| agent_status_sort_priority(&entry.status));
    agents
        .into_iter()
        .map(|entry| AgentCatalogEntry {
            id: entry.id.into(),
            name: entry.name.into(),
            version: entry.version.into(),
            status: entry.status.as_wire_str().into(),
            enabled: entry.enabled,
        })
        .collect()
}

/// Builds the `LocalTerminalItem` Slint property from a real
/// `AgentBridge::local_terminal_snapshot` result -- `None` (no terminal
/// open for this thread) becomes the all-default/`open: false` struct,
/// same convention `PendingRequestItem`'s "no `Option<T>` in Slint"
/// doc comment establishes.
pub fn to_local_terminal_item(
    snapshot: Option<crate::agent_bridge::LocalTerminalSnapshot>,
) -> LocalTerminalItem {
    match snapshot {
        Some(s) => LocalTerminalItem {
            open: true,
            screen_text: s.screen_text.into(),
            cols: s.cols as i32,
            rows: s.rows as i32,
            cursor_row: s.cursor_row as i32,
            cursor_col: s.cursor_col as i32,
            has_exited: s.has_exited,
        },
        None => LocalTerminalItem {
            open: false,
            screen_text: String::new().into(),
            cols: 0,
            rows: 0,
            cursor_row: 0,
            cursor_col: 0,
            has_exited: false,
        },
    }
}

/// Translates one Slint `KeyEvent.text` into the raw bytes to write to
/// a client-local PTY's input side -- a real terminal emulator forwards
/// keystrokes as bytes, not as a Rust-level "insert this string"
/// operation. Only one real remapping needed: Slint's `Key::Return`
/// produces `"\n"` as its `text`, but a PTY in the OS's usual line
/// discipline expects Enter as carriage return (`\r`). Slint represents
/// non-printing navigation keys as private-use characters, so map those
/// explicitly to the ANSI byte sequences a real PTY expects instead of
/// writing those private-use codepoints into the shell.
pub fn translate_local_terminal_key(text: &str) -> Vec<u8> {
    match text.chars().collect::<Vec<_>>().as_slice() {
        [ch] if *ch == char::from(Key::Return) => vec![b'\r'],
        [ch] if *ch == char::from(Key::Backspace) => vec![0x7f],
        [ch] if *ch == char::from(Key::Delete) => b"\x1b[3~".to_vec(),
        [ch] if *ch == char::from(Key::Escape) => vec![0x1b],
        [ch] if *ch == char::from(Key::Tab) => vec![b'\t'],
        [ch] if *ch == char::from(Key::LeftArrow) => b"\x1b[D".to_vec(),
        [ch] if *ch == char::from(Key::UpArrow) => b"\x1b[A".to_vec(),
        [ch] if *ch == char::from(Key::RightArrow) => b"\x1b[C".to_vec(),
        [ch] if *ch == char::from(Key::DownArrow) => b"\x1b[B".to_vec(),
        [ch] if *ch == char::from(Key::Home) => b"\x1b[H".to_vec(),
        [ch] if *ch == char::from(Key::End) => b"\x1b[F".to_vec(),
        _ => text.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    const NAMES: &[&str] = &[
        "Fix timeline crash",
        "Add fade transition",
        "Refactor filters",
        "Export pipeline bug",
    ];
    const STATE: &[ThreadState] = &[
        ThreadState::Idle,
        ThreadState::Loading,
        ThreadState::Error,
        ThreadState::Idle,
    ];
    const NO_DESCRIPTIONS: &[String] = &[];
    const BACKGROUND: &[bool] = &[false, true, false, false];
    const NO_CLOSED: &[bool] = &[false, false, false, false];
    const NO_ARCHIVED: &[bool] = &[false, false, false, false];

    #[test]
    fn empty_query_returns_every_thread_in_order() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].item.name, "Fix timeline crash");
        assert_eq!(items[0].real_index, 0);
        assert_eq!(items[3].item.name, "Export pipeline bug");
        assert_eq!(items[3].real_index, 3);
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let items =
            build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "FADE");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
        // Real index must survive filtering -- "Add fade transition" is
        // THREAD_NAMES[1], even though it's now row 0 of the filtered
        // list. This is exactly the mismatch `real_index` exists to fix.
        assert_eq!(items[0].real_index, 1);

        let items =
            build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "fade");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
    }

    #[test]
    fn multiple_matches_preserve_original_order_no_resort() {
        // "x" appears in 2 non-adjacent names (index 0 and 3); must come
        // back in the same relative order as NAMES, not re-sorted, and
        // must skip the non-matching ones in between.
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "x");
        let matched_names: Vec<&str> = items.iter().map(|i| i.item.name.as_str()).collect();
        assert_eq!(
            matched_names,
            vec!["Fix timeline crash", "Export pipeline bug"]
        );
        let real_indices: Vec<usize> = items.iter().map(|i| i.real_index).collect();
        assert_eq!(real_indices, vec![0, 3]);
    }

    #[test]
    fn no_match_returns_empty_not_error() {
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "zzz-no-such-thread",
        );
        assert!(items.is_empty());
    }

    #[test]
    fn whitespace_only_query_behaves_like_empty() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "   ");
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn status_is_carried_through_unfiltered() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "");
        assert_eq!(items[1].item.status, "loading");
        assert_eq!(items[2].item.status, "error");
    }

    #[test]
    fn closed_thread_reports_closed_status_regardless_of_thread_state() {
        // Coverage Matrix `session/close`/`session/delete` row: once a
        // thread is closed, its sidebar row must display "closed", not
        // whatever transient `ThreadState` it was last in -- STATE[1]
        // is `Loading` here, proving the override wins even over that.
        let closed: &[bool] = &[false, true, false, false];
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, closed, NO_ARCHIVED, "");
        assert_eq!(items[1].item.status, "closed");
        assert!(items[1].item.closed);
        assert_eq!(items[0].item.status, "idle");
        assert!(!items[0].item.closed);
    }

    #[test]
    fn archived_thread_reports_archived_status_even_when_also_closed() {
        // setup-followups plan, archive_thread_backend_verify: archived
        // must win over both the transient ThreadState (STATE[1] is
        // Loading) and over closed (also true here), since archiving is
        // the final, explicitly-chosen state a user picks after a thread
        // may already be closed.
        let closed: &[bool] = &[false, true, false, false];
        let archived: &[bool] = &[false, true, false, false];
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, closed, archived, "");
        assert_eq!(items[1].item.status, "archived");
        assert!(items[1].item.archived);
        assert!(items[1].item.closed);
        assert_eq!(items[0].item.status, "idle");
        assert!(!items[0].item.archived);
    }

    #[test]
    fn description_is_carried_through_by_real_index_when_filtered() {
        let descriptions: Vec<String> = vec![
            "Fixed the crash".into(),
            "Added a fade".into(),
            "".into(),
            "Bug still open".into(),
        ];
        let items = build_thread_items(NAMES, STATE, &descriptions, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "fade");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.description, "Added a fade");
    }

    #[test]
    fn description_defaults_to_empty_when_shorter_than_names() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "");
        assert!(items.iter().all(|i| i.item.description.is_empty()));
    }

    #[test]
    fn background_policy_is_preserved_after_filtering() {
        let items =
            build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, BACKGROUND, NO_CLOSED, NO_ARCHIVED, "fade");
        assert!(items[0].item.background);
    }

    fn chat_msg(kind: MessageKind, text: &str, status: Option<&str>) -> ChatMessage {
        ChatMessage {
            kind,
            text: text.to_string(),
            status: status.map(str::to_string),
            id: None,
            raw_input: None,
            raw_output: None,
        }
    }

    #[test]
    fn describe_thread_uses_last_message_flattened_and_truncated() {
        assert_eq!(describe_thread(&[], 40), "");
        let msgs = vec![
            chat_msg(MessageKind::User, "add a crossfade", None),
            chat_msg(MessageKind::Agent, "line one\nline two   with   gaps", None),
        ];
        assert_eq!(describe_thread(&msgs, 40), "line one line two with gaps");

        let long = vec![chat_msg(
            MessageKind::Agent,
            "this response is deliberately much longer than the truncation limit",
            None,
        )];
        let desc = describe_thread(&long, 20);
        assert_eq!(desc.chars().count(), 20);
        assert!(desc.ends_with('\u{2026}'));
    }

    #[test]
    fn to_message_model_uppercases_status_and_defaults_expanded_false() {
        let msgs = vec![
            chat_msg(MessageKind::User, "hi", None),
            chat_msg(
                MessageKind::ToolCall,
                "ffmpeg.export(...)",
                Some("in_progress"),
            ),
        ];
        let model = to_message_model(msgs, &[]);
        assert_eq!(model.row_count(), 2);
        let user_row = model.row_data(0).unwrap();
        assert_eq!(user_row.status, "");
        assert_eq!(user_row.index, 0);
        let tool_row = model.row_data(1).unwrap();
        assert_eq!(tool_row.status, "IN_PROGRESS");
        assert!(!tool_row.expanded);
        assert_eq!(tool_row.index, 1);
    }

    #[test]
    fn to_message_model_honors_provided_expanded_state() {
        let msgs = vec![chat_msg(MessageKind::ToolCall, "x", Some("completed"))];
        let model = to_message_model(msgs, &[true]);
        assert!(model.row_data(0).unwrap().expanded);
    }

    #[test]
    fn to_mcp_server_options_extracts_name_and_command_falling_back_to_empty() {
        let servers = vec![
            crate::protocol_types::McpServerEntry::from_json(&serde_json::json!({
                "name": "central-fs", "command": "mcp-central-fs"
            }))
            .unwrap(),
            // No "command" field at all -- still a valid MCP server
            // entry (e.g. URL-based), must not panic or drop the row.
            crate::protocol_types::McpServerEntry::from_json(&serde_json::json!({
                "name": "url-only"
            }))
            .unwrap(),
        ];
        let model = to_mcp_server_options(servers);
        assert_eq!(model.row_count(), 2);
        let first = model.row_data(0).unwrap();
        assert_eq!(first.name, "central-fs");
        assert_eq!(first.command, "mcp-central-fs");
        let second = model.row_data(1).unwrap();
        assert_eq!(second.name, "url-only");
        assert_eq!(second.command, "");
    }

    #[test]
    fn to_mcp_server_options_parses_tools_url_and_needs_auth() {
        use slint::Model;
        let servers = vec![crate::protocol_types::McpServerEntry::from_json(
            &serde_json::json!({
                "name": "remote-tools",
                "url": "https://example.com/mcp",
                "type": "remote",
                "auth_status": "not authenticated",
                "tools": [
                    { "name": "read", "enabled": true, "deferred": false, "token_usage": 12 },
                    { "name": "write", "enabled": false, "deferred": true }
                ]
            }),
        )
        .unwrap()];
        let model = to_mcp_server_options(servers);
        let row = model.row_data(0).unwrap();
        assert_eq!(row.transport.as_str(), "http");
        assert_eq!(row.url.as_str(), "https://example.com/mcp");
        assert!(row.needs_auth);
        assert_eq!(row.tools.row_count(), 2);
        let t0 = row.tools.row_data(0).unwrap();
        assert_eq!(t0.name.as_str(), "read");
        assert!(t0.enabled);
        assert_eq!(t0.token_usage, 12);
        let t1 = row.tools.row_data(1).unwrap();
        assert_eq!(t1.name.as_str(), "write");
        assert!(!t1.enabled);
        assert!(t1.deferred);
    }

    #[test]
    fn to_agent_catalog_entries_forwards_registry_fields_verbatim() {
        let agents =
            vec![
                crate::protocol_types::AgentCatalogEntry::from_json(&serde_json::json!({
                    "id": "codex-acp",
                    "name": "Codex Agent",
                    "version": "1.0.0",
                    "status": "installed"
                }))
                .unwrap(),
            ];
        let model = to_agent_catalog_entries(agents);
        assert_eq!(model.row_count(), 1);
        let entry = model.row_data(0).unwrap();
        assert_eq!(entry.id, "codex-acp");
        assert_eq!(entry.name, "Codex Agent");
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(entry.status, "installed");
    }

    #[test]
    fn agent_catalog_entries_sort_detected_before_undetected_stably() {
        // setup-followups plan, agent_settings_ordering_and_install_
        // enable_flow: registry wire order is alphabetical-ish, unrelated
        // to detection status -- this proves the Rust-side sort actually
        // reorders to detected-first, and that agents sharing a status
        // keep their original relative order (stable sort), not an
        // arbitrary shuffle.
        fn entry(id: &str, status: &str) -> crate::protocol_types::AgentCatalogEntry {
            crate::protocol_types::AgentCatalogEntry::from_json(&serde_json::json!({
                "id": id,
                "name": id,
                "version": "1.0.0",
                "status": status,
            }))
            .unwrap()
        }
        let agents = vec![
            entry("aardvark-acp", "not_installed"),
            entry("codex-acp", "installed"),
            entry("blocked-acp", "runtime_missing"),
            entry("claude-acp", "installed_no_session"),
            entry("zebra-acp", "not_installed"),
        ];
        let model = to_agent_catalog_entries(agents);
        let ids: Vec<String> = (0..model.row_count())
            .map(|i| model.row_data(i).unwrap().id.to_string())
            .collect();
        assert_eq!(
            ids,
            vec!["codex-acp", "claude-acp", "blocked-acp", "aardvark-acp", "zebra-acp"],
            "expected installed/installed_no_session first, then runtime_missing, then \
             not_installed with original relative order preserved within each group"
        );
    }

    #[test]
    fn to_local_terminal_item_none_becomes_closed_default() {
        let item = to_local_terminal_item(None);
        assert!(!item.open);
        assert_eq!(item.screen_text, "");
        assert!(!item.has_exited);
    }

    #[test]
    fn to_local_terminal_item_some_is_marked_open_with_fields_forwarded() {
        let snapshot = crate::agent_bridge::LocalTerminalSnapshot {
            screen_text: "$ echo hi\nhi".to_string(),
            cols: 80,
            rows: 24,
            cursor_row: 1,
            cursor_col: 2,
            has_exited: false,
        };
        let item = to_local_terminal_item(Some(snapshot));
        assert!(item.open);
        assert_eq!(item.screen_text, "$ echo hi\nhi");
        assert_eq!(item.cols, 80);
        assert_eq!(item.rows, 24);
        assert_eq!(item.cursor_row, 1);
        assert_eq!(item.cursor_col, 2);
    }

    #[test]
    fn translate_local_terminal_key_maps_return_to_carriage_return() {
        assert_eq!(translate_local_terminal_key("\n"), vec![b'\r']);
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::Return).to_string()),
            vec![b'\r']
        );
    }

    #[test]
    fn translate_local_terminal_key_maps_editing_and_navigation_keys_to_pty_bytes() {
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::Backspace).to_string()),
            vec![0x7f]
        );
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::Delete).to_string()),
            b"\x1b[3~"
        );
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::LeftArrow).to_string()),
            b"\x1b[D"
        );
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::RightArrow).to_string()),
            b"\x1b[C"
        );
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::UpArrow).to_string()),
            b"\x1b[A"
        );
        assert_eq!(
            translate_local_terminal_key(&char::from(Key::DownArrow).to_string()),
            b"\x1b[B"
        );
    }

    #[test]
    fn translate_local_terminal_key_forwards_printable_text_verbatim() {
        assert_eq!(translate_local_terminal_key("a"), b"a".to_vec());
        assert_eq!(translate_local_terminal_key("unicode"), b"unicode".to_vec());
    }
}

#[cfg(test)]
mod transcript_model_tests {
    use super::*;
    use crate::conversation::{ConversationEvent, ConversationState};
    use crate::protocol_types::ConfigOptionValue;
    use slint::Model;

    #[test]
    fn to_message_model_from_transcript_preserves_tool_raw() {
        let mut state = ConversationState::new("t1");
        state.apply(ConversationEvent::ToolCall {
            thread_id: "t1".into(),
            tool_call_id: "tc1".into(),
            title: Some("Skill".into()),
            status: Some("completed".into()),
            detail: None,
            raw_input: Some(r#"{"skill":"artifact-design"}"#.into()),
            raw_output: Some(r#"{"ok":true}"#.into()),
        });
        let model = to_message_model_from_transcript(state.items().to_vec(), &[false]);
        let row = model.row_data(0).expect("one row");
        assert_eq!(row.kind.as_str(), "skill_use");
        assert!(row.first_use);
        assert_eq!(row.raw_input.as_str(), r#"{"skill":"artifact-design"}"#);
        assert_eq!(row.raw_output.as_str(), r#"{"ok":true}"#);
    }

    #[test]
    fn transcript_row_keys_are_stable_and_omit_non_message_rows() {
        let items = vec![
            crate::conversation::TranscriptItem::User {
                message_id: "u1".to_owned(),
                text: "hello".to_owned(),
            },
            crate::conversation::TranscriptItem::Assistant {
                message_id: "a1".to_owned(),
                text: "world".to_owned(),
                streaming: true,
            },
            crate::conversation::TranscriptItem::Notice {
                text: "ignored".to_owned(),
            },
        ];

        assert_eq!(
            transcript_row_keys(&items),
            vec!["user:u1".to_owned(), "assistant:a1".to_owned()]
        );
    }

    #[test]
    fn streaming_markdown_matches_one_shot_for_agent() {
        let full = "Hello **world**\n\n- one\n- two\n";
        let one_shot = markdown_lines_for("agent", full);
        let mut renderer = markdown::StreamingMarkdownRenderer::new(markdown::DEFAULT_WRAP_COLS);
        for ch in full.chars() {
            renderer.push(&ch.to_string());
        }
        let finished = finished_streaming_markdown_model(&mut renderer);
        assert_eq!(one_shot.row_count(), finished.row_count());
        assert!(one_shot.row_count() > 0);
    }

    #[test]
    fn non_agent_rows_skip_markdown_parse() {
        assert_eq!(markdown_lines_for("user", "# not parsed").row_count(), 0);
        assert!(markdown_lines_for("agent", "# Title").row_count() > 0);
    }

    #[test]
    fn current_config_trigger_label_prefers_option_display_name() {
        let options = vec![ConfigOptionInfo {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: None,
            kind: "select".into(),
            current_value: Some("gpt-5-mini".into()),
            options: vec![
                ConfigOptionValue {
                    value: "gpt-5".into(),
                    name: "GPT-5".into(),
                    description: None,
                },
                ConfigOptionValue {
                    value: "gpt-5-mini".into(),
                    name: "GPT-5 mini".into(),
                    description: None,
                },
            ],
        }];
        assert_eq!(current_config_trigger_label(&options), "GPT-5 mini");
        assert_eq!(model_name_from_config(&options), "gpt-5-mini");

        let entries = to_config_dropdown_entries(options);
        assert_eq!(entries.row_count(), 3); // header + 2 values
        let cur = entries.row_data(2).expect("mini row");
        assert!(!cur.is_header);
        assert!(cur.is_current);
        assert_eq!(cur.value.as_str(), "gpt-5-mini");
        assert_eq!(cur.id.as_str(), "model");
    }

    #[test]
    fn config_dropdown_entries_omit_fast_mode_which_has_its_own_toggle() {
        let options = vec![
            ConfigOptionInfo {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: None,
                kind: "select".into(),
                current_value: Some("gpt-5".into()),
                options: vec![ConfigOptionValue {
                    value: "gpt-5".into(),
                    name: "GPT-5".into(),
                    description: None,
                }],
            },
            ConfigOptionInfo {
                id: "fastMode".into(),
                name: "Fast Mode".into(),
                description: Some("Trade quality for speed".into()),
                category: None,
                kind: "select".into(),
                current_value: Some("off".into()),
                options: vec![
                    ConfigOptionValue {
                        value: "off".into(),
                        name: "Off".into(),
                        description: None,
                    },
                    ConfigOptionValue {
                        value: "on".into(),
                        name: "On".into(),
                        description: None,
                    },
                ],
            },
        ];

        // Fast mode is a dedicated compose Toggle, not a dropdown group.
        let entries = to_config_dropdown_entries(options.clone());
        assert_eq!(entries.row_count(), 2); // model header + value only
        assert_eq!(entries.row_data(0).unwrap().id.as_str(), "model");
        assert_eq!(entries.row_data(1).unwrap().id.as_str(), "model");

        let fast = fast_mode_from_config(&options);
        assert!(fast.available);
        assert!(!fast.enabled);
        assert_eq!(fast.option_id, "fastMode");
        assert_eq!(fast.on_value, "on");
        assert_eq!(fast.off_value, "off");

        let mut on_opts = options.clone();
        on_opts[1].current_value = Some("on".into());
        assert!(fast_mode_from_config(&on_opts).enabled);
    }

    #[test]
    fn provider_dropdown_dedupes_by_agent_and_model_list_filters_namespaced_values() {
        let profiles = vec![
            ProfileOption {
                name: "work".into(),
                agent_id: "codex-acp".into(),
                terminal_enabled: true,
                fs_enabled: true,
            },
            ProfileOption {
                name: "work-fs".into(),
                agent_id: "codex-acp".into(),
                terminal_enabled: true,
                fs_enabled: true,
            },
            ProfileOption {
                name: "claude-safe".into(),
                agent_id: "claude-acp".into(),
                terminal_enabled: false,
                fs_enabled: false,
            },
        ];
        let entries = to_profile_dropdown_entries(&profiles, "work");
        assert_eq!(entries.row_count(), 2); // one per agent
        assert_eq!(entries.row_data(0).unwrap().label.as_str(), "codex-acp");
        assert_eq!(entries.row_data(0).unwrap().value.as_str(), "codex-acp");
        assert!(entries.row_data(0).unwrap().is_current);
        assert_eq!(entries.row_data(1).unwrap().label.as_str(), "claude-acp");
        assert_eq!(
            current_provider_trigger_label(&profiles, "work"),
            "codex-acp"
        );

        let options = vec![ConfigOptionInfo {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: None,
            kind: "select".into(),
            current_value: Some("codex-acp/gpt-5".into()),
            options: vec![
                ConfigOptionValue {
                    value: "codex-acp/gpt-5".into(),
                    name: "GPT-5".into(),
                    description: None,
                },
                ConfigOptionValue {
                    value: "claude-acp/sonnet".into(),
                    name: "Sonnet".into(),
                    description: None,
                },
            ],
        }];
        let filtered = to_config_dropdown_entries_for_provider(options, "codex-acp");
        // header + one value
        assert_eq!(filtered.row_count(), 2);
        assert_eq!(filtered.row_data(1).unwrap().value.as_str(), "codex-acp/gpt-5");
    }

    #[test]
    fn reasoning_effort_is_split_into_its_own_dropdown_model() {
        let options = vec![
            ConfigOptionInfo {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: None,
                kind: "select".into(),
                current_value: Some("gpt-5".into()),
                options: vec![ConfigOptionValue {
                    value: "gpt-5".into(),
                    name: "GPT-5".into(),
                    description: None,
                }],
            },
            ConfigOptionInfo {
                id: "reasoning".into(),
                name: "Reasoning effort".into(),
                description: None,
                category: None,
                kind: "select".into(),
                current_value: Some("medium".into()),
                options: vec![
                    ConfigOptionValue {
                        value: "low".into(),
                        name: "Low".into(),
                        description: None,
                    },
                    ConfigOptionValue {
                        value: "medium".into(),
                        name: "Medium".into(),
                        description: None,
                    },
                    ConfigOptionValue {
                        value: "high".into(),
                        name: "High".into(),
                        description: None,
                    },
                ],
            },
            ConfigOptionInfo {
                id: "fastMode".into(),
                name: "Fast Mode".into(),
                description: None,
                category: None,
                kind: "select".into(),
                current_value: Some("off".into()),
                options: vec![
                    ConfigOptionValue {
                        value: "off".into(),
                        name: "Off".into(),
                        description: None,
                    },
                    ConfigOptionValue {
                        value: "on".into(),
                        name: "On".into(),
                        description: None,
                    },
                ],
            },
        ];

        let model_entries = to_config_dropdown_entries(options.clone());
        assert_eq!(model_entries.row_count(), 2);
        assert_eq!(model_entries.row_data(0).unwrap().id.as_str(), "model");

        let reasoning = to_reasoning_dropdown_entries(options.clone());
        assert_eq!(reasoning.row_count(), 4); // header + low/medium/high
        assert_eq!(reasoning.row_data(0).unwrap().label.as_str(), "Reasoning effort");
        assert!(reasoning.row_data(2).unwrap().is_current); // medium
        assert_eq!(current_reasoning_trigger_label(&options), "Medium");
        assert_eq!(current_config_trigger_label(&options), "GPT-5");
    }
}
