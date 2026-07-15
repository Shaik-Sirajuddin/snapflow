//! Conversions between `rui-acp-client`'s ACP-facing types and the
//! generated Slint `ThreadItem`/`MessageItem` structs, kept apart from
//! `agent_bridge.rs`'s actual ACP/jsonl orchestration logic and from
//! `lib.rs`'s FFI/event-wiring glue (modularity requirement,
//! chat-panel-ui-theme-parity.md). Pure data transforms only -- nothing
//! here touches the Slint runtime beyond the generated struct types
//! themselves, so it's straightforward to unit test without a live
//! `ChatPanel` component.

use crate::agent_bridge::TerminalBuffer;
use crate::{
    AgentCatalogEntry, ConfigOptionRow, LocalTerminalItem, McpServerOption, MessageItem,
    ModeOption, ProfileOption, TerminalItem, ThreadItem,
};
use crate::protocol_types::{ChatMessage, ConfigOptionInfo, MessageKind, SessionModesEvent};
use slint::{ModelRc, VecModel};

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

fn message_kind_str(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::User => "user",
        MessageKind::Agent => "agent",
        MessageKind::Thinking => "thinking",
        MessageKind::ToolCall => "tool-call",
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
    let items: Vec<MessageItem> = msgs
        .into_iter()
        .enumerate()
        .map(|(i, m)| MessageItem {
            kind: message_kind_str(&m.kind).into(),
            text: m.text.into(),
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
    use crate::conversation::TranscriptItem;

    let mut index = 0i32;
    let rows: Vec<MessageItem> = items
        .into_iter()
        .filter_map(|item| {
            let (kind, text, status) = match item {
                TranscriptItem::User { text, .. } => ("user", text, String::new()),
                TranscriptItem::Assistant { text, .. } => ("agent", text, String::new()),
                TranscriptItem::Thought { text, .. } => ("thinking", text, String::new()),
                TranscriptItem::Tool { title, status, .. } => (
                    "tool-call",
                    title,
                    // Same uppercasing convention `to_message_model`
                    // documents above.
                    status.map(|s| s.to_uppercase()).unwrap_or_default(),
                ),
                TranscriptItem::Terminal { .. } | TranscriptItem::Notice { .. } => return None,
            };
            let row = MessageItem {
                kind: kind.into(),
                text: text.into(),
                status: status.into(),
                expanded: expanded.get(index as usize).copied().unwrap_or(false),
                index,
            };
            index += 1;
            Some(row)
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// Builds the mode-selector's chip row model from a thread's currently
/// advertised `AgentBridge::session_modes` -- `None` (no `modes` field
/// advertised at all, or `session/new` hasn't resolved yet) maps to an
/// empty model, which the Slint side's capability-gating (`available-
/// modes.length > 0`) treats as "hide the selector entirely."
pub fn to_mode_options(modes: Option<SessionModesEvent>) -> ModelRc<ModeOption> {
    let items: Vec<ModeOption> = modes
        .map(|m| {
            m.available
                .into_iter()
                .map(|mode| ModeOption {
                    id: mode.id.into(),
                    name: mode.name.into(),
                    description: mode.description.unwrap_or_default().into(),
                })
                .collect()
        })
        .unwrap_or_default();
    ModelRc::new(VecModel::from(items))
}

/// Builds the config-option selector's flat row model from a thread's
/// currently advertised `AgentBridge::config_options` -- see
/// `ConfigOptionRow`'s doc comment for the header-then-values flattening
/// this performs. An option with no `options[]` entries at all (a
/// `select`-kind option a backend advertised with an empty choice list,
/// or a future non-`select` `kind` this UI doesn't render values for
/// yet) still emits its header row, so its `current_value` remains
/// visible even though nothing is clickable for it.
pub fn to_config_option_rows(options: Vec<ConfigOptionInfo>) -> ModelRc<ConfigOptionRow> {
    let mut items: Vec<ConfigOptionRow> = Vec::new();
    for option in options {
        items.push(ConfigOptionRow {
            option_id: option.id.clone().into(),
            is_header: true,
            name: option.name.into(),
            description: option.description.unwrap_or_default().into(),
            value: String::new().into(),
            is_current: false,
        });
        for value in option.options {
            let is_current = option.current_value.as_deref() == Some(value.value.as_str());
            items.push(ConfigOptionRow {
                option_id: option.id.clone().into(),
                is_header: false,
                name: value.name.into(),
                description: value.description.unwrap_or_default().into(),
                value: value.value.into(),
                is_current,
            });
        }
    }
    ModelRc::new(VecModel::from(items))
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
pub struct VisibleThreadItem {
    pub real_index: usize,
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
            item: ThreadItem {
                name: name.as_ref().into(),
                status: st.as_str().into(),
                busy: matches!(st, ThreadState::Loading),
                open: true,
                description: descriptions
                    .get(real_index)
                    .cloned()
                    .unwrap_or_default()
                    .into(),
            },
        })
        .collect()
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
    let items: Vec<TerminalItem> = entries
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
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// Builds the settings sheet's profile-picker row model from a real
/// `profiles/list` result (`AgentBridge::list_profiles`).
pub fn to_profile_options(profiles: Vec<crate::gateway_actor::ProfileSummary>) -> ModelRc<ProfileOption> {
    let items: Vec<ProfileOption> = profiles
        .into_iter()
        .map(|p| ProfileOption {
            name: p.name.into(),
            agent_id: p.agent_id.into(),
            terminal_enabled: p.allow_terminal_access,
            fs_enabled: p.allow_fs_access,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
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
    let items: Vec<McpServerOption> = servers
        .into_iter()
        .map(|entry| McpServerOption {
            name: entry.name.into(),
            command: entry.command.unwrap_or_default().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
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
    let items: Vec<AgentCatalogEntry> = agents
        .into_iter()
        .map(|entry| AgentCatalogEntry {
            id: entry.id.into(),
            name: entry.name.into(),
            version: entry.version.into(),
            status: entry.status.as_wire_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
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
/// discipline expects Enter as carriage return (`\r`) -- every other
/// character (printable UTF-8, `\x7f` Backspace, `\x1b` Escape, `\t`
/// Tab, Ctrl-key combinations, arrow-key escape sequences Slint already
/// encodes as private-use-area text) is forwarded verbatim.
pub fn translate_local_terminal_key(text: &str) -> Vec<u8> {
    if text == "\n" {
        vec![b'\r']
    } else {
        text.as_bytes().to_vec()
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

    #[test]
    fn empty_query_returns_every_thread_in_order() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].item.name, "Fix timeline crash");
        assert_eq!(items[0].real_index, 0);
        assert_eq!(items[3].item.name, "Export pipeline bug");
        assert_eq!(items[3].real_index, 3);
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "FADE");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
        // Real index must survive filtering -- "Add fade transition" is
        // THREAD_NAMES[1], even though it's now row 0 of the filtered
        // list. This is exactly the mismatch `real_index` exists to fix.
        assert_eq!(items[0].real_index, 1);

        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "fade");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
    }

    #[test]
    fn multiple_matches_preserve_original_order_no_resort() {
        // "x" appears in 2 non-adjacent names (index 0 and 3); must come
        // back in the same relative order as NAMES, not re-sorted, and
        // must skip the non-matching ones in between.
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "x");
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
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "zzz-no-such-thread");
        assert!(items.is_empty());
    }

    #[test]
    fn whitespace_only_query_behaves_like_empty() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "   ");
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn status_is_carried_through_unfiltered() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "");
        assert_eq!(items[1].item.status, "loading");
        assert_eq!(items[2].item.status, "error");
    }

    #[test]
    fn description_is_carried_through_by_real_index_when_filtered() {
        let descriptions: Vec<String> = vec![
            "Fixed the crash".into(),
            "Added a fade".into(),
            "".into(),
            "Bug still open".into(),
        ];
        let items = build_thread_items(NAMES, STATE, &descriptions, "fade");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.description, "Added a fade");
    }

    #[test]
    fn description_defaults_to_empty_when_shorter_than_names() {
        let items = build_thread_items(NAMES, STATE, NO_DESCRIPTIONS, "");
        assert!(items.iter().all(|i| i.item.description.is_empty()));
    }

    fn chat_msg(kind: MessageKind, text: &str, status: Option<&str>) -> ChatMessage {
        ChatMessage {
            kind,
            text: text.to_string(),
            status: status.map(str::to_string),
            id: None,
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
    fn to_agent_catalog_entries_forwards_registry_fields_verbatim() {
        let agents = vec![crate::protocol_types::AgentCatalogEntry::from_json(&serde_json::json!({
            "id": "codex-acp",
            "name": "Codex Agent",
            "version": "1.0.0",
            "status": "installed"
        }))
        .unwrap()];
        let model = to_agent_catalog_entries(agents);
        assert_eq!(model.row_count(), 1);
        let entry = model.row_data(0).unwrap();
        assert_eq!(entry.id, "codex-acp");
        assert_eq!(entry.name, "Codex Agent");
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(entry.status, "installed");
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
    }

    #[test]
    fn translate_local_terminal_key_forwards_other_text_verbatim() {
        assert_eq!(translate_local_terminal_key("a"), b"a".to_vec());
        assert_eq!(translate_local_terminal_key("\u{7f}"), vec![0x7f]);
    }
}
