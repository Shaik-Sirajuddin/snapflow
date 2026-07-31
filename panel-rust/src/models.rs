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
    AgentCatalogEntry, DropdownEntry, LocalTerminalItem, MarkdownBlock, MarkdownLine, MarkdownRun,
    McpServerFormData, McpServerOption, McpToolOption, MessageItem, PlanEntryItem, ProfileOption,
    SkillOption, TerminalItem, ThreadItem,
};
use slint::platform::Key;
use slint::{ModelRc, VecModel};

/// Same taxonomy as `chat_area.slint`'s `is-tool-kind` -- kept in sync by
/// hand since the Slint side can't import a Rust constant list.
fn is_tool_kind(kind: &str) -> bool {
    matches!(
        kind,
        "tool_use" | "mcp_server_call" | "skill_use" | "skill_load" | "terminal"
    )
}

/// Stamps `MessageItem::tool_group_len` on the first row of every
/// contiguous tool-kind run (see that field's doc comment in
/// `types.slint` for why this lives in Rust rather than a Slint
/// `pure function`: no loops/recursion there, so an unbounded scan isn't
/// expressible without a hand-unrolled, arbitrarily-capped `if` ladder).
/// Every other row's `tool_group_len` is left at its literal default (0).
fn assign_tool_group_lengths(rows: &mut [MessageItem]) {
    let mut i = 0;
    while i < rows.len() {
        if !is_tool_kind(rows[i].kind.as_str()) {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && is_tool_kind(rows[i].kind.as_str()) {
            i += 1;
        }
        rows[start].tool_group_len = (i - start) as i32;
    }
}

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
        .map(|line| {
            let plain_text: String = line.runs.iter().map(|r| r.text.as_str()).collect();
            MarkdownLine {
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
                plain_text: plain_text.into(),
            }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// Agent/thinking rows get full markdown parse; other kinds leave lines
/// empty so MarkdownView falls back to plain `text`.
///
/// markdown-render-cache-layer plan, Phase 1/3: reuses `render_index`'s
/// already-rendered `ModelRc<MarkdownLine>` for `key` when `text` hasn't
/// changed since it was last recorded there, instead of the old global,
/// text-content-keyed `MARKDOWN_CACHE` thread_local (retired -- see that
/// plan's "Unification decision"). `render_index` is per-thread
/// (`ThreadModel::markdown_render_index`), so this is naturally bounded
/// by that thread's own message count, not a global cap needing
/// wholesale-clear eviction. This is the same fix for the thread-switch/
/// poll-tick freeze the old cache was for (see memory/acpx/gen/plans/
/// panel-thread-switch-freeze-fix-plan.md): `update.rs`'s snapshot
/// handler calls `message_rows_for_thread_with_state` -> here on *every*
/// poll tick for the selected thread, not just on an actual switch, so
/// an unchanged historical message must not be re-parsed by
/// `pulldown-cmark` and re-wrapped every tick.
fn markdown_lines_for(
    render_index: &mut crate::thread_message_index::ThreadMessageIndex,
    key: &str,
    row_index: usize,
    kind: &str,
    text: &str,
) -> ModelRc<MarkdownLine> {
    if kind != "agent" && kind != "thinking" {
        return ModelRc::new(VecModel::from(Vec::<MarkdownLine>::new()));
    }
    // UI freeze fix: reconnect/status storms (e.g. "Reconnecting... 1/5")
    // and hard transport failures update agent text many times per second.
    // Full markdown reparse of the whole transcript on every poll tick
    // stalls the single UI/paint thread under software GL. These lines
    // are plain status -- MarkdownView already falls back to `text`
    // when `markdown_lines` is empty.
    if agent_text_skips_markdown(text) {
        return ModelRc::new(VecModel::from(Vec::<MarkdownLine>::new()));
    }
    // `row_index` always comes from the caller's own row-construction
    // loop, never inferred from `check()`'s result -- a `RowChange::New`
    // key has no prior row_index to infer, and using a placeholder here
    // would corrupt the index for a genuinely new message.
    let change = render_index.check(key, text);
    if let crate::thread_message_index::RowChange::Unchanged(_) = change {
        if let Some(cached) = render_index.rendered_lines_for(key) {
            crate::trace_host_input(format_args!("markdown cache hit key={key} kind=lines"));
            return cached;
        }
    }
    crate::trace_host_input(format_args!(
        "markdown cache miss key={key} kind=lines reason={}",
        match change {
            crate::thread_message_index::RowChange::New => "new",
            crate::thread_message_index::RowChange::Changed(_) => "changed",
            crate::thread_message_index::RowChange::Unchanged(_) => "uncached",
        }
    ));
    let built = lines_to_slint_model(markdown::render_document(text, markdown::DEFAULT_WRAP_COLS));
    render_index.record(key, row_index, text);
    render_index.set_rendered_lines(key, built.clone());
    built
}

/// True for agent text that is only status/reconnect/hard-error noise --
/// never real markdown content worth a full document parse.
pub(crate) fn agent_text_skips_markdown(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    trimmed.lines().all(|line| {
        let line = line.trim();
        line.is_empty()
            || line.starts_with("Reconnecting...")
            || line.starts_with("unexpected status ")
            || line.contains("502 Bad Gateway")
            || line.contains("Bad Gateway")
    })
}

/// Hard agent/backend failure that should clear `Loading` immediately so
/// the compose/send controls unlock instead of waiting for a late
/// `TurnEnded` after a long reconnect loop.
pub(crate) fn agent_text_is_hard_failure(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("502 bad gateway")
        || lower.contains("unexpected status 5")
        || lower.contains("unexpected status 4")
        || (lower.contains("reconnecting... 5/5")
            && (lower.contains("unexpected status") || lower.contains("bad gateway")))
}

fn empty_markdown_blocks() -> ModelRc<MarkdownBlock> {
    ModelRc::new(VecModel::from(Vec::<MarkdownBlock>::new()))
}

/// Same heading-size ladder `markdown_view.slint`'s `heading-size()`
/// pure function already uses -- kept in lock-step by hand (Slint can't
/// import a Rust constant table) the same way `models.rs`'s `is_tool_kind`
/// doc comment already flags for `chat_area.slint`'s `is-tool-kind`.
/// Setting this on `StyledText.default-font-size` per block (instead of
/// baking it into a shared wrap-column estimate the way `wrap_runs` did)
/// is the fix for the "different text sized line break calc" bug found
/// during the freeze investigation -- see the plan doc's "current
/// implementation has an issue with different text sized line break
/// calc" exchange.
fn heading_font_size(level: Option<u8>) -> f32 {
    match level {
        Some(1) => 18.0,
        Some(2) => 16.0,
        Some(3) => 14.0,
        Some(_) => 13.0,
        None => 0.0, // 0px = inherit MarkdownView's own default for body text
    }
}

/// Plain-data mirror of [`MarkdownBlock`] with a `slint::StyledText`
/// instead of a `ModelRc`-wrapped `[MarkdownBlock]` row -- `ModelRc`
/// wraps a non-atomic `Rc` (never `Send`), but `StyledText` is built on
/// `SharedVector`, which genuinely is `Send + Sync` (spike-confirmed:
/// `slint_styledtext_spike`, `unsafe impl<T: Send + Sync> Send for
/// SharedVector<T>` in `i-slint-core`). This type exists so
/// `build_markdown_block_data` (below) can run on a background thread
/// (`markdown_worker.rs`) -- the final `ModelRc<MarkdownBlock>`
/// wrapping (cheap: no parsing, just moving already-built values into a
/// `VecModel`) has to happen back on the UI thread, in
/// `markdown_blocks_for` or the worker's delivery callback.
///
/// `Debug, PartialEq` (markdown-render-cache-layer plan Phase 2): needed
/// so `Msg::MarkdownBlocksReady` (which carries a `Vec<MarkdownBlockData>`
/// during delivery, before the reducer converts it to one `ModelRc` --
/// see 00-plan.md's "Ownership flow") can derive the same traits every
/// other `Msg` variant does. `slint::StyledText` itself derives
/// `Debug, PartialEq, Clone, Default`, so this doesn't require any
/// hand-written impl.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownBlockData {
    pub kind: &'static str,
    pub text: slint::StyledText,
    pub default_font_size: f32,
    pub indent: i32,
    pub table_cells: Vec<slint::StyledText>,
    pub table_col_count: i32,
    pub code_text: String,
}

/// Segments+styles `text` into [`MarkdownBlockData`] -- the `Send`-safe,
/// `ModelRc`-free half of Architecture v2's per-block rendering. Pure
/// function of `(text, is_streaming_tail)`, no caching, no Slint model
/// types -- safe to call from any thread, including
/// `markdown_worker.rs`'s background render thread. `markdown_blocks_for`
/// (below) wraps this for the synchronous/cached UI-thread call sites;
/// the worker calls it directly and does its own `ModelRc` wrapping in
/// its UI-thread delivery closure.
pub fn build_markdown_block_data(text: &str, is_streaming_tail: bool) -> Vec<MarkdownBlockData> {
    let spans = markdown::segment_blocks(text);
    spans
        .into_iter()
        .map(|span| {
            let styled_text_for = |range: std::ops::Range<usize>| -> slint::StyledText {
                // `segment_blocks`' byte ranges are pulldown-cmark's own
                // verbatim block spans -- they legitimately include a
                // trailing newline (block ranges) or table-cell padding
                // whitespace around `|` delimiters, neither of which is
                // real content (see markdown.rs's segment_blocks tests).
                let raw = text[range].trim();
                let candidate = if is_streaming_tail {
                    markdown::heal_open_markers(raw)
                } else {
                    raw.to_string()
                };
                slint::StyledText::from_markdown(&candidate)
                    .unwrap_or_else(|_| slint::StyledText::from_plain_text(raw))
            };
            match span.kind {
                markdown::BlockSpanKind::Text => MarkdownBlockData {
                    kind: "text",
                    text: styled_text_for(span.source_range),
                    default_font_size: heading_font_size(span.heading_level),
                    indent: span.indent as i32,
                    table_cells: Vec::new(),
                    table_col_count: 0,
                    code_text: String::new(),
                },
                markdown::BlockSpanKind::Code(body) => MarkdownBlockData {
                    kind: "code",
                    text: slint::StyledText::default(),
                    default_font_size: 0.0_f32,
                    indent: span.indent as i32,
                    table_cells: Vec::new(),
                    table_col_count: 0,
                    code_text: body,
                },
                markdown::BlockSpanKind::Rule => MarkdownBlockData {
                    kind: "rule",
                    text: slint::StyledText::default(),
                    default_font_size: 0.0_f32,
                    indent: span.indent as i32,
                    table_cells: Vec::new(),
                    table_col_count: 0,
                    code_text: String::new(),
                },
                markdown::BlockSpanKind::Table { cells, col_count } => {
                    let cell_styled: Vec<slint::StyledText> =
                        cells.into_iter().map(styled_text_for).collect();
                    MarkdownBlockData {
                        kind: "table",
                        text: slint::StyledText::default(),
                        default_font_size: 0.0_f32,
                        indent: span.indent as i32,
                        table_cells: cell_styled,
                        table_col_count: col_count as i32,
                        code_text: String::new(),
                    }
                }
            }
        })
        .collect()
}

/// Wraps `Vec<MarkdownBlockData>` into the `ModelRc<MarkdownBlock>` Slint
/// actually consumes -- purely mechanical (no parsing), so it's cheap
/// enough to run inline on the UI thread even for a worker-delivered
/// chunk (see `markdown_worker.rs`).
pub fn markdown_block_data_to_model(rows: Vec<MarkdownBlockData>) -> ModelRc<MarkdownBlock> {
    let rows: Vec<MarkdownBlock> = rows
        .into_iter()
        .map(|d| MarkdownBlock {
            kind: d.kind.into(),
            text: d.text,
            default_font_size: d.default_font_size,
            indent: d.indent,
            table_cells: ModelRc::new(VecModel::from(d.table_cells)),
            table_col_count: d.table_col_count,
            code_text: d.code_text.into(),
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// Architecture v2 (markdown-thread-freeze-fix phase 3): builds one
/// `MarkdownBlock` per top-level markdown block, each carrying a native
/// `slint::StyledText` (or, for code/rule blocks, plain data routed
/// through the existing `TerminalLogBlock`/rule rendering) instead of a
/// Rust-computed `Vec<MarkdownLine>`. See `markdown::segment_blocks`'s
/// doc comment and the plan doc's "Architecture v2" section for why:
/// `StyledText::from_markdown` parses inline styling *and* wraps at real
/// pixel width itself, so there is nothing left for Rust to precompute
/// beyond block boundaries. `is_streaming_tail` selects whether
/// `markdown::heal_open_markers` runs first (only ever needed on the
/// single actively-streaming tail block -- every closed historical
/// block is already well-formed source text).
/// markdown-render-cache-layer plan, Phase 1/3: mirrors `markdown_lines_for`
/// above -- reuses `render_index`'s already-rendered `ModelRc<MarkdownBlock>`
/// for `key` when `text` is unchanged, instead of the old global,
/// text-content-keyed `MARKDOWN_BLOCK_CACHE` thread_local (retired -- see
/// that plan's "Unification decision"). `row_index` always comes from the
/// caller's row-construction loop, same reasoning as `markdown_lines_for`.
///
/// While `is_streaming_tail` is true, the index is never read or written
/// for this key -- matching the old cache's exact bypass behavior. The
/// tail block's text changes on essentially every call while streaming
/// (so a cache read would never hit anyway), and `heal_open_markers`'
/// healed output must never be mistaken for the final, unhealed-source
/// rendering once the message settles (`is_streaming_tail` flips to
/// `false`) -- the first settled call naturally sees `RowChange::New` or
/// `Changed` (nothing was ever recorded during streaming) and renders
/// fresh.
fn markdown_blocks_for(
    render_index: &mut crate::thread_message_index::ThreadMessageIndex,
    key: &str,
    row_index: usize,
    kind: &str,
    text: &str,
    is_streaming_tail: bool,
) -> ModelRc<MarkdownBlock> {
    if kind != "agent" {
        return empty_markdown_blocks();
    }
    if !is_streaming_tail {
        let change = render_index.check(key, text);
        if let crate::thread_message_index::RowChange::Unchanged(_) = change {
            if let Some(cached) = render_index.rendered_blocks_for(key) {
                crate::trace_host_input(format_args!("markdown cache hit key={key} kind=blocks"));
                return cached;
            }
        }
        // "uncached" (Unchanged but no rendered_blocks yet) is expected
        // exactly once per key -- the render that's about to happen
        // below is what populates it. If this reason keeps recurring
        // for the same key across ticks, that's the
        // record()-wipes-the-other-caller's-payload regression this
        // module's own record_with_matching_hash_does_not_wipe_an_
        // already_rendered_payload test guards against.
        crate::trace_host_input(format_args!(
            "markdown cache miss key={key} kind=blocks reason={}",
            match change {
                crate::thread_message_index::RowChange::New => "new",
                crate::thread_message_index::RowChange::Changed(_) => "changed",
                crate::thread_message_index::RowChange::Unchanged(_) => "uncached",
            }
        ));
    }
    let built = markdown_block_data_to_model(build_markdown_block_data(text, is_streaming_tail));
    if !is_streaming_tail {
        render_index.record(key, row_index, text);
        render_index.set_rendered_blocks(key, built.clone());
    }
    built
}

/// Plan phase 27: markdown render of the skill editor's active content
/// for the editor's Preview toggle -- same renderer/wrap as agent chat
/// bodies, so the "MD formatter" applies to the skill's editing section.
pub fn skill_markdown_preview(text: &str) -> ModelRc<MarkdownLine> {
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
    /// PROF-7 (`profile-only-backend-selection` plan): a real per-thread
    /// state, not a render-time heuristic -- set once, at the moment a
    /// thread's session attach completes (see `external_snapshot`'s
    /// `agent_detected` collection and `update.rs`'s fold of it), when the
    /// thread's bound profile names an agent id that acpx's own
    /// `agents/list` reports as NOT `Installed`/`InstalledNoSession`
    /// (typically a restored thread whose agent is no longer present on
    /// this machine). A thread with no bound profile (native/unmanaged
    /// mode) is never marked Stale -- there is no registry agent id to
    /// check it against, so "can't determine" fails open rather than
    /// guessing.
    Stale,
}

impl ThreadState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadState::Idle => "idle",
            ThreadState::Loading => "loading",
            ThreadState::Cancelling => "cancelling",
            ThreadState::Error => "error",
            ThreadState::Stale => "stale",
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
    // Not production-reachable (see this fn's doc comment: real call
    // sites use `to_message_model_from_transcript`) -- a throwaway,
    // call-local index is fine here, no cross-call cache reuse to prove.
    let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
    let mut items: Vec<MessageItem> = msgs
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
                markdown_lines: markdown_lines_for(&mut render_index, &i.to_string(), i, kind, &m.text),
                markdown_blocks: markdown_blocks_for(
                    &mut render_index,
                    &i.to_string(),
                    i,
                    kind,
                    &m.text,
                    false,
                ),
                // Send-queue state is not modelled by the raw `ChatMessage`
                // feed -- a message reaching here has already been dispatched.
                queued: false,
                can_edit: false,
                can_send_now: false,
                sending: false,
                first_use,
                tool_group_len: 0,
            }
        })
        .collect();
    assign_tool_group_lengths(&mut items);
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
    render_index: &mut crate::thread_message_index::ThreadMessageIndex,
) -> ModelRc<MessageItem> {
    ModelRc::new(VecModel::from(to_message_rows_from_transcript(
        items,
        expanded,
        render_index,
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
///
/// `render_index` is this thread's own `ThreadMessageIndex` (markdown-
/// render-cache-layer plan) -- passed in rather than owned here so the
/// same per-thread cache survives across repeated calls (every poll
/// tick) instead of starting empty each time, which would defeat the
/// whole point of caching by key.
pub fn to_message_rows_from_transcript(
    items: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
    render_index: &mut crate::thread_message_index::ThreadMessageIndex,
) -> Vec<MessageItem> {
    use crate::conversation::TranscriptItem;

    let mut index = 0i32;
    let mut seen_skills = std::collections::HashSet::<String>::new();
    let mut rows: Vec<MessageItem> = items
        .into_iter()
        .filter_map(|item| {
            // Stable key for the render-index lookup below -- computed
            // before `item` is consumed by the `match`. `transcript_row_key`
            // only borrows, so this is fine to compute even for the
            // `Notice` arm, which returns `None` right after without
            // using it.
            let key = transcript_row_key(&item);
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
            let row_index = index as usize;
            let row = MessageItem {
                kind: kind.into(),
                markdown_lines: markdown_lines_for(render_index, &key, row_index, kind, &text),
                // `is_streaming_tail: false` -- not yet wired to the
                // in-flight/generation state tracked elsewhere in this
                // function; every message reaching here today is
                // treated as already-closed source text, which is safe
                // (no healing needed) even though it under-uses
                // `heal_open_markers` for the live-streaming case.
                // Wiring the real last-message-while-generating signal
                // through is deferred to the background-render-worker
                // phase, which already needs to thread that state.
                markdown_blocks: markdown_blocks_for(
                    render_index,
                    &key,
                    row_index,
                    kind,
                    &text,
                    false,
                ),
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
                can_send_now: false,
                sending: false,
                first_use,
                tool_group_len: 0,
            };
            index += 1;
            Some(row)
        })
        .collect();
    assign_tool_group_lengths(&mut rows);
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
            markdown_blocks: empty_markdown_blocks(),
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
            // Any row that isn't already the one actively being drained can
            // jump the queue and send immediately (send_queue.rs's
            // send_now/steer subsystem) -- the front row while generating
            // already shows Stop instead.
            can_send_now: !(generation_in_flight && i == 0),
            first_use: false,
            // Always "user" kind above -- never a tool-group start.
            tool_group_len: 0,
        });
    }
}

/// Full projection for a thread: transcript + send queue.
pub fn message_rows_for_thread(
    transcript: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
    queue: &crate::send_queue::SendQueue,
    render_index: &mut crate::thread_message_index::ThreadMessageIndex,
) -> (Vec<MessageItem>, Vec<String>) {
    message_rows_for_thread_with_state(transcript, expanded, queue, false, render_index)
}

/// Like [`message_rows_for_thread`], but marks the front queue row as
/// `sending` when a turn is currently in flight.
pub fn message_rows_for_thread_with_state(
    transcript: Vec<crate::conversation::TranscriptItem>,
    expanded: &[bool],
    queue: &crate::send_queue::SendQueue,
    generation_in_flight: bool,
    render_index: &mut crate::thread_message_index::ThreadMessageIndex,
) -> (Vec<MessageItem>, Vec<String>) {
    let mut keys = transcript_row_keys(&transcript);
    // Note: the render_index-threading call below is main's addition (new
    // markdown-render pipeline); the transcript-tail `last_is_user` check
    // that main computed here is intentionally NOT kept -- this branch's
    // fix (see the comment below, "Checked against `rows`") already
    // recomputes an equivalent, more-correct `last_is_user` from `rows`
    // after they're built, so keeping both would just shadow/waste the
    // earlier one.
    let mut rows = to_message_rows_from_transcript(transcript, expanded, render_index);
    // Phase 18 (send_feedback_and_empty_states): the instant the user's
    // message is the transcript tail and a generation is in flight,
    // append a synthetic minimal "pending" row (kind "pending") so the
    // chat shows immediate feedback before any real agent event
    // arrives. Rendered as a subtle thinking-style item with a loading
    // animation, deliberately distinct from real "thinking" rows.
    //
    // Checked against `rows` (the actually-displayed, post-filter list),
    // not the raw transcript: `to_message_rows_from_transcript` can drop
    // a trailing item (e.g. `filter_map` skipping an empty in-progress
    // chunk), which let a stale "user is last" read survive even after a
    // real "thinking" row had already landed -- rendering both "Agent is
    // working..." and "Thinking" at once instead of the pending
    // placeholder yielding to the real one, as intended.
    let last_is_user = rows
        .last()
        .map(|row| row.kind == "user")
        .unwrap_or(false);
    if generation_in_flight && last_is_user {
        rows.push(MessageItem {
            kind: "pending".into(),
            ..MessageItem::default()
        });
        keys.push("pending:awaiting-response".to_string());
    }
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

/// True when this config option is the native ACP `permissionMode`
/// (dedicated compose dropdown, not mixed into the model selector) --
/// see `acpx-core::bridge_sessions`'s own `"permissionMode"` `configId`
/// convention (e.g. its `select_adapter_config_option(..., "permissionMode",
/// "acceptEdits")` call), same normalized-id-matching shape as
/// `is_reasoning_option_id`.
pub fn is_permission_mode_option_id(id: &str) -> bool {
    matches!(
        id.to_ascii_lowercase().replace('-', "_").as_str(),
        "permissionmode" | "permission_mode" | "permission"
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
    matches!(v.as_str(), "on" | "true" | "1" | "yes" | "enabled" | "fast")
        || matches!(n.as_str(), "on" | "true" | "yes" | "enabled" | "fast")
}

fn looks_off_value(value: &str, name: &str) -> bool {
    let v = value.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    matches!(
        v.as_str(),
        "off" | "false" | "0" | "no" | "disabled" | "slow" | "quality"
    ) || matches!(
        n.as_str(),
        "off" | "false" | "no" | "disabled" | "slow" | "quality"
    )
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
/// not fast-mode). The catalog is already agent-scoped by ACPX's
/// `models/list` or the session that advertised it; the panel must not guess
/// ownership from model-name strings.
pub fn to_config_dropdown_entries(options: Vec<ConfigOptionInfo>) -> ModelRc<DropdownEntry> {
    let mut items: Vec<DropdownEntry> = Vec::new();
    for option in options {
        if option_id_norm(&option.id) != "model" {
            continue;
        }
        if option.options.is_empty() {
            continue;
        }
        append_option_entries(&mut items, option);
    }
    ModelRc::new(VecModel::from(items))
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

/// Permission-mode selector rows (dedicated compose dropdown) -- same
/// shape as [`to_reasoning_dropdown_entries`], filtering on
/// [`is_permission_mode_option_id`] instead.
pub fn to_permission_mode_dropdown_entries(options: Vec<ConfigOptionInfo>) -> ModelRc<DropdownEntry> {
    let mut items: Vec<DropdownEntry> = Vec::new();
    for option in options {
        if is_permission_mode_option_id(&option.id) {
            append_option_entries(&mut items, option);
        }
    }
    ModelRc::new(VecModel::from(items))
}

/// Trigger label for the permission-mode dropdown (current value name, or
/// ""), same shape as [`current_reasoning_trigger_label`].
pub fn current_permission_mode_trigger_label(options: &[ConfigOptionInfo]) -> String {
    for option in options.iter().filter(|o| is_permission_mode_option_id(&o.id)) {
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
    /// Review-gate fix (phase 32): the bridge-side session binding, when
    /// known -- how the frame poll folds a background-attached session
    /// into `ThreadModel::session_id` (add_thread attaches async now, so
    /// no `SessionAttached` fold ever carries it).
    pub session_id: Option<String>,
    /// PROF-7: whether the thread's bound profile's agent id is
    /// `Installed`/`InstalledNoSession` per a real `agents/list` catalog
    /// read, collected ONLY at the same session-attach-completion
    /// transition `session_id` above is collected for (never every frame
    /// -- `agents/list` is a real RPC, and every other frame this stays
    /// `None`, meaning "no new information this frame", not "not
    /// detected"). `None` also covers native/unmanaged-mode threads (no
    /// bound profile to check) and the profile/catalog lookup failing to
    /// resolve -- both fail open rather than guessing Stale.
    pub agent_detected: Option<bool>,
    pub item: ThreadItem,
}

/// PROF-7: resolves whether `profile_name`'s bound agent id is genuinely
/// present on this machine, from real `profiles/list` + `agents/list`
/// reads (never a guess). Pure so it's directly testable without a real
/// bridge: `None` (fail open, not "not detected") when `profile_name` is
/// empty (native/unmanaged mode has no agent id to check), when no
/// profile with that name is found (a real profiles/list race/lag, not
/// evidence of anything), or when the profile's `agent_id` doesn't appear
/// in the catalog at all (an incomplete/still-loading catalog read, same
/// reasoning). `Some(false)` only when the catalog genuinely reports the
/// bound agent id as something other than `Installed`/`InstalledNoSession`.
pub fn agent_detected_for_profile(
    profiles: &[crate::gateway_actor::ProfileSummary],
    agents: &[crate::protocol_types::AgentCatalogEntry],
    profile_name: &str,
) -> Option<bool> {
    if profile_name.is_empty() {
        return None;
    }
    let agent_id = &profiles.iter().find(|p| p.name == profile_name)?.agent_id;
    let entry = agents.iter().find(|a| &a.id == agent_id)?;
    Some(matches!(
        entry.status,
        crate::protocol_types::AgentStatus::Installed
            | crate::protocol_types::AgentStatus::InstalledNoSession
    ))
}

/// PROF-8 (`profile-only-backend-selection` plan): detects whether an
/// `AgentEvent::Error` message text is acpx-core's
/// `RouterError::BackendRequiresAuthentication` (acpx-core/src/router.rs)
/// -- "the agent is reachable but not authenticated" -- rather than any
/// other session/new or turn failure.
///
/// **This is fragile by design, not by oversight, and the team decided to
/// accept that rather than reach into acpx-core right now.** acpx-server's
/// own transport maps EVERY `RouterError` variant to the same generic
/// JSON-RPC code -32000 (see acpx-server/src/transport/http.rs's
/// `json_rpc_error`) -- there is no distinct code or structured field for
/// "needs auth" to match on instead, so the exact `Display` text of
/// `RouterError::BackendRequiresAuthentication` ("backend requires
/// authentication before session/new") is the only signal that exists.
/// Nothing keeps that string in sync between the two crates -- a future
/// acpx-core wording change breaks this silently, with no compile error
/// anywhere. `agent_bridge.rs`'s
/// `open_session_fails_with_a_detectable_authentication_required_message`
/// test is the tripwire for that: it runs a real acpx-server against a
/// real backend that advertises `authMethods` with no `auth_method_id`
/// configured, and asserts the REAL error text this function is matching
/// against still contains the substring below -- so a wording drift fails
/// that test loudly instead of this detector silently going dark.
///
/// A real acpx-core fix (a distinct error code/field) belongs in the same
/// class as PROF-14's acpx-side fix: deferred, not attempted here, because
/// acpx-core/acpx-server are mid-rewrite in the uncommitted
/// agents-install-runtime worktree and touching them now risks a
/// guaranteed merge conflict for no benefit today.
pub fn is_backend_requires_authentication_error(message: &str) -> bool {
    message.contains("backend requires authentication before session/new")
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
            // Post-populated by real_index in external_snapshot, same as
            // provider/model.
            session_id: None,
            agent_detected: None,
            item: ThreadItem {
                name: name.as_ref().into(),
                relative_time: String::from("now").into(),
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
                project_instance_live: false,
                profile_name: String::new().into(),
                has_session: false,
            },
        })
        .collect()
}

/// Scope the visible thread list to the active project. Empty associations
/// are legacy-only and must not make a thread global: a newly-created project
/// has a real identity before a first save. With no active project, nothing is
/// displayed by the legacy shared-store compatibility path; the chat surface
/// still renders its explicit neutral state after a close signal.
pub fn retain_items_for_project(
    items: &mut Vec<VisibleThreadItem>,
    thread_project_paths: &[String],
    active_project_path: Option<&str>,
) {
    let Some(active) = active_project_path.filter(|path| !path.is_empty()) else {
        return;
    };
    items.retain(|item| {
        let recorded = thread_project_paths
            .get(item.real_index)
            .map(String::as_str)
            .unwrap_or("");
        recorded.is_empty() || recorded == active
    });
}

/// PISO-5 (project-isolation-mlt-binding plan): the project path to show on
/// a thread row's project indicator/chip -- STRICTLY the thread's own
/// recorded association (`ThreadRecord::project_path`, hydrated durably as
/// of PISO-3), never a guess. `""` when the thread has none, meaning the
/// indicator stays dark.
///
/// This retires the former "no recorded path -> show whatever project is
/// ACTIVE right now" fallback. That fallback was added (phase 16/26) back
/// when a restored thread's recorded path was always empty regardless of
/// its real history -- borrowing the active project was the only way to
/// light the indicator at all. PISO-3 fixed the underlying data (restored
/// threads now carry their real recorded path), which makes the fallback
/// actively wrong instead of merely redundant: it would relabel an
/// intentionally-unscoped thread with whatever project a user happens to
/// have open, the exact "guess the association" pattern this whole plan
/// exists to delete (see `retain_items_for_project`'s doc comment -- an
/// empty recorded path already means "visible/neutral everywhere", not
/// "assume the active project").
pub fn display_project_path(recorded: Option<&str>) -> String {
    recorded
        .filter(|path| !path.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// PISO-8 (project-isolation-mlt-binding plan): true only when
/// `thread_project_path` (the same value that gates `display_project_
/// path`'s badge above) is CONFIRMED live right now via snapshotd's
/// `daemon.list`/`daemon.listProjects` -- an agent-launched, possibly
/// headless, instance for a project this panel's own host process never
/// opened -- rather than merely a stale sqlite-recorded association from
/// a session that has long since closed. Mirrors `display_project_path`'s
/// own precedent: derives purely from the thread's OWN recorded value,
/// never a guess, and never lights up for a thread whose project never
/// changed (empty or equal to the active project).
pub fn thread_project_instance_is_live(
    thread_project_path: &str,
    active_project_path: Option<&str>,
    live_daemon_projects: &[crate::agent_bridge::DaemonProjectInstance],
) -> bool {
    if thread_project_path.is_empty() || Some(thread_project_path) == active_project_path {
        return false;
    }
    live_daemon_projects.iter().any(|instance| {
        std::path::Path::new(&instance.project_path) == std::path::Path::new(thread_project_path)
    })
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

pub fn to_terminal_item_rows(entries: Vec<(String, Option<TerminalBuffer>)>) -> Vec<TerminalItem> {
    entries
        .into_iter()
        .map(|(terminal_id, buffer)| match buffer {
            Some(buffer) => {
                let active = buffer.active();
                TerminalItem {
                    terminal_id: terminal_id.into(),
                    output: buffer.output.into(),
                    truncated: buffer.truncated,
                    has_exited: buffer.exit_status.is_some(),
                    exit_code: buffer
                        .exit_status
                        .and_then(|(code, _signal)| code)
                        .unwrap_or_default(),
                    title: buffer.command.clone().into(),
                    last_command: if buffer.args.is_empty() {
                        buffer.command.clone().into()
                    } else {
                        format!("{} {}", buffer.command, buffer.args.join(" ")).into()
                    },
                    started_at: buffer.started_at.into(),
                    active,
                }
            }
            None => TerminalItem {
                terminal_id: terminal_id.into(),
                output: String::new().into(),
                truncated: false,
                has_exited: false,
                exit_code: 0,
                title: String::new().into(),
                last_command: String::new().into(),
                started_at: String::new().into(),
                active: true,
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
///
/// PROF-10 (`profile-only-backend-selection` plan): also filters OUT
/// providers whose agent is not actually live, using the exact same
/// liveness marker PROF-7's `agent_detected_for_profile` reads --
/// `agents` (a real `agents/list` catalog) reporting the profile's
/// `agent_id` as `Installed`/`InstalledNoSession`. Without this, the
/// picker listed every profile regardless of whether its agent was ever
/// installed, letting a user pick a provider session/new can't open.
/// Same fail-open posture as PROF-7 throughout: a profile with no
/// `agent_id` (native/unmanaged mode, nothing to check) or whose
/// `agent_id` isn't in `agents` yet (an incomplete/still-loading catalog
/// read, not evidence the agent is missing) is kept, not hidden -- only a
/// catalog hit that's genuinely NOT `Installed`/`InstalledNoSession`
/// excludes the row.
pub fn to_profile_dropdown_entries(
    profiles: &[ProfileOption],
    agents: &[crate::protocol_types::AgentCatalogEntry],
    current: &str,
) -> ModelRc<DropdownEntry> {
    let current_agent = profiles
        .iter()
        .find(|p| !current.is_empty() && p.name.as_str() == current)
        .map(|p| p.agent_id.to_string())
        .unwrap_or_default();

    let is_live = |agent_id: &str| -> bool {
        if agent_id.is_empty() {
            return true;
        }
        agents
            .iter()
            .find(|a| a.id == agent_id)
            .is_none_or(|entry| {
                matches!(
                    entry.status,
                    crate::protocol_types::AgentStatus::Installed
                        | crate::protocol_types::AgentStatus::InstalledNoSession
                )
            })
    };

    let mut seen_agents = std::collections::HashSet::<String>::new();
    let mut items: Vec<DropdownEntry> = Vec::new();
    for p in profiles {
        let agent = p.agent_id.to_string();
        if !is_live(&agent) {
            continue;
        }
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
    items.push(DropdownEntry {
        is_current: false,
        id: "__new_provider__".into(),
        label: "+ New provider".into(),
        value: "".into(),
        is_header: false,
    });
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
/// `mcp_servers/list` result (`AgentBridge::list_mcp_servers`), now typed
/// end to end (`crate::protocol_types::McpServerEntry`, re-exported from
/// `acpx_client::mcp`) -- `transport`/`command`/`url`/`needs_auth`/
/// `auth_status` below are read from real struct fields, not guessed out
/// of an opaque JSON blob's inconsistently-named keys the way this used
/// to work.
pub fn to_mcp_server_options(
    servers: Vec<crate::protocol_types::McpServerEntry>,
) -> ModelRc<McpServerOption> {
    ModelRc::new(VecModel::from(to_mcp_server_option_rows(servers, &[])))
}

/// `busy_keys` is `AgentBridge::mcp_operations_in_flight`'s raw output
/// (`"<action>:<server-name>"` per in-flight RPC, see that method's doc
/// comment) -- folded here into each row's `remove-busy`/`enabled-busy`/
/// `authenticate-busy`/`logout-busy` booleans so the Spinner in
/// `mcp_servers_view.slint` shows precisely on the button whose action is
/// actually in flight for *that* server, not a global spinner. Tools-fetch
/// deliberately reads `tool_fetch_status` instead (see `AgentBridge::
/// fetch_mcp_server_tools_async`'s doc comment for why).
pub fn to_mcp_server_option_rows(
    servers: Vec<crate::protocol_types::McpServerEntry>,
    busy_keys: &[String],
) -> Vec<McpServerOption> {
    use crate::protocol_types::{McpAuthStatus, McpServerConfig};

    let is_busy = |action: &str, name: &str| {
        busy_keys
            .iter()
            .any(|key| key == &format!("{action}:{name}"))
    };

    servers
        .into_iter()
        .map(|entry| {
            let enabled = entry.enabled;
            let transport = entry.config.transport_name().to_owned();
            let command = entry.command().unwrap_or("").to_owned();
            let url = entry.url().unwrap_or("").to_owned();
            let needs_auth = entry.needs_auth();
            let auth = match entry.auth_status {
                Some(McpAuthStatus::Authenticated) => "authenticated",
                Some(McpAuthStatus::Unauthenticated) => "unauthenticated",
                None => "",
            }
            .to_owned();
            let (args, env, headers, timeout, oauth_client_id) = match &entry.config {
                McpServerConfig::Stdio {
                    args, env, timeout, ..
                } => (
                    args.join(" "),
                    format_kv_lines(env, "="),
                    String::new(),
                    timeout.map(|t| t.to_string()).unwrap_or_default(),
                    String::new(),
                ),
                McpServerConfig::Http {
                    headers,
                    timeout,
                    oauth,
                    ..
                } => (
                    String::new(),
                    String::new(),
                    format_kv_lines(headers, ": "),
                    timeout.map(|t| t.to_string()).unwrap_or_default(),
                    oauth
                        .as_ref()
                        .map(|o| o.client_id.clone())
                        .unwrap_or_default(),
                ),
            };
            // Connection status for StatusDot: prefer a real probe value
            // from `extra["status"]` when the gateway supplies one; else
            // derive from enable/auth so the enable toggle visibly
            // rewires the UI (disabled → red "disconnected", auth-needed
            // → yellow, otherwise green "connected"). Previously this
            // only read `extra`, which is almost always empty today.
            let status = entry
                .extra
                .get("status")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if !enabled {
                        "disconnected".to_owned()
                    } else if needs_auth {
                        "auth required".to_owned()
                    } else {
                        "connected".to_owned()
                    }
                });
            let tools = mcp_tools_from_entry(&entry);
            // Kickoff RPC marks server-side toolCatalog Fetching before
            // returning; until the next list poll lands, also treat an
            // in-flight `tools_fetch:<name>` key as fetching so the
            // Fetch button spinner is never stuck waiting on the poll.
            let (tool_fetch_status, tool_fetch_error) = match &entry.tool_catalog {
                None if is_busy("tools_fetch", &entry.name) => {
                    ("fetching".to_string(), String::new())
                }
                None => (String::new(), String::new()),
                Some(crate::protocol_types::McpToolCatalog::Fetching) => {
                    ("fetching".to_string(), String::new())
                }
                Some(crate::protocol_types::McpToolCatalog::Ready { .. }) => {
                    ("ready".to_string(), String::new())
                }
                Some(crate::protocol_types::McpToolCatalog::Error { message }) => {
                    ("error".to_string(), message.clone())
                }
            };
            // Pre-format status subtitle in Rust (audit §4.3) so Slint
            // does not concatenate nested ternaries.
            let mut parts: Vec<&str> = Vec::new();
            if !transport.is_empty() {
                parts.push(transport.as_str());
            }
            if !status.is_empty() {
                parts.push(status.as_str());
            }
            if !auth.is_empty() && auth != "unauthenticated" {
                parts.push(auth.as_str());
            }
            if !enabled {
                parts.push("disabled");
            }
            let status_line = parts.join(" · ");
            // Lets the page search bar find a server by one of its real
            // discovered tool names/descriptions, same reasoning as
            // widening the predicate to args/env/headers earlier.
            let tools_search_blob = tools
                .iter()
                .flat_map(|t| [t.name.as_str(), t.description.as_str()])
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            let remove_busy = is_busy("delete", &entry.name);
            let enabled_busy = is_busy("enabled", &entry.name);
            let authenticate_busy = is_busy("authenticate", &entry.name);
            let logout_busy = is_busy("logout", &entry.name);
            McpServerOption {
                name: entry.name.into(),
                command: command.into(),
                status_line: status_line.into(),
                transport: transport.into(),
                url: url.into(),
                enabled,
                status: status.into(),
                needs_auth,
                auth_status: auth.into(),
                tools: ModelRc::new(VecModel::from(tools)),
                tool_fetch_status: tool_fetch_status.into(),
                tool_fetch_error: tool_fetch_error.into(),
                tools_search_blob: tools_search_blob.into(),
                // Every acpx `mcp_servers/list` row is a user-added registry
                // entry -- removable. The one non-removable row (the built-in
                // snapshotd daemon) is prepended separately, see
                // [`builtin_snapshotd_option`].
                removable: true,
                remove_busy,
                enabled_busy,
                authenticate_busy,
                logout_busy,
                args: args.into(),
                env: env.into(),
                headers: headers.into(),
                timeout: timeout.into(),
                oauth_client_id: oauth_client_id.into(),
            }
        })
        .collect()
}

/// Formats a `HashMap<String, String>` (env vars or HTTP headers) as
/// `key<sep>value` lines, one per entry, sorted by key for deterministic
/// output (a `HashMap`'s iteration order is otherwise unspecified, which
/// would make the form's textarea re-shuffle lines on every reload).
fn format_kv_lines(map: &std::collections::HashMap<String, String>, sep: &str) -> String {
    let mut pairs: Vec<(&String, &String)> = map.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}{sep}{v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses `format_kv_lines`' inverse: one `key<sep>value` pair per
/// non-empty line, splitting on the *first* `sep` only (so an HTTP
/// header's value may itself contain `:` -- `Authorization: Bearer a:b`
/// stays intact). Blank lines and lines with an empty key are silently
/// skipped rather than erroring -- this is user-typed free text in a
/// settings form, not a wire payload with a validation contract to
/// enforce.
fn parse_kv_lines(text: &str, sep: char) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (key, value) = line.split_once(sep)?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Builds a full typed [`crate::protocol_types::McpServerEntry`] from the
/// add/edit form's submitted [`McpServerFormData`] -- the Rust-side
/// counterpart to `mcp_servers_view.slint`'s `mcp-server-submit`
/// callback. `args` is whitespace-split (`str::split_whitespace`, no
/// shell-style quoting/escaping); `timeout` is parsed as whole seconds,
/// `None` on empty/invalid input rather than erroring, since 0 is a
/// meaningless timeout and the field is optional.
pub fn mcp_server_entry_from_form(data: &McpServerFormData) -> crate::protocol_types::McpServerEntry {
    use crate::protocol_types::{McpServerConfig, McpServerEntry, OAuthClientConfig};

    let timeout = data.timeout.trim().parse::<u64>().ok();
    let config = if data.transport.as_str() == "http" {
        let client_id = data.oauth_client_id.trim();
        McpServerConfig::Http {
            url: data.url.trim().to_string(),
            headers: parse_kv_lines(&data.headers, ':'),
            timeout,
            oauth: if client_id.is_empty() {
                None
            } else {
                Some(OAuthClientConfig {
                    client_id: client_id.to_string(),
                })
            },
        }
    } else {
        McpServerConfig::Stdio {
            command: data.command.trim().to_string(),
            args: data
                .args
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            env: parse_kv_lines(&data.env, '='),
            timeout,
        }
    };
    McpServerEntry::new(data.name.trim(), config)
}

/// PUI-015: the built-in `snapflow` daemon MCP row for the Settings list,
/// or `None` when the watcher has no current authoritative MCP status. This
/// is the same endpoint the panel injects into sessions, surfaced here as a
/// first-class, non-removable row so the always-on daemon server the model
/// really talks to is visible in Settings, instead of the list showing only
/// user-added registry servers and hiding the built-in one entirely. Not a
/// synthetic UI guess: it names the exact `http://<addr>/mcp` endpoint the
/// injection uses (`agent_bridge::snapshotd_mcp_addr`).
pub fn builtin_snapshotd_option(addr: Option<String>) -> Option<McpServerOption> {
    let addr = addr?;
    // Live injection gate (Settings toggle) — not always-on once the
    // user has disabled snapflow; still show the row so they can re-enable.
    let enabled = crate::agent_bridge::snapflow_mcp_enabled();
    let status = if enabled {
        "connected"
    } else {
        "disconnected"
    };
    let status_line = if enabled {
        "built-in daemon · injected into sessions"
    } else {
        "built-in daemon · disabled (not in live sessions)"
    };
    Some(McpServerOption {
        name: "snapflow".into(),
        command: String::new().into(),
        status_line: status_line.into(),
        transport: "http".into(),
        url: format!("http://{addr}/mcp").into(),
        enabled,
        status: status.into(),
        needs_auth: false,
        auth_status: String::new().into(),
        tools: ModelRc::new(VecModel::from(Vec::<McpToolOption>::new())),
        // The built-in daemon isn't a registry entry at all -- there's no
        // `mcp_servers/tools_fetch` target for it, so it never has a
        // fetch status to show.
        tool_fetch_status: String::new().into(),
        tool_fetch_error: String::new().into(),
        tools_search_blob: String::new().into(),
        removable: false,
        // Not a registry entry -- none of these actions have a target to
        // dispatch against for this row, so never busy.
        remove_busy: false,
        enabled_busy: false,
        authenticate_busy: false,
        logout_busy: false,
        args: String::new().into(),
        env: String::new().into(),
        headers: String::new().into(),
        timeout: String::new().into(),
        oauth_client_id: String::new().into(),
    })
}

/// Reconciles a server's live-fetched tool catalog (`entry.tool_catalog`,
/// populated by the real `mcp_servers/tools_fetch` background probe --
/// see `crate::protocol_types::McpToolCatalog`'s doc comment) with its
/// durable per-tool preferences (`entry.extra["tools"]`, written by
/// `dispatch_mcp_server_tool_enabled_changed`/`dispatch_mcp_server_tool_
/// deferred_changed`) into one row list for the settings UI.
///
/// A tool present in the live catalog with no persisted preference yet
/// defaults to `enabled: true, deferred: false` (same default a freshly
/// discovered ACP capability gets). A tool with a persisted preference
/// but currently absent from the live catalog (never fetched yet, or the
/// server just doesn't currently advertise it) still shows up, carrying
/// its last-known preference -- toggling something once must never
/// silently vanish just because a later fetch didn't happen to include
/// it.
fn mcp_tools_from_entry(entry: &crate::protocol_types::McpServerEntry) -> Vec<McpToolOption> {
    use std::collections::{HashMap, HashSet};

    let mut preferences: HashMap<String, (bool, bool, i32)> = HashMap::new();
    if let Some(arr) = entry.extra.get("tools").and_then(|v| v.as_array()) {
        for tool in arr {
            let Some(name) = tool.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let enabled = tool.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let deferred = tool.get("deferred").and_then(|v| v.as_bool()).unwrap_or(false);
            let token_usage = tool.get("token_usage").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            preferences.insert(name.to_string(), (enabled, deferred, token_usage));
        }
    }

    let mut rows = Vec::new();
    let mut seen = HashSet::new();

    if let Some(crate::protocol_types::McpToolCatalog::Ready { tools }) = &entry.tool_catalog {
        for tool in tools {
            let (enabled, deferred, token_usage) =
                preferences.get(&tool.name).copied().unwrap_or((true, false, 0));
            seen.insert(tool.name.clone());
            rows.push(McpToolOption {
                name: tool.name.clone().into(),
                description: tool.description.clone().unwrap_or_default().into(),
                enabled,
                deferred,
                token_usage,
            });
        }
    }

    let mut leftover: Vec<(String, (bool, bool, i32))> = preferences
        .into_iter()
        .filter(|(name, _)| !seen.contains(name))
        .collect();
    leftover.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, (enabled, deferred, token_usage)) in leftover {
        rows.push(McpToolOption {
            name: name.into(),
            description: String::new().into(),
            enabled,
            deferred,
            token_usage,
        });
    }

    rows
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
            is_dev_only: entry.is_dev_only,
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
    ModelRc::new(VecModel::from(to_agent_catalog_entry_rows(agents, &[])))
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
    loading_ids: &[String],
) -> Vec<AgentCatalogEntry> {
    agents.sort_by_key(|entry| agent_status_sort_priority(&entry.status));
    agents
        .into_iter()
        .map(|entry| {
            let id = entry.id.clone();
            AgentCatalogEntry {
                id: id.clone().into(),
                name: entry.name.into(),
                version: entry.version.into(),
                status: entry.status.as_wire_str().into(),
                enabled: entry.enabled,
                loading: loading_ids.iter().any(|loading_id| loading_id == &id),
            }
        })
        .collect()
}

/// PROF-11: builds the compose-header plan panel's row model from a real
/// `plan` session/update (`AgentBridge::plan`). Order is preserved
/// verbatim -- ACP's `Plan.entries` is already the agent's own intended
/// display order, not something this layer should re-sort.
pub fn to_plan_entry_rows(
    entries: Vec<crate::protocol_types::PlanEntryInfo>,
) -> Vec<PlanEntryItem> {
    entries
        .into_iter()
        .map(|entry| PlanEntryItem {
            content: entry.content.into(),
            priority: entry.priority.into(),
            status: entry.status.into(),
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

    // PROF-7: agent_detected_for_profile is the pure decision function the
    // real per-thread Stale state is built on -- exercised directly here so
    // its fail-open cases (empty profile, unknown profile, agent id absent
    // from the catalog) don't depend on a real bridge/gateway to prove.
    fn catalog_entry(
        id: &str,
        status: crate::protocol_types::AgentStatus,
    ) -> crate::protocol_types::AgentCatalogEntry {
        crate::protocol_types::AgentCatalogEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            version: String::new(),
            status,
            enabled: true,
        }
    }

    fn profile_summary(name: &str, agent_id: &str) -> crate::gateway_actor::ProfileSummary {
        crate::gateway_actor::ProfileSummary {
            name: name.to_owned(),
            agent_id: agent_id.to_owned(),
            allow_terminal_access: false,
            allow_fs_access: false,
        }
    }

    #[test]
    fn agent_detected_for_profile_true_when_catalog_says_installed() {
        let profiles = [profile_summary("codex-profile", "codex-acp")];
        let agents = [catalog_entry(
            "codex-acp",
            crate::protocol_types::AgentStatus::Installed,
        )];
        assert_eq!(
            agent_detected_for_profile(&profiles, &agents, "codex-profile"),
            Some(true)
        );
    }

    #[test]
    fn agent_detected_for_profile_false_when_catalog_says_not_installed() {
        let profiles = [profile_summary("codex-profile", "codex-acp")];
        let agents = [catalog_entry(
            "codex-acp",
            crate::protocol_types::AgentStatus::NotInstalled,
        )];
        assert_eq!(
            agent_detected_for_profile(&profiles, &agents, "codex-profile"),
            Some(false),
            "a real registry hit that isn't Installed/InstalledNoSession must read as not \
             detected"
        );
    }

    #[test]
    fn agent_detected_for_profile_installed_no_session_still_counts_as_detected() {
        let profiles = [profile_summary("codex-profile", "codex-acp")];
        let agents = [catalog_entry(
            "codex-acp",
            crate::protocol_types::AgentStatus::InstalledNoSession,
        )];
        assert_eq!(
            agent_detected_for_profile(&profiles, &agents, "codex-profile"),
            Some(true)
        );
    }

    #[test]
    fn agent_detected_for_profile_fails_open_on_empty_profile_name() {
        // Native/unmanaged mode: no bound profile, so no agent id to check
        // against the registry at all -- must never guess Stale.
        assert_eq!(agent_detected_for_profile(&[], &[], ""), None);
    }

    #[test]
    fn agent_detected_for_profile_fails_open_when_profile_name_not_found() {
        let profiles = [profile_summary("other-profile", "codex-acp")];
        let agents = [catalog_entry(
            "codex-acp",
            crate::protocol_types::AgentStatus::NotInstalled,
        )];
        assert_eq!(
            agent_detected_for_profile(&profiles, &agents, "missing-profile"),
            None,
            "a profiles/list that hasn't caught up yet must fail open, not read as not detected"
        );
    }

    #[test]
    fn agent_detected_for_profile_fails_open_when_agent_id_absent_from_catalog() {
        let profiles = [profile_summary("codex-profile", "codex-acp")];
        // Catalog present but doesn't (yet) include this agent id -- an
        // incomplete/still-loading read, not evidence the agent is gone.
        let agents = [catalog_entry(
            "claude-acp",
            crate::protocol_types::AgentStatus::Installed,
        )];
        assert_eq!(
            agent_detected_for_profile(&profiles, &agents, "codex-profile"),
            None
        );
    }

    // PUI-015: the built-in snapshotd daemon row is only produced when the
    // daemon is reachable, is non-removable, and names the same /mcp
    // endpoint the session injection uses.
    #[test]
    fn builtin_snapshotd_option_is_present_and_non_removable_only_when_reachable() {
        assert!(
            builtin_snapshotd_option(None).is_none(),
            "no built-in row when the daemon is unreachable"
        );
        let addr = "127.0.0.1:43210";
        let row =
            builtin_snapshotd_option(Some(addr.to_owned())).expect("row present when reachable");
        assert_eq!(row.name.as_str(), "snapflow");
        assert!(!row.removable, "built-in daemon row must not be removable");
        assert_eq!(row.transport.as_str(), "http");
        assert!(
            row.url.as_str().ends_with("/mcp"),
            "must point at the streamable-HTTP /mcp endpoint, got {}",
            row.url
        );
        assert!(
            row.url.contains(addr),
            "must name the same address the session injection uses"
        );
        // Enabled state tracks the live injection gate (default true).
        assert_eq!(
            row.enabled,
            crate::agent_bridge::snapflow_mcp_enabled(),
            "built-in row.enabled must mirror injection gate"
        );
    }

    // Registry-derived rows stay removable (only the built-in daemon is not).
    #[test]
    fn registry_mcp_rows_are_removable() {
        let entry = crate::protocol_types::McpServerEntry::new(
            "my-server",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "do-thing".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].removable, "user-added registry rows are removable");
    }

    #[test]
    fn stdio_option_row_surfaces_args_and_env_as_formatted_lines() {
        let entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec!["--root".to_string(), "/tmp".to_string()],
                env: std::collections::HashMap::from([
                    ("B_KEY".to_string(), "2".to_string()),
                    ("A_KEY".to_string(), "1".to_string()),
                ]),
                timeout: Some(30),
            },
        );
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].args.as_str(), "--root /tmp");
        // Sorted by key for deterministic output, not HashMap iteration order.
        assert_eq!(rows[0].env.as_str(), "A_KEY=1\nB_KEY=2");
        assert_eq!(rows[0].timeout.as_str(), "30");
        assert_eq!(rows[0].headers.as_str(), "");
        assert_eq!(rows[0].oauth_client_id.as_str(), "");
    }

    #[test]
    fn http_option_row_surfaces_headers_and_oauth_client_id() {
        let entry = crate::protocol_types::McpServerEntry::new(
            "remote",
            crate::protocol_types::McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer abc".to_string(),
                )]),
                timeout: None,
                oauth: Some(crate::protocol_types::OAuthClientConfig {
                    client_id: "client-123".to_string(),
                }),
            },
        );
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].headers.as_str(), "Authorization: Bearer abc");
        assert_eq!(rows[0].oauth_client_id.as_str(), "client-123");
        assert_eq!(rows[0].args.as_str(), "");
        assert_eq!(rows[0].env.as_str(), "");
        assert_eq!(rows[0].timeout.as_str(), "");
    }

    /// A tool present in a real `Ready` live catalog with no persisted
    /// preference yet must still show up, defaulting to enabled/not
    /// deferred.
    #[test]
    fn tool_row_defaults_to_enabled_when_only_seen_in_the_live_catalog() {
        let mut entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        entry.tool_catalog = Some(crate::protocol_types::McpToolCatalog::Ready {
            tools: vec![crate::protocol_types::McpToolInfo {
                name: "read_file".to_string(),
                description: None,
            }],
        });
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        let tools: Vec<_> = rows[0].tools.iter().collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_str(), "read_file");
        assert!(tools[0].enabled);
        assert!(!tools[0].deferred);
        assert_eq!(rows[0].tool_fetch_status.as_str(), "ready");
        assert_eq!(rows[0].tool_fetch_error.as_str(), "");
    }

    /// A tool's persisted preference (set by a previous enabled/deferred
    /// toggle) must override the live catalog's bare-discovery default,
    /// and must survive even when the live catalog temporarily doesn't
    /// include that tool (e.g. before the first fetch, or a fetch that
    /// failed).
    #[test]
    fn persisted_tool_preference_overrides_live_default_and_survives_a_missing_catalog_entry() {
        let mut entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        entry.extra.insert(
            "tools".to_string(),
            serde_json::json!([
                {"name": "read_file", "enabled": false, "deferred": true, "token_usage": 42},
                {"name": "vanished_tool", "enabled": true, "deferred": false},
            ]),
        );
        entry.tool_catalog = Some(crate::protocol_types::McpToolCatalog::Ready {
            tools: vec![crate::protocol_types::McpToolInfo {
                name: "read_file".to_string(),
                description: None,
            }],
        });
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        let tools: Vec<_> = rows[0].tools.iter().collect();
        assert_eq!(tools.len(), 2);
        let read_file = tools.iter().find(|t| t.name == "read_file").expect("read_file row");
        assert!(!read_file.enabled, "persisted preference must override the live default");
        assert!(read_file.deferred);
        assert_eq!(read_file.token_usage, 42);
        let vanished = tools
            .iter()
            .find(|t| t.name == "vanished_tool")
            .expect("a tool absent from the live catalog must still show its last preference");
        assert!(vanished.enabled);
        assert!(!vanished.deferred);
    }

    /// `tool_fetch_status`/`tool_fetch_error` reflect a failed background
    /// probe -- distinct from "never fetched" (empty string).
    #[test]
    fn tool_fetch_error_state_surfaces_the_real_message() {
        let mut entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        entry.tool_catalog = Some(crate::protocol_types::McpToolCatalog::Error {
            message: "process exited before responding".to_string(),
        });
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].tool_fetch_status.as_str(), "error");
        assert_eq!(rows[0].tool_fetch_error.as_str(), "process exited before responding");
    }

    /// Never-fetched entries (no `tool_catalog` at all) must not be
    /// confused with a `Fetching` state -- both differ from "ready"/
    /// "error", but only one means "nothing has ever been requested".
    #[test]
    fn never_fetched_entry_has_empty_fetch_status() {
        let entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].tool_fetch_status.as_str(), "");
        assert!(rows[0].tools.iter().next().is_none());
    }

    /// In-flight `tools_fetch:<name>` must surface as fetching even when
    /// the catalog has not yet stamped `toolCatalog` (optimistic UI path).
    #[test]
    fn busy_tools_fetch_key_marks_row_fetching() {
        let entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        let rows =
            to_mcp_server_option_rows(vec![entry], &["tools_fetch:fs".to_owned()]);
        assert_eq!(rows[0].tool_fetch_status.as_str(), "fetching");
    }

    /// Enable toggle drives StatusDot via derived connection status when
    /// the gateway does not supply `extra["status"]`.
    #[test]
    fn disabled_server_status_is_disconnected() {
        let mut entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        entry.enabled = false;
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].status.as_str(), "disconnected");
        assert!(!rows[0].enabled);
    }

    #[test]
    fn enabled_server_status_is_connected() {
        let mut entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        entry.enabled = true;
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].status.as_str(), "connected");
        assert!(rows[0].enabled);
    }

    /// `tools_search_blob` is what the Settings page search bar matches
    /// against (see `mcp_servers_view.slint`'s widened predicate) --
    /// verify it actually joins every real discovered tool's name and
    /// description, and skips empty descriptions rather than leaving a
    /// stray blank line that would still (harmlessly, but confusingly)
    /// match an empty query.
    #[test]
    fn tools_search_blob_joins_real_tool_names_and_descriptions() {
        let mut entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        entry.tool_catalog = Some(crate::protocol_types::McpToolCatalog::Ready {
            tools: vec![
                crate::protocol_types::McpToolInfo {
                    name: "read_file".to_string(),
                    description: Some("Reads a file from disk".to_string()),
                },
                crate::protocol_types::McpToolInfo {
                    name: "ping".to_string(),
                    description: None,
                },
            ],
        });
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(
            rows[0].tools_search_blob.as_str(),
            "read_file\nReads a file from disk\nping"
        );
    }

    #[test]
    fn tools_search_blob_is_empty_when_no_tools_have_ever_been_fetched() {
        let entry = crate::protocol_types::McpServerEntry::new(
            "fs",
            crate::protocol_types::McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        let rows = to_mcp_server_option_rows(vec![entry], &[]);
        assert_eq!(rows[0].tools_search_blob.as_str(), "");
    }

    #[test]
    fn parse_kv_lines_splits_on_first_separator_and_skips_malformed_lines() {
        let parsed = parse_kv_lines(
            "A=1\n\nB=2\nNO_SEPARATOR_HERE\n=empty-key\nC=has=equals=too",
            '=',
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.get("A").map(String::as_str), Some("1"));
        assert_eq!(parsed.get("B").map(String::as_str), Some("2"));
        // Splits on the FIRST '=' only -- the value keeps any further '='s.
        assert_eq!(parsed.get("C").map(String::as_str), Some("has=equals=too"));
    }

    #[test]
    fn parse_kv_lines_keeps_colons_inside_a_header_value() {
        let parsed = parse_kv_lines("Authorization: Bearer a:b:c", ':');
        assert_eq!(
            parsed.get("Authorization").map(String::as_str),
            Some("Bearer a:b:c")
        );
    }

    #[test]
    fn mcp_server_entry_from_form_builds_a_stdio_entry() {
        let form = McpServerFormData {
            name: "fs".into(),
            transport: "stdio".into(),
            command: "mcp-fs".into(),
            args: "--root /tmp  --verbose".into(),
            env: "TOKEN=abc\nDEBUG=1".into(),
            url: "".into(),
            headers: "".into(),
            timeout: "30".into(),
            oauth_client_id: "".into(),
            is_edit: false,
        };
        let entry = mcp_server_entry_from_form(&form);
        assert_eq!(entry.name, "fs");
        match entry.config {
            crate::protocol_types::McpServerConfig::Stdio {
                command,
                args,
                env,
                timeout,
            } => {
                assert_eq!(command, "mcp-fs");
                assert_eq!(args, vec!["--root", "/tmp", "--verbose"]);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc"));
                assert_eq!(timeout, Some(30));
            }
            other => panic!("expected Stdio config, got {other:?}"),
        }
    }

    #[test]
    fn mcp_server_entry_from_form_builds_an_http_entry_with_oauth() {
        let form = McpServerFormData {
            name: "remote".into(),
            transport: "http".into(),
            command: "".into(),
            args: "".into(),
            env: "".into(),
            url: "https://example.com/mcp".into(),
            headers: "Authorization: Bearer xyz".into(),
            timeout: "".into(),
            oauth_client_id: "client-abc".into(),
            is_edit: true,
        };
        let entry = mcp_server_entry_from_form(&form);
        match entry.config {
            crate::protocol_types::McpServerConfig::Http {
                url,
                headers,
                timeout,
                oauth,
            } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer xyz")
                );
                assert_eq!(timeout, None, "blank timeout must parse to None, not 0");
                assert_eq!(
                    oauth.map(|o| o.client_id),
                    Some("client-abc".to_string())
                );
            }
            other => panic!("expected Http config, got {other:?}"),
        }
    }

    #[test]
    fn mcp_server_entry_from_form_treats_blank_oauth_client_id_as_no_oauth() {
        let form = McpServerFormData {
            name: "remote".into(),
            transport: "http".into(),
            command: "".into(),
            args: "".into(),
            env: "".into(),
            url: "https://example.com/mcp".into(),
            headers: "".into(),
            timeout: "".into(),
            oauth_client_id: "   ".into(),
            is_edit: false,
        };
        let entry = mcp_server_entry_from_form(&form);
        match entry.config {
            crate::protocol_types::McpServerConfig::Http { oauth, .. } => {
                assert!(oauth.is_none());
            }
            other => panic!("expected Http config, got {other:?}"),
        }
    }

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
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "",
        );
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].item.name, "Fix timeline crash");
        assert_eq!(items[0].real_index, 0);
        assert_eq!(items[3].item.name, "Export pipeline bug");
        assert_eq!(items[3].real_index, 3);
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "FADE",
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
        // Real index must survive filtering -- "Add fade transition" is
        // THREAD_NAMES[1], even though it's now row 0 of the filtered
        // list. This is exactly the mismatch `real_index` exists to fix.
        assert_eq!(items[0].real_index, 1);

        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "fade",
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
    }

    #[test]
    fn multiple_matches_preserve_original_order_no_resort() {
        // "x" appears in 2 non-adjacent names (index 0 and 3); must come
        // back in the same relative order as NAMES, not re-sorted, and
        // must skip the non-matching ones in between.
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "x",
        );
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
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "   ",
        );
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn status_is_carried_through_unfiltered() {
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "",
        );
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
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            closed,
            NO_ARCHIVED,
            "",
        );
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
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            closed,
            archived,
            "",
        );
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
        let items = build_thread_items(
            NAMES,
            STATE,
            &descriptions,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "fade",
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.description, "Added a fade");
    }

    #[test]
    fn description_defaults_to_empty_when_shorter_than_names() {
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "",
        );
        assert!(items.iter().all(|i| i.item.description.is_empty()));
    }

    #[test]
    fn background_policy_is_preserved_after_filtering() {
        let items = build_thread_items(
            NAMES,
            STATE,
            NO_DESCRIPTIONS,
            BACKGROUND,
            NO_CLOSED,
            NO_ARCHIVED,
            "fade",
        );
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

    // Regression coverage for `chat_area.slint`'s formerly Slint-side,
    // hand-unrolled-to-10 group scan (undercounted/mis-rendered any
    // contiguous tool-kind run past that cap) -- `assign_tool_group_lengths`
    // replaces it with an unbounded Rust pass. 15 > the old cap of 10.
    #[test]
    fn tool_group_length_is_not_capped_at_ten() {
        let mut msgs = vec![chat_msg(MessageKind::User, "start", None)];
        for _ in 0..15 {
            msgs.push(chat_msg(MessageKind::ToolCall, "x", Some("completed")));
        }
        msgs.push(chat_msg(MessageKind::User, "end", None));
        let model = to_message_model(msgs, &[]);
        assert_eq!(model.row_count(), 17);
        assert_eq!(model.row_data(0).unwrap().tool_group_len, 0);
        assert_eq!(model.row_data(1).unwrap().tool_group_len, 15);
        for i in 2..16 {
            assert_eq!(model.row_data(i).unwrap().tool_group_len, 0);
        }
        assert_eq!(model.row_data(16).unwrap().tool_group_len, 0);
    }

    #[test]
    fn tool_group_length_handles_multiple_separate_groups() {
        let msgs = vec![
            chat_msg(MessageKind::ToolCall, "a", Some("completed")),
            chat_msg(MessageKind::ToolCall, "b", Some("completed")),
            chat_msg(MessageKind::User, "between", None),
            chat_msg(MessageKind::ToolCall, "c", Some("completed")),
        ];
        let model = to_message_model(msgs, &[]);
        assert_eq!(model.row_data(0).unwrap().tool_group_len, 2);
        assert_eq!(model.row_data(1).unwrap().tool_group_len, 0);
        assert_eq!(model.row_data(2).unwrap().tool_group_len, 0);
        assert_eq!(model.row_data(3).unwrap().tool_group_len, 1);
    }

    #[test]
    fn terminal_rows_carry_title_active_and_start_time_from_the_buffer() {
        let rows = to_terminal_item_rows(vec![(
            "term_7".to_owned(),
            Some(TerminalBuffer {
                output: "hi".to_owned(),
                truncated: false,
                exit_status: None, // still running -> active
                command: "cargo test".to_owned(),
                args: vec!["--lib".to_owned()],
                started_at: "2026-07-24T05:00:00.000000000Z".to_owned(),
            }),
        )]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_str(), "cargo test");
        assert_eq!(rows[0].last_command.as_str(), "cargo test --lib");
        assert!(rows[0].active, "a non-exited terminal is active");
        assert_eq!(
            rows[0].started_at.as_str(),
            "2026-07-24T05:00:00.000000000Z"
        );
    }

    #[test]
    fn to_mcp_server_options_extracts_name_and_command_falling_back_to_empty() {
        use crate::protocol_types::{McpServerConfig, McpServerEntry};
        let servers = vec![
            McpServerEntry::new(
                "central-fs",
                McpServerConfig::Stdio {
                    command: "mcp-central-fs".to_string(),
                    args: vec![],
                    env: Default::default(),
                    timeout: None,
                },
            ),
            // A URL-based (http transport) server -- must not panic or
            // drop the row, and must fall back to an empty `command`.
            McpServerEntry::new(
                "url-only",
                McpServerConfig::Http {
                    url: "https://example.com/mcp".to_string(),
                    headers: Default::default(),
                    timeout: None,
                    oauth: None,
                },
            ),
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
        use crate::protocol_types::{McpServerConfig, McpServerEntry, OAuthClientConfig};
        use slint::Model;
        let mut entry = McpServerEntry::new(
            "remote-tools",
            McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                headers: Default::default(),
                timeout: None,
                oauth: Some(OAuthClientConfig {
                    client_id: "client-123".to_string(),
                }),
            },
        );
        entry.extra.insert(
            "tools".to_string(),
            serde_json::json!([
                { "name": "read", "enabled": true, "deferred": false, "token_usage": 12 },
                { "name": "write", "enabled": false, "deferred": true }
            ]),
        );
        let servers = vec![entry];
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
            vec![
                "codex-acp",
                "claude-acp",
                "blocked-acp",
                "aardvark-acp",
                "zebra-acp"
            ],
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
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let model =
            to_message_model_from_transcript(state.items().to_vec(), &[false], &mut render_index);
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
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let one_shot = markdown_lines_for(&mut render_index, "k", 0, "agent", full);
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
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        assert_eq!(
            markdown_lines_for(&mut render_index, "k1", 0, "user", "# not parsed").row_count(),
            0
        );
        assert!(markdown_lines_for(&mut render_index, "k2", 1, "agent", "# Title").row_count() > 0);
    }

    #[test]
    fn markdown_lines_for_cache_hit_returns_equivalent_content_for_repeated_text() {
        // Same call repeated for identical text, same key -- the case
        // that fires on every poll tick for an already-rendered
        // historical message (see
        // memory/acpx/gen/plans/panel-thread-switch-freeze-fix-plan.md).
        // Correctness matters more than proving cache-hit-ness here: a
        // wrong cached value would be a worse bug than a slow one.
        let text = "Hello **world**, this is *italic* and `code`.";
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let first = markdown_lines_for(&mut render_index, "k", 0, "agent", text);
        let second = markdown_lines_for(&mut render_index, "k", 0, "agent", text);
        assert_eq!(first.row_count(), second.row_count());
        for i in 0..first.row_count() {
            let a = first.row_data(i).unwrap();
            let b = second.row_data(i).unwrap();
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.plain_text, b.plain_text);
        }
    }

    #[test]
    fn markdown_lines_for_second_call_with_same_key_is_a_genuine_cache_hit() {
        // Strengthens the test above: prove reuse via ThreadMessageIndex's
        // own bookkeeping, not just equal output.
        let text = "Some **repeated** agent text.";
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        markdown_lines_for(&mut render_index, "k", 0, "agent", text);
        assert!(render_index.rendered_lines_for("k").is_some());
        assert_eq!(
            render_index.check("k", text),
            crate::thread_message_index::RowChange::Unchanged(0)
        );
    }

    #[test]
    fn markdown_lines_for_distinguishes_different_text() {
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let a = markdown_lines_for(&mut render_index, "k1", 0, "agent", "# First");
        let b = markdown_lines_for(&mut render_index, "k2", 1, "agent", "# Second");
        assert_ne!(a.row_data(0).unwrap().plain_text, b.row_data(0).unwrap().plain_text);
    }

    #[test]
    fn markdown_blocks_for_non_agent_kind_is_empty() {
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        assert_eq!(
            markdown_blocks_for(&mut render_index, "k", 0, "user", "# not parsed", false)
                .row_count(),
            0
        );
    }

    #[test]
    fn markdown_blocks_for_heading_and_paragraph_produce_text_blocks_with_font_size_by_level() {
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let blocks = markdown_blocks_for(
            &mut render_index,
            "k",
            0,
            "agent",
            "# Title\n\nBody text.\n",
            false,
        );
        assert_eq!(blocks.row_count(), 2);
        let heading = blocks.row_data(0).unwrap();
        assert_eq!(heading.kind, slint::SharedString::from("text"));
        assert_eq!(heading.default_font_size, 18.0);
        let body = blocks.row_data(1).unwrap();
        assert_eq!(body.kind, slint::SharedString::from("text"));
        assert_eq!(body.default_font_size, 0.0);
    }

    #[test]
    fn markdown_blocks_for_code_block_carries_verbatim_text_not_a_styled_text() {
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let blocks = markdown_blocks_for(
            &mut render_index,
            "k",
            0,
            "agent",
            "```\nlet x = 1;\n```\n",
            false,
        );
        assert_eq!(blocks.row_count(), 1);
        let block = blocks.row_data(0).unwrap();
        assert_eq!(block.kind, slint::SharedString::from("code"));
        assert_eq!(block.code_text, slint::SharedString::from("let x = 1;"));
    }

    #[test]
    fn markdown_blocks_for_table_produces_flat_cells_and_col_count() {
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let blocks = markdown_blocks_for(
            &mut render_index,
            "k",
            0,
            "agent",
            "| a | b |\n|---|---|\n| 1 | 2 |\n",
            false,
        );
        assert_eq!(blocks.row_count(), 1);
        let table = blocks.row_data(0).unwrap();
        assert_eq!(table.kind, slint::SharedString::from("table"));
        assert_eq!(table.table_col_count, 2);
        assert_eq!(table.table_cells.row_count(), 4);
    }

    #[test]
    fn markdown_blocks_for_cache_hit_on_repeated_non_streaming_text() {
        let text = "Repeated **agent** text.";
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let first = markdown_blocks_for(&mut render_index, "k", 0, "agent", text, false);
        let second = markdown_blocks_for(&mut render_index, "k", 0, "agent", text, false);
        assert_eq!(first.row_count(), second.row_count());
        assert!(render_index.rendered_blocks_for("k").is_some());
    }

    #[test]
    fn markdown_blocks_for_streaming_tail_heals_unterminated_html_tag_without_erroring() {
        // Doesn't panic/produce an Err path -- from_markdown's fallback to
        // from_plain_text only fires when healing didn't fully fix things,
        // and the real assertion here is just that this returns *a* block
        // at all rather than losing the in-progress message entirely.
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        let blocks = markdown_blocks_for(&mut render_index, "k", 0, "agent", "Hello <u>wor", true);
        assert_eq!(blocks.row_count(), 1);
    }

    #[test]
    fn markdown_blocks_for_streaming_tail_never_populates_the_index() {
        // Matches the old MARKDOWN_BLOCK_CACHE's exact bypass behavior:
        // while streaming, the index must not be read or written for
        // this key, so the eventual settled (non-streaming) render is
        // never satisfied by a stale, possibly-healed streaming render.
        let mut render_index = crate::thread_message_index::ThreadMessageIndex::default();
        markdown_blocks_for(&mut render_index, "k", 0, "agent", "Hello <u>wor", true);
        assert!(render_index.rendered_blocks_for("k").is_none());
        assert_eq!(
            render_index.check("k", "Hello <u>wor"),
            crate::thread_message_index::RowChange::New
        );
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
        // Empty catalog: PROF-10's fail-open posture (catalog not loaded
        // yet is not evidence an agent is missing), so nothing is filtered
        // here -- exercised on its own below.
        let entries = to_profile_dropdown_entries(&profiles, &[], "work");
        assert_eq!(entries.row_count(), 3); // one per agent + new-provider action
        assert_eq!(entries.row_data(0).unwrap().label.as_str(), "codex-acp");
        assert_eq!(entries.row_data(0).unwrap().value.as_str(), "codex-acp");
        assert!(entries.row_data(0).unwrap().is_current);
        assert_eq!(entries.row_data(1).unwrap().label.as_str(), "claude-acp");
        assert_eq!(entries.row_data(2).unwrap().id.as_str(), "__new_provider__");
        assert_eq!(
            entries.row_data(2).unwrap().label.as_str(),
            "+ New provider"
        );
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
        let filtered = to_config_dropdown_entries(options);
        // header + both provider-scoped values. Provider ownership is
        // authoritative from models/list/session state; the panel no longer
        // drops namespaced values using string heuristics.
        assert_eq!(filtered.row_count(), 3);
        assert_eq!(
            filtered.row_data(1).unwrap().value.as_str(),
            "codex-acp/gpt-5"
        );
        assert_eq!(
            filtered.row_data(2).unwrap().value.as_str(),
            "claude-acp/sonnet"
        );
    }

    /// PROF-10: a provider whose agent the catalog genuinely reports as
    /// NOT `Installed`/`InstalledNoSession` must not appear in the
    /// compose-bar picker at all -- but a profile with no `agent_id`
    /// (native/unmanaged mode) and one whose `agent_id` isn't in the
    /// catalog yet (still-loading read) must both still be listed, same
    /// fail-open posture as `agent_detected_for_profile`.
    #[test]
    fn provider_dropdown_hides_providers_the_catalog_reports_as_not_live() {
        let profiles = vec![
            ProfileOption {
                name: "work".into(),
                agent_id: "codex-acp".into(),
                terminal_enabled: true,
                fs_enabled: true,
            },
            ProfileOption {
                name: "gone".into(),
                agent_id: "vanished-acp".into(),
                terminal_enabled: true,
                fs_enabled: true,
            },
            ProfileOption {
                name: "still-loading".into(),
                agent_id: "unknown-to-catalog-yet".into(),
                terminal_enabled: true,
                fs_enabled: true,
            },
            ProfileOption {
                name: "native".into(),
                agent_id: "".into(),
                terminal_enabled: true,
                fs_enabled: true,
            },
        ];
        // Inlined rather than reusing `models::tests::catalog_entry` --
        // this test lives in `transcript_model_tests`, a sibling module
        // that helper is private to.
        let agent_catalog_entry = |id: &str, status: crate::protocol_types::AgentStatus| {
            crate::protocol_types::AgentCatalogEntry {
                id: id.to_owned(),
                name: id.to_owned(),
                version: String::new(),
                status,
                enabled: true,
            }
        };
        let agents = [
            agent_catalog_entry("codex-acp", crate::protocol_types::AgentStatus::Installed),
            agent_catalog_entry(
                "vanished-acp",
                crate::protocol_types::AgentStatus::NotInstalled,
            ),
        ];
        let entries = to_profile_dropdown_entries(&profiles, &agents, "work");
        let labels: Vec<String> = (0..entries.row_count())
            .filter_map(|i| entries.row_data(i))
            .map(|e| e.label.to_string())
            .collect();
        assert_eq!(
            labels,
            vec![
                "codex-acp".to_owned(),
                "unknown-to-catalog-yet".to_owned(),
                "native".to_owned(),
                "+ New provider".to_owned(),
            ],
            "vanished-acp must be hidden; still-loading and native must fail open and stay visible"
        );
    }

    fn model_option(values: &[(&str, &str)]) -> Vec<ConfigOptionInfo> {
        vec![ConfigOptionInfo {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: None,
            kind: "select".into(),
            current_value: values.first().map(|(v, _)| (*v).to_owned()),
            options: values
                .iter()
                .map(|(value, name)| ConfigOptionValue {
                    value: (*value).to_owned(),
                    name: (*name).to_owned(),
                    description: None,
                })
                .collect(),
        }]
    }

    fn dropdown_values(entries: &ModelRc<DropdownEntry>) -> Vec<String> {
        (0..entries.row_count())
            .filter_map(|i| entries.row_data(i))
            .filter(|e| !e.is_header)
            .map(|e| e.value.to_string())
            .collect()
    }

    // Model catalogs are already scoped by the backend; preserve every value
    // returned by the agent instead of applying panel-side vendor guesses.
    fn mixed_catalog() -> Vec<ConfigOptionInfo> {
        model_option(&[
            ("anthropic/claude-sonnet-4", "Claude Sonnet 4"),
            ("anthropic/claude-opus-4", "Claude Opus 4"),
            ("openai/gpt-5", "GPT-5"),
            ("openai/o4-mini", "o4-mini"),
            ("xai/grok-4", "Grok 4"),
        ])
    }

    #[test]
    fn agent_scoped_catalog_preserves_backend_values() {
        let entries = to_config_dropdown_entries(mixed_catalog());
        assert_eq!(
            dropdown_values(&entries),
            vec![
                "anthropic/claude-sonnet-4",
                "anthropic/claude-opus-4",
                "openai/gpt-5",
                "openai/o4-mini",
                "xai/grok-4"
            ]
        );
    }

    // Plan phase 26: project-scoped thread list.
    fn project_items(n: usize) -> Vec<VisibleThreadItem> {
        (0..n)
            .map(|real_index| VisibleThreadItem {
                real_index,
                thread_id: format!("thread:{real_index}"),
                session_id: None,
                agent_detected: None,
                item: ThreadItem::default(),
            })
            .collect()
    }

    #[test]
    fn project_switch_rebinds_the_visible_thread_list() {
        let mut items = project_items(3);
        let paths = vec![
            "/work/a/project.mlt".to_owned(), // other project
            "/work/b/project.mlt".to_owned(), // active project
            String::new(),                    // pre-project thread
        ];
        retain_items_for_project(&mut items, &paths, Some("/work/b/project.mlt"));
        assert_eq!(
            items.iter().map(|i| i.real_index).collect::<Vec<_>>(),
            vec![1, 2],
            "other-project threads drop; legacy-unscoped rows remain visible for compatibility"
        );
    }

    #[test]
    fn no_active_project_keeps_legacy_threads_visible_for_compatibility() {
        let mut items = project_items(2);
        let paths = vec!["/work/a/project.mlt".to_owned(), String::new()];
        retain_items_for_project(&mut items, &paths, None);
        assert_eq!(items.len(), 2);
        let mut items = project_items(2);
        retain_items_for_project(&mut items, &paths, Some(""));
        assert_eq!(items.len(), 2);
    }

    // PISO-5: the indicator must show ONLY a thread's own recorded
    // project, never a live guess at whatever is currently active.
    #[test]
    fn display_project_path_shows_the_threads_own_recorded_project() {
        assert_eq!(
            display_project_path(Some("/work/b/project.mlt")),
            "/work/b/project.mlt"
        );
    }

    #[test]
    fn display_project_path_stays_dark_for_an_unscoped_thread() {
        // Pre-PISO-5 the caller fell back to whatever project happened to
        // be active -- this function deliberately takes no such fallback
        // input at all: an unscoped thread must never appear to belong to
        // a project it was never actually bound to.
        assert_eq!(display_project_path(None), "");
        assert_eq!(display_project_path(Some("")), "");
    }

    fn daemon_instance(path: &str, headless: bool) -> crate::agent_bridge::DaemonProjectInstance {
        crate::agent_bridge::DaemonProjectInstance {
            project_path: path.to_string(),
            headless,
        }
    }

    #[test]
    fn thread_project_instance_is_live_false_for_an_unscoped_thread() {
        let live = vec![daemon_instance("/work/b/project.mlt", true)];
        assert!(!thread_project_instance_is_live(
            "",
            Some("/work/a/project.mlt"),
            &live
        ));
    }

    // PISO-8's negative case: a thread whose project never changed (equal
    // to the panel's own active project) must never show any indicator,
    // even if that same project happens to also have a live daemon
    // instance (e.g. the user's own headful instance).
    #[test]
    fn thread_project_instance_is_live_false_when_thread_project_equals_active() {
        let live = vec![daemon_instance("/work/a/project.mlt", false)];
        assert!(!thread_project_instance_is_live(
            "/work/a/project.mlt",
            Some("/work/a/project.mlt"),
            &live
        ));
    }

    #[test]
    fn thread_project_instance_is_live_false_when_recorded_but_not_actually_live() {
        // Differs from active (so the existing project-name badge WOULD
        // show) but snapshotd reports no live instance for it -- a stale
        // sqlite-recorded association from a session that has since
        // closed, not something the agent is actually driving right now.
        let live = vec![daemon_instance("/work/c/project.mlt", true)];
        assert!(!thread_project_instance_is_live(
            "/work/b/project.mlt",
            Some("/work/a/project.mlt"),
            &live
        ));
    }

    #[test]
    fn thread_project_instance_is_live_true_for_a_confirmed_live_headless_instance() {
        let live = vec![daemon_instance("/work/b/project.mlt", true)];
        assert!(thread_project_instance_is_live(
            "/work/b/project.mlt",
            Some("/work/a/project.mlt"),
            &live
        ));
    }

    #[test]
    fn custom_agent_catalog_is_preserved() {
        let custom = model_option(&[("somevendor/model-x", "Model X")]);
        let entries = to_config_dropdown_entries(custom);
        assert_eq!(dropdown_values(&entries), vec!["somevendor/model-x"]);
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
        assert_eq!(
            reasoning.row_data(0).unwrap().label.as_str(),
            "Reasoning effort"
        );
        assert!(reasoning.row_data(2).unwrap().is_current); // medium
        assert_eq!(current_reasoning_trigger_label(&options), "Medium");
        assert_eq!(current_config_trigger_label(&options), "GPT-5");
    }
}
