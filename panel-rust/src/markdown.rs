//! Markdown -> visual-line/run model for rendering agent message bodies
//! in Slint.
//!
//! This is a tailored port of xAI's grok-build terminal coding agent's
//! streaming markdown renderer (`crates/codegen/xai-grok-markdown` in
//! https://github.com/xai-org/grok-build), which renders markdown into
//! `ratatui::Line`/`Span` for a terminal UI. The core idea carries over
//! directly: parse with `pulldown-cmark`, and while a message streams in
//! chunk by chunk, only the still-open trailing block is re-rendered on
//! each push -- everything before it is "frozen" and reused as-is, so
//! re-render cost stays roughly proportional to the *new* text rather
//! than the whole message so far.
//!
//! Two things don't carry over, because Slint isn't a terminal grid:
//!
//! - grok-build's checkpoints are line-granular (ratatui already lays
//!   out styled `Span`s on a fixed-width character grid). Slint's `Text`
//!   element has no equivalent -- there is no way to mix differently
//!   styled runs inside one wrapped, reflowing `Text`. So instead of
//!   letting Slint wrap prose, wrapping happens here in Rust
//!   (`wrap_runs`), turning each paragraph into a `Vec<Line>` of
//!   already-wrapped visual rows up front. Each `Line` maps to one
//!   `HorizontalLayout` row of `Text` runs in `markdown_view.slint` --
//!   no runtime wrapping needed on the Slint side.
//! - The freeze boundary here is block-granular (all top-level blocks
//!   but the last are frozen), not line-granular. A chat message is a
//!   few KB at most, so re-wrapping one open block per chunk is cheap;
//!   the coarser granularity keeps this file a fraction of the size of
//!   grok-build's checkpoint/source-map machinery for the same payoff.
//!
//! Not implemented (fine for a v1 chat body, unlike a general-purpose
//! terminal renderer): syntax highlighting inside code fences, tables
//! (rendered as plain pipe-joined rows, unwrapped), and link targets
//! (link text renders as plain text -- no click-through yet).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

/// One styled run of text within a visual line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    /// Phase 17 (markdown_highlight_and_real_links): the link target
    /// (URL or file path) when this run is inside a markdown link --
    /// empty for non-link runs. The view styles link runs distinctly
    /// and Ctrl+Click opens the target (files locally, URLs in the
    /// system browser).
    pub link: String,
}

/// What kind of visual line this is -- drives `markdown_view.slint`'s
/// per-kind margin/indent/background treatment (heading size, code
/// block background box, blockquote left bar, list bullet/number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Heading(u8),
    Paragraph,
    Code,
    Quote,
    ListItem,
    OrderedListItem,
    Rule,
    Table,
    Blank,
}

/// One visual (already-wrapped) line -- the unit `markdown_view.slint`
/// loops over, one `HorizontalLayout` row per `Line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub runs: Vec<Run>,
    /// Nesting depth (blockquotes / nested lists), for indentation.
    pub indent: u8,
    /// Ordered-list number, 0 when not applicable.
    pub ordinal: u32,
    /// Groups contiguous `Code` lines belonging to the same fenced block
    /// so the view can draw one continuous background box instead of a
    /// box per line; -1 when not a code line.
    pub code_block_id: i32,
}

/// Column budget used to wrap paragraph/quote/list-item text into
/// discrete visual rows. Approximate (proportional fonts don't wrap at
/// exact character-cell boundaries the way a terminal does) but good
/// enough for a fixed-width chat bubble, and it's what actually lets
/// inline bold/italic/code runs render as real adjacent `Text` elements
/// instead of being flattened to one style per paragraph.
pub const DEFAULT_WRAP_COLS: usize = 64;

/// One top-level markdown block. `parse_blocks` yields these in source
/// order, which is all [`StreamingMarkdownRenderer`] needs to find the
/// freeze boundary: every block but the last is guaranteed closed.
struct Block {
    ast: BlockAst,
}

enum BlockAst {
    Heading(u8, Vec<Run>),
    Paragraph(Vec<Run>),
    Code {
        lines: Vec<String>,
    },
    Quote(Vec<BlockAst>),
    List {
        ordered: bool,
        start: u32,
        items: Vec<Vec<BlockAst>>,
    },
    Rule,
    Table(Vec<Vec<String>>),
}

fn parser_options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES
}

/// Architecture v2 (markdown-thread-freeze-fix phase 3, see
/// memory/acpx/gen/plans/panel-thread-switch-freeze-fix-plan.md's
/// "Architecture v2" section): what kind of leaf a [`MarkdownBlockSpan`]
/// is, and the data needed to render it. Unlike [`BlockAst`]/[`Block`]
/// above (which extract styled `Run`s and get wrapped by `wrap_runs`),
/// this carries no extracted styling at all -- `Text`'s `source_range`
/// is handed directly to Slint's `StyledText::from_markdown`, which
/// parses inline styling and wraps itself, so there is nothing for Rust
/// to extract here beyond block boundaries.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockSpanKind {
    /// Heading/paragraph/list-item/blockquote text -- one `StyledText`.
    Text,
    /// One fenced/indented code block -- rendered verbatim through the
    /// existing `TerminalLogBlock` path, never markdown-reinterpreted.
    /// Carries the already-extracted body text (fence delimiters and
    /// info string stripped, same `Event::Text`-only extraction
    /// `parse_one_block`'s `Tag::CodeBlock` arm above uses) rather than
    /// a raw byte range, since the raw range includes the fence markers
    /// themselves (` ``` `) which are not part of the displayed code.
    Code(String),
    /// A table: row-major cell byte ranges plus the column count, so the
    /// Slint side can lay out a real grid instead of today's flat
    /// pipe-joined string.
    Table {
        cells: Vec<Range<usize>>,
        col_count: usize,
    },
    /// Thematic break (`---`).
    Rule,
}

/// One markdown block's structural span: which kind it is, its raw byte
/// range in the original source (fed to `StyledText::from_markdown`
/// as-is for `Text`/`Table` cells -- no re-serialization, since the
/// source slice is already valid markdown for that block), its heading
/// level if it's a heading (`StyledText.default-font-size` is set from
/// this, not baked into any wrap math), and its container nesting depth
/// (blockquote/list indent).
#[derive(Debug, Clone, PartialEq)]
pub struct MarkdownBlockSpan {
    pub kind: BlockSpanKind,
    pub source_range: Range<usize>,
    pub heading_level: Option<u8>,
    pub indent: u8,
}

/// Closes unterminated `<u>`/`<font ...>` HTML tags so `StyledText::
/// from_markdown` never fails on the actively-streaming tail block.
/// Spike-confirmed (see the plan doc's "Spike-tested and corrected"
/// note): CommonMark emphasis/code/link markers (`**`, `*`, `` ` ``,
/// `[...]`) already degrade to literal text gracefully on their own when
/// left unterminated -- only these two HTML tags hard-error
/// (`StyledTextParseErrorKind::UnterminatedTag`), so this only needs to
/// track those, not a general markdown-balance stack. Only ever called
/// on the single actively-streaming tail block; every other (closed)
/// block is already well-formed source text.
pub fn heal_open_markers(partial: &str) -> String {
    let mut open_tags: Vec<&'static str> = Vec::new();
    let bytes = partial.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if partial[i..].starts_with("<u>") {
                open_tags.push("</u>");
                i += 3;
                continue;
            }
            if partial[i..].starts_with("<font") {
                if let Some(close) = partial[i..].find('>') {
                    open_tags.push("</font>");
                    i += close + 1;
                    continue;
                }
                // `<font` opened but its own `>` hasn't streamed in yet --
                // drop the dangling partial tag rather than guess at
                // attributes; the closing `</font>` push above only
                // fires once the opening tag itself is fully seen.
                return format!("{}{}", &partial[..i], open_tags.into_iter().rev().collect::<String>());
            }
            if partial[i..].starts_with("</u>") {
                if open_tags.last() == Some(&"</u>") {
                    open_tags.pop();
                }
                i += 4;
                continue;
            }
            if partial[i..].starts_with("</font>") {
                if open_tags.last() == Some(&"</font>") {
                    open_tags.pop();
                }
                i += 7;
                continue;
            }
        }
        i += 1;
    }
    if open_tags.is_empty() {
        partial.to_string()
    } else {
        let mut healed = String::with_capacity(partial.len() + open_tags.len() * 7);
        healed.push_str(partial);
        for tag in open_tags.into_iter().rev() {
            healed.push_str(tag);
        }
        healed
    }
}

/// Segments `source` into top-level block spans without extracting any
/// inline styling -- the block-structure-only half of Architecture v2.
/// Nested list/blockquote content is flattened to `Text` spans tagged
/// with `indent` (matching `BlockAst`'s block-kind set) rather than kept
/// as a recursive tree, since `StyledText` needs only a flat sequence of
/// leaf blocks to render, not a tree.
pub fn segment_blocks(source: &str) -> Vec<MarkdownBlockSpan> {
    let parser = Parser::new_ext(source, parser_options());
    let events: Vec<(Event, Range<usize>)> = parser.into_offset_iter().collect();
    let mut spans = Vec::new();
    segment_events(&events, 0, events.len(), 0, &mut spans);
    spans
}

/// Walks `events[start..end]` at nesting `indent`, appending each
/// top-level-at-this-depth block's span to `out`. Recurses one level
/// deeper for blockquote/list contents (same depth-tracking idea as
/// `parse_one_block`'s recursion above, but flat output instead of a
/// tree).
fn segment_events(
    events: &[(Event, Range<usize>)],
    start: usize,
    end: usize,
    indent: u8,
    out: &mut Vec<MarkdownBlockSpan>,
) {
    let mut i = start;
    while i < end {
        let (event, range) = &events[i];
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level = *level;
                let mut j = i + 1;
                while !matches!(&events[j].0, Event::End(TagEnd::Heading(_))) {
                    j += 1;
                }
                out.push(MarkdownBlockSpan {
                    kind: BlockSpanKind::Text,
                    source_range: range.start..events[j].1.end,
                    heading_level: Some(level as u8),
                    indent,
                });
                i = j + 1;
            }
            Event::Start(Tag::Paragraph) => {
                let mut j = i + 1;
                while !matches!(&events[j].0, Event::End(TagEnd::Paragraph)) {
                    j += 1;
                }
                out.push(MarkdownBlockSpan {
                    kind: BlockSpanKind::Text,
                    source_range: range.start..events[j].1.end,
                    heading_level: None,
                    indent,
                });
                i = j + 1;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                let mut body = String::new();
                let mut j = i + 1;
                while !matches!(&events[j].0, Event::End(TagEnd::CodeBlock)) {
                    if let Event::Text(t) = &events[j].0 {
                        body.push_str(t);
                    }
                    j += 1;
                }
                // A fenced block's content always ends with a newline
                // before the closing fence -- same trailing-newline trim
                // `parse_one_block`'s `Tag::CodeBlock` arm above does.
                if body.ends_with('\n') {
                    body.pop();
                }
                out.push(MarkdownBlockSpan {
                    kind: BlockSpanKind::Code(body),
                    source_range: range.start..events[j].1.end,
                    heading_level: None,
                    indent,
                });
                i = j + 1;
            }
            Event::Rule => {
                out.push(MarkdownBlockSpan {
                    kind: BlockSpanKind::Rule,
                    source_range: range.clone(),
                    heading_level: None,
                    indent,
                });
                i += 1;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let mut depth = 1;
                let mut j = i + 1;
                let inner_start = j;
                while depth > 0 {
                    match &events[j].0 {
                        Event::Start(Tag::BlockQuote(_)) => depth += 1,
                        Event::End(TagEnd::BlockQuote(_)) => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                segment_events(events, inner_start, j - 1, indent + 1, out);
                i = j;
            }
            Event::Start(Tag::List(_)) => {
                let mut depth = 1;
                let mut j = i + 1;
                let list_end;
                while depth > 0 {
                    match &events[j].0 {
                        Event::Start(Tag::List(_)) => depth += 1,
                        Event::End(TagEnd::List(_)) => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                list_end = j - 1;
                // Each `Item` needs its own walk, not a blanket recurse
                // over the whole list body: a *tight* list (`- one\n- two`,
                // the common case) emits inline events (`Event::Text`,
                // `Event::Start(Tag::Emphasis)`, ...) directly under
                // `Item` with **no** `Tag::Paragraph` wrapper at all --
                // pulldown-cmark only wraps item content in `Paragraph`
                // for a *loose* list (items separated by a blank line).
                // Recursing blindly (as this used to) meant tight-list
                // items fell through the `_ => i += 1` catch-all doing
                // nothing, silently dropping every tight list's content.
                let mut k = i + 1;
                while k < list_end {
                    match &events[k].0 {
                        Event::Start(Tag::Item) => {
                            let item_start = k + 1;
                            let mut item_depth = 1;
                            let mut m = item_start;
                            while item_depth > 0 {
                                match &events[m].0 {
                                    Event::Start(Tag::Item) => item_depth += 1,
                                    Event::End(TagEnd::Item) => item_depth -= 1,
                                    _ => {}
                                }
                                m += 1;
                            }
                            let item_end = m - 1;
                            let has_block_child = events[item_start..item_end].iter().any(|(e, _)| {
                                matches!(
                                    e,
                                    Event::Start(Tag::Paragraph)
                                        | Event::Start(Tag::List(_))
                                        | Event::Start(Tag::CodeBlock(_))
                                        | Event::Start(Tag::BlockQuote(_))
                                        | Event::Start(Tag::Table(_))
                                )
                            });
                            if has_block_child {
                                segment_events(events, item_start, item_end, indent + 1, out);
                            } else if item_start < item_end {
                                // Tight item: no Paragraph wrapper: the
                                // item's own Start/End range *is* the
                                // text span.
                                out.push(MarkdownBlockSpan {
                                    kind: BlockSpanKind::Text,
                                    // `events[item_start]` (the item's
                                    // first inline content event), not
                                    // `events[k]` (the `Item` container
                                    // itself, whose own range starts at
                                    // the bullet marker `- `) -- matches
                                    // a Paragraph-wrapped item's range,
                                    // which likewise excludes the marker.
                                    source_range: events[item_start].1.start..events[item_end].1.end,
                                    heading_level: None,
                                    indent: indent + 1,
                                });
                            }
                            k = m;
                        }
                        _ => k += 1,
                    }
                }
                i = j;
            }
            Event::Start(Tag::Table(_)) => {
                let mut cells = Vec::new();
                let mut col_count = 0;
                let mut j = i + 1;
                loop {
                    match &events[j].0 {
                        Event::End(TagEnd::Table) => {
                            j += 1;
                            break;
                        }
                        Event::Start(Tag::TableRow) | Event::Start(Tag::TableHead) => {
                            let mut row_len = 0;
                            let mut k = j + 1;
                            loop {
                                match &events[k].0 {
                                    Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                                        k += 1;
                                        break;
                                    }
                                    Event::Start(Tag::TableCell) => {
                                        let cell_start = events[k].1.start;
                                        let mut m = k + 1;
                                        while !matches!(&events[m].0, Event::End(TagEnd::TableCell)) {
                                            m += 1;
                                        }
                                        cells.push(cell_start..events[m].1.end);
                                        row_len += 1;
                                        k = m + 1;
                                    }
                                    _ => k += 1,
                                }
                            }
                            col_count = col_count.max(row_len);
                            j = k;
                        }
                        _ => j += 1,
                    }
                }
                out.push(MarkdownBlockSpan {
                    kind: BlockSpanKind::Table { cells, col_count },
                    source_range: range.start..events[j - 1].1.end,
                    heading_level: None,
                    indent,
                });
                i = j;
            }
            _ => i += 1,
        }
    }
}

/// Parse `source` into its top-level blocks, each tagged with the byte
/// range it spans. Nested content (list items, blockquotes) is parsed
/// recursively into the same [`BlockAst`] shape.
fn parse_blocks(source: &str) -> Vec<Block> {
    let parser = Parser::new_ext(source, parser_options());
    let events: Vec<(Event, Range<usize>)> = parser.into_offset_iter().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let (ast, end_i, _range) = parse_one_block(&events, i);
        blocks.push(Block { ast });
        i = end_i;
    }
    blocks
}

/// Parse a single top-level (or nested) block starting at `events[i]`
/// (expected to be a `Start` or a leaf event). Returns the parsed AST,
/// the index just past its matching `End`, and its byte range.
fn parse_one_block(events: &[(Event, Range<usize>)], i: usize) -> (BlockAst, usize, Range<usize>) {
    let (event, range) = &events[i];
    let start_byte = range.start;
    match event {
        Event::Start(Tag::Heading { level, .. }) => {
            let level = *level;
            let (runs, end_i, end_byte) =
                collect_inline_runs(events, i + 1, TagEnd::Heading(level));
            (
                BlockAst::Heading(level as u8, runs),
                end_i,
                start_byte..end_byte,
            )
        }
        Event::Start(Tag::Paragraph) => {
            let (runs, end_i, end_byte) = collect_inline_runs(events, i + 1, TagEnd::Paragraph);
            (BlockAst::Paragraph(runs), end_i, start_byte..end_byte)
        }
        Event::Start(Tag::CodeBlock(kind)) => {
            let _ = kind;
            let mut lines = vec![String::new()];
            let mut j = i + 1;
            let end_byte;
            loop {
                match &events[j].0 {
                    Event::Text(t) => {
                        for (k, part) in t.split('\n').enumerate() {
                            if k > 0 {
                                lines.push(String::new());
                            }
                            lines.last_mut().unwrap().push_str(part);
                        }
                        j += 1;
                    }
                    Event::End(TagEnd::CodeBlock) => {
                        end_byte = events[j].1.end;
                        j += 1;
                        break;
                    }
                    _ => {
                        j += 1;
                    }
                }
            }
            // A fenced block's content always ends with a newline before
            // the closing fence, which left one spurious trailing empty
            // line in `lines` -- drop it so the code box doesn't show a
            // blank final row.
            if lines.last().map(String::is_empty).unwrap_or(false) && lines.len() > 1 {
                lines.pop();
            }
            (BlockAst::Code { lines }, j, start_byte..end_byte)
        }
        Event::Start(Tag::BlockQuote(_)) => {
            let mut inner = Vec::new();
            let mut j = i + 1;
            let end_byte;
            loop {
                match &events[j].0 {
                    Event::End(TagEnd::BlockQuote(_)) => {
                        end_byte = events[j].1.end;
                        j += 1;
                        break;
                    }
                    _ => {
                        let (ast, next_i, _) = parse_one_block(events, j);
                        inner.push(ast);
                        j = next_i;
                    }
                }
            }
            (BlockAst::Quote(inner), j, start_byte..end_byte)
        }
        Event::Start(Tag::List(start)) => {
            let ordered = start.is_some();
            let start_num = start.unwrap_or(0) as u32;
            let mut items = Vec::new();
            let mut j = i + 1;
            let end_byte;
            loop {
                match &events[j].0 {
                    Event::End(TagEnd::List(_)) => {
                        end_byte = events[j].1.end;
                        j += 1;
                        break;
                    }
                    Event::Start(Tag::Item) => {
                        let mut item_blocks = Vec::new();
                        let mut k = j + 1;
                        loop {
                            match &events[k].0 {
                                Event::End(TagEnd::Item) => {
                                    k += 1;
                                    break;
                                }
                                _ => {
                                    let (ast, next_k, _) = parse_one_block(events, k);
                                    item_blocks.push(ast);
                                    k = next_k;
                                }
                            }
                        }
                        items.push(item_blocks);
                        j = k;
                    }
                    _ => {
                        j += 1;
                    }
                }
            }
            (
                BlockAst::List {
                    ordered,
                    start: start_num,
                    items,
                },
                j,
                start_byte..end_byte,
            )
        }
        Event::Rule => (BlockAst::Rule, i + 1, range.clone()),
        Event::Start(Tag::Table(_)) => {
            let mut rows = Vec::new();
            let mut j = i + 1;
            let end_byte;
            loop {
                match &events[j].0 {
                    Event::End(TagEnd::Table) => {
                        end_byte = events[j].1.end;
                        j += 1;
                        break;
                    }
                    Event::Start(Tag::TableRow) | Event::Start(Tag::TableHead) => {
                        let mut cells = Vec::new();
                        let mut k = j + 1;
                        loop {
                            match &events[k].0 {
                                Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                                    k += 1;
                                    break;
                                }
                                Event::Start(Tag::TableCell) => {
                                    let (runs, next_k, _) =
                                        collect_inline_runs(events, k + 1, TagEnd::TableCell);
                                    cells.push(
                                        runs.iter()
                                            .map(|r| r.text.as_str())
                                            .collect::<Vec<_>>()
                                            .join(""),
                                    );
                                    k = next_k;
                                }
                                _ => {
                                    k += 1;
                                }
                            }
                        }
                        rows.push(cells);
                        j = k;
                    }
                    _ => {
                        j += 1;
                    }
                }
            }
            (BlockAst::Table(rows), j, start_byte..end_byte)
        }
        // Any other leaf/unhandled event at block level (e.g. a stray
        // `Event::Text` from a construct we don't special-case): treat
        // as a one-run paragraph so nothing silently vanishes.
        _ => {
            let text = leaf_event_text(event);
            (
                BlockAst::Paragraph(vec![Run {
                    text,
                    ..Default::default()
                }]),
                i + 1,
                range.clone(),
            )
        }
    }
}

fn leaf_event_text(event: &Event) -> String {
    match event {
        Event::Text(t) => t.to_string(),
        Event::Code(t) => t.to_string(),
        _ => String::new(),
    }
}

/// Collect inline events (text/emphasis/strong/strikethrough/code/
/// links/soft+hard breaks) from `events[start..]` up to the matching
/// `end_tag`, into a flat run list. Soft/hard breaks become a single
/// space -- prose reflows through `wrap_runs` regardless of the
/// source's own line breaks, matching how a browser renders CommonMark.
fn collect_inline_runs(
    events: &[(Event, Range<usize>)],
    start: usize,
    end_tag: TagEnd,
) -> (Vec<Run>, usize, usize) {
    let mut runs = Vec::new();
    let mut bold_depth = 0u32;
    let mut italic_depth = 0u32;
    let mut strike_depth = 0u32;
    let mut link_stack: Vec<String> = Vec::new();
    let mut i = start;
    let end_byte;
    loop {
        let (event, range) = &events[i];
        if std::mem::discriminant(event) == std::mem::discriminant(&Event::End(end_tag))
            && matches!(event, Event::End(t) if *t == end_tag)
        {
            end_byte = range.end;
            i += 1;
            break;
        }
        match event {
            Event::Text(t) => {
                runs.push(Run {
                    text: t.to_string(),
                    bold: bold_depth > 0,
                    italic: italic_depth > 0,
                    code: false,
                    strike: strike_depth > 0,
                    link: link_stack.last().cloned().unwrap_or_default(),
                });
            }
            Event::Code(t) => {
                runs.push(Run {
                    text: t.to_string(),
                    bold: bold_depth > 0,
                    italic: italic_depth > 0,
                    code: true,
                    strike: strike_depth > 0,
                    link: link_stack.last().cloned().unwrap_or_default(),
                });
            }
            Event::SoftBreak | Event::HardBreak => {
                runs.push(Run {
                    text: " ".to_string(),
                    ..Default::default()
                });
            }
            Event::Start(Tag::Strong) => bold_depth += 1,
            Event::End(TagEnd::Strong) => bold_depth = bold_depth.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic_depth += 1,
            Event::End(TagEnd::Emphasis) => italic_depth = italic_depth.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => strike_depth += 1,
            Event::End(TagEnd::Strikethrough) => strike_depth = strike_depth.saturating_sub(1),
            // Link/image wrappers: keep the visible text, drop the target.
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_stack.push(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                link_stack.pop();
            }
            Event::Start(Tag::Image { .. }) => {}
            Event::End(TagEnd::Image) => {}
            _ => {}
        }
        i += 1;
    }
    (runs, i, end_byte)
}

/// Greedy word-wrap over a run sequence, preserving each word's style.
/// Mirrors `textwrap`'s greedy algorithm but operates on styled runs
/// instead of a plain string, so inline bold/italic/code survive
/// wrapping as real adjacent runs on each output line.
fn wrap_runs(runs: &[Run], wrap_cols: usize) -> Vec<Vec<Run>> {
    struct Word {
        text: String,
        style: (bool, bool, bool, bool), // bold, italic, code, strike
        link: String,
    }
    let mut words = Vec::new();
    for run in runs {
        for word in run.text.split(' ') {
            if word.is_empty() {
                continue;
            }
            words.push(Word {
                text: word.to_string(),
                style: (run.bold, run.italic, run.code, run.strike),
                link: run.link.clone(),
            });
        }
    }
    if words.is_empty() {
        return vec![vec![]];
    }

    let mut lines: Vec<Vec<Run>> = Vec::new();
    let mut current: Vec<Run> = Vec::new();
    let mut current_cols = 0usize;
    for word in words {
        let word_cols = word.text.chars().count();
        let needs_space = current_cols > 0;
        let extra = if needs_space { 1 } else { 0 };
        if current_cols > 0 && current_cols + extra + word_cols > wrap_cols {
            lines.push(std::mem::take(&mut current));
            current_cols = 0;
        }
        let (bold, italic, code, strike) = word.style;
        let link = word.link;
        let needs_space = current_cols > 0;
        let text = if needs_space {
            format!(" {}", word.text)
        } else {
            word.text
        };
        // Merge into the previous run when the style matches, so runs
        // in the output stay maximally coalesced (fewer `Text`
        // elements for `markdown_view.slint` to lay out per line).
        if let Some(last) = current.last_mut() {
            if last.bold == bold
                && last.italic == italic
                && last.code == code
                && last.strike == strike
                && last.link == link
            {
                last.text.push_str(&text);
                current_cols += extra + word_cols;
                continue;
            }
        }
        current.push(Run {
            text,
            bold,
            italic,
            code,
            strike,
            link,
        });
        current_cols += extra + word_cols;
    }
    lines.push(current);
    lines
}

fn render_ast(
    ast: &BlockAst,
    indent: u8,
    wrap_cols: usize,
    code_block_id: &mut i32,
    out: &mut Vec<Line>,
) {
    match ast {
        BlockAst::Heading(level, runs) => {
            for wrapped in wrap_runs(runs, wrap_cols) {
                out.push(Line {
                    kind: LineKind::Heading(*level),
                    runs: wrapped,
                    indent,
                    ordinal: 0,
                    code_block_id: -1,
                });
            }
        }
        BlockAst::Paragraph(runs) => {
            for wrapped in wrap_runs(runs, wrap_cols) {
                out.push(Line {
                    kind: LineKind::Paragraph,
                    runs: wrapped,
                    indent,
                    ordinal: 0,
                    code_block_id: -1,
                });
            }
        }
        BlockAst::Code { lines } => {
            let id = *code_block_id;
            *code_block_id += 1;
            for line in lines {
                out.push(Line {
                    kind: LineKind::Code,
                    runs: vec![Run {
                        text: line.clone(),
                        code: true,
                        ..Default::default()
                    }],
                    indent,
                    ordinal: 0,
                    code_block_id: id,
                });
            }
        }
        BlockAst::Quote(inner) => {
            for block in inner {
                let before = out.len();
                render_ast(block, indent + 1, wrap_cols, code_block_id, out);
                // Retag plain paragraph/blank lines as `Quote` so the view
                // draws the left accent bar; headings/code/rules/list
                // items keep their own kind (their indent alone reflects
                // quote nesting) since those already carry a more
                // specific treatment.
                for line in &mut out[before..] {
                    if line.kind == LineKind::Paragraph || line.kind == LineKind::Blank {
                        line.kind = LineKind::Quote;
                    }
                }
            }
        }
        BlockAst::List {
            ordered,
            start,
            items,
        } => {
            for (idx, item) in items.iter().enumerate() {
                let ordinal = if *ordered { start + idx as u32 } else { 0 };
                let mut first = true;
                for block in item {
                    let kind_override = first;
                    first = false;
                    let before = out.len();
                    render_ast(block, indent + 1, wrap_cols, code_block_id, out);
                    if kind_override {
                        if let Some(line) = out.get_mut(before) {
                            line.kind = if *ordered {
                                LineKind::OrderedListItem
                            } else {
                                LineKind::ListItem
                            };
                            line.ordinal = ordinal;
                        }
                    }
                }
            }
        }
        BlockAst::Rule => {
            out.push(Line {
                kind: LineKind::Rule,
                runs: vec![],
                indent,
                ordinal: 0,
                code_block_id: -1,
            });
        }
        BlockAst::Table(rows) => {
            for row in rows {
                out.push(Line {
                    kind: LineKind::Table,
                    runs: vec![Run {
                        text: row.join("  |  "),
                        ..Default::default()
                    }],
                    indent,
                    ordinal: 0,
                    code_block_id: -1,
                });
            }
        }
    }
}

/// Render one top-level block into its visual lines.
fn render_block(ast: &BlockAst, wrap_cols: usize, code_block_id: &mut i32) -> Vec<Line> {
    let mut out = Vec::new();
    render_ast(ast, 0, wrap_cols, code_block_id, &mut out);
    out
}

/// One-shot full render of a complete markdown document.
pub fn render_document(source: &str, wrap_cols: usize) -> Vec<Line> {
    let blocks = parse_blocks(source);
    let mut code_block_id = 0;
    let mut out = Vec::new();
    for block in &blocks {
        out.extend(render_block(&block.ast, wrap_cols, &mut code_block_id));
    }
    out
}

/// Incremental renderer for markdown arriving in chunks (e.g. an LLM
/// streaming response). See the module doc comment for how this
/// relates to grok-build's `StreamingMarkdownRenderer`.
pub struct StreamingMarkdownRenderer {
    source: String,
    wrap_cols: usize,
    /// How many leading top-level blocks are frozen (rendered once and
    /// cached in `frozen_lines`, never re-wrapped again).
    frozen_block_count: usize,
    frozen_lines: Vec<Line>,
    frozen_code_block_id: i32,
}

impl StreamingMarkdownRenderer {
    pub fn new(wrap_cols: usize) -> Self {
        Self {
            source: String::new(),
            wrap_cols,
            frozen_block_count: 0,
            frozen_lines: Vec::new(),
            frozen_code_block_id: 0,
        }
    }

    /// Append a chunk of markdown text (no rendering).
    pub fn push(&mut self, chunk: &str) {
        self.source.push_str(chunk);
    }

    /// Number of leading blocks currently frozen -- exposed for tests.
    pub fn frozen_block_count(&self) -> usize {
        self.frozen_block_count
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Render accumulated content. Only the still-open trailing block
    /// (if any) is re-wrapped; everything before it is frozen and
    /// reused as-is.
    pub fn render(&mut self) -> Vec<Line> {
        let blocks = parse_blocks(&self.source);
        // All blocks but the last are guaranteed closed (pulldown-cmark
        // already committed to their extent once a later block starts).
        let closed_count = blocks.len().saturating_sub(1);
        while self.frozen_block_count < closed_count {
            let block = &blocks[self.frozen_block_count];
            self.frozen_lines.extend(render_block(
                &block.ast,
                self.wrap_cols,
                &mut self.frozen_code_block_id,
            ));
            self.frozen_block_count += 1;
        }
        let mut out = self.frozen_lines.clone();
        if let Some(tail) = blocks.get(self.frozen_block_count) {
            let mut tail_code_id = self.frozen_code_block_id;
            out.extend(render_block(&tail.ast, self.wrap_cols, &mut tail_code_id));
        }
        out
    }

    /// Finalize streaming: freeze every remaining block and return the
    /// full rendered output. Idempotent -- safe to call once streaming
    /// is known to be complete.
    pub fn finish(&mut self) -> Vec<Line> {
        let blocks = parse_blocks(&self.source);
        while self.frozen_block_count < blocks.len() {
            let block = &blocks[self.frozen_block_count];
            self.frozen_lines.extend(render_block(
                &block.ast,
                self.wrap_cols,
                &mut self.frozen_code_block_id,
            ));
            self.frozen_block_count += 1;
        }
        self.frozen_lines.clone()
    }

    pub fn clear(&mut self) {
        self.source.clear();
        self.frozen_block_count = 0;
        self.frozen_lines.clear();
        self.frozen_code_block_id = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn heading_and_paragraph() {
        let lines = render_document("# Title\n\nHello world.\n", DEFAULT_WRAP_COLS);
        assert!(matches!(lines[0].kind, LineKind::Heading(1)));
        assert_eq!(lines[0].runs[0].text, "Title");
        assert!(matches!(lines[1].kind, LineKind::Paragraph));
        assert_eq!(lines[1].runs[0].text, "Hello world.");
    }

    #[test]
    fn inline_bold_italic_code_produce_separate_runs() {
        let lines = render_document("a **bold** and *italic* and `code`.", DEFAULT_WRAP_COLS);
        let runs = &lines[0].runs;
        assert!(runs.iter().any(|r| r.bold && r.text.contains("bold")));
        assert!(runs.iter().any(|r| r.italic && r.text.contains("italic")));
        assert!(runs.iter().any(|r| r.code && r.text.contains("code")));
    }

    #[test]
    fn fenced_code_block_lines() {
        let lines = render_document(
            "```rust\nfn main() {}\nlet x = 1;\n```\n",
            DEFAULT_WRAP_COLS,
        );
        let code_lines: Vec<&Line> = lines.iter().filter(|l| l.kind == LineKind::Code).collect();
        assert_eq!(code_lines.len(), 2);
        assert_eq!(code_lines[0].runs[0].text, "fn main() {}");
        assert_eq!(code_lines[1].runs[0].text, "let x = 1;");
        assert_eq!(code_lines[0].code_block_id, code_lines[1].code_block_id);
    }

    #[test]
    fn unordered_list_items() {
        let lines = render_document("- one\n- two\n", DEFAULT_WRAP_COLS);
        assert!(lines.iter().all(|l| l.kind == LineKind::ListItem));
        assert_eq!(texts(&lines), vec!["one", "two"]);
    }

    #[test]
    fn ordered_list_ordinals() {
        let lines = render_document("1. first\n2. second\n", DEFAULT_WRAP_COLS);
        assert_eq!(lines[0].ordinal, 1);
        assert_eq!(lines[1].ordinal, 2);
    }

    #[test]
    fn blockquote_lines_tagged_quote() {
        let lines = render_document("> quoted text\n", DEFAULT_WRAP_COLS);
        assert_eq!(lines[0].kind, LineKind::Quote);
        assert_eq!(lines[0].indent, 1);
    }

    #[test]
    fn thematic_break() {
        let lines = render_document("above\n\n---\n\nbelow\n", DEFAULT_WRAP_COLS);
        assert!(lines.iter().any(|l| l.kind == LineKind::Rule));
    }

    #[test]
    fn wrap_splits_long_paragraph_and_preserves_bold_run() {
        let text = "one two three four **five six seven eight** nine ten";
        let lines = render_document(text, 20);
        assert!(
            lines.len() > 1,
            "expected wrapping into multiple lines, got {:?}",
            lines
        );
        let has_bold = lines.iter().any(|l| l.runs.iter().any(|r| r.bold));
        assert!(has_bold, "bold run should survive wrapping: {:?}", lines);
    }

    #[test]
    fn streaming_matches_full_render_after_finish() {
        let full = "# Title\n\nSome **bold** text.\n\n- item one\n- item two\n\n> a quote\n\n```\ncode line\n```\n";
        let expected = render_document(full, DEFAULT_WRAP_COLS);

        let mut renderer = StreamingMarkdownRenderer::new(DEFAULT_WRAP_COLS);
        for chunk in [
            "# Title\n\n",
            "Some **bold** text.\n\n",
            "- item one\n- item two\n\n",
            "> a quote\n\n",
            "```\ncode line\n```\n",
        ] {
            renderer.push(chunk);
            renderer.render();
        }
        let finished = renderer.finish();
        assert_eq!(texts(&finished), texts(&expected));
    }

    #[test]
    fn streaming_freezes_all_but_last_block() {
        let mut renderer = StreamingMarkdownRenderer::new(DEFAULT_WRAP_COLS);
        renderer.push("# Title\n\nParagraph one.\n\n");
        renderer.render();
        assert_eq!(
            renderer.frozen_block_count(),
            1,
            "heading should be frozen once paragraph starts"
        );

        renderer.push("Paragraph two is still growing");
        renderer.render();
        // Heading + paragraph one are now both closed (paragraph two started).
        assert_eq!(renderer.frozen_block_count(), 2);
    }

    #[test]
    fn streaming_chunked_matches_full_char_by_char() {
        let full = "# Demo\n\nText with **bold** and *italic* words that is long enough to wrap across more than one line when using a narrow column budget.\n";
        let expected = render_document(full, 30);

        let mut renderer = StreamingMarkdownRenderer::new(30);
        for ch in full.chars() {
            renderer.push(&ch.to_string());
            renderer.render();
        }
        let finished = renderer.finish();
        assert_eq!(texts(&finished), texts(&expected));
    }

    #[test]
    fn empty_source_renders_nothing() {
        assert!(render_document("", DEFAULT_WRAP_COLS).is_empty());
    }

    // -- segment_blocks (Architecture v2 block segmentation) --

    #[test]
    fn segment_blocks_heading_and_paragraph_source_ranges_slice_back_to_the_original_text() {
        let source = "# Title\n\nHello world.\n";
        let spans = segment_blocks(source);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].heading_level, Some(1));
        // pulldown-cmark's own byte range for a block includes its
        // trailing newline -- trimmed downstream (models.rs's
        // styled_text_for) before reaching StyledText::from_markdown,
        // not here: segment_blocks reports the real span verbatim.
        assert_eq!(source[spans[0].source_range.clone()].trim_end(), "# Title");
        assert_eq!(spans[1].heading_level, None);
        assert_eq!(source[spans[1].source_range.clone()].trim_end(), "Hello world.");
    }

    #[test]
    fn segment_blocks_code_block_is_tagged_code_not_text() {
        let source = "```rust\nfn main() {}\n```\n";
        let spans = segment_blocks(source);
        assert_eq!(spans.len(), 1);
        assert!(matches!(spans[0].kind, BlockSpanKind::Code(_)));
    }

    #[test]
    fn segment_blocks_rule_is_its_own_span() {
        let source = "above\n\n---\n\nbelow\n";
        let spans = segment_blocks(source);
        assert_eq!(spans.len(), 3);
        assert!(matches!(spans[0].kind, BlockSpanKind::Text));
        assert!(matches!(spans[1].kind, BlockSpanKind::Rule));
        assert!(matches!(spans[2].kind, BlockSpanKind::Text));
    }

    #[test]
    fn segment_blocks_list_items_flatten_to_indented_text_spans() {
        let source = "- one\n- two\n";
        let spans = segment_blocks(source);
        assert_eq!(spans.len(), 2);
        for span in &spans {
            assert!(matches!(span.kind, BlockSpanKind::Text));
            assert_eq!(span.indent, 1);
        }
        assert_eq!(source[spans[0].source_range.clone()].trim_end(), "one");
        assert_eq!(source[spans[1].source_range.clone()].trim_end(), "two");
    }

    #[test]
    fn segment_blocks_blockquote_increments_indent() {
        let source = "> quoted text\n";
        let spans = segment_blocks(source);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].indent, 1);
        assert_eq!(source[spans[0].source_range.clone()].trim_end(), "quoted text");
    }

    #[test]
    fn segment_blocks_table_captures_cell_ranges_and_col_count() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let spans = segment_blocks(source);
        assert_eq!(spans.len(), 1);
        match &spans[0].kind {
            BlockSpanKind::Table { cells, col_count } => {
                assert_eq!(*col_count, 2);
                assert_eq!(cells.len(), 4); // header row (2) + body row (2)
                // Cell ranges include the table syntax's padding
                // whitespace around `|` delimiters (real pulldown-cmark
                // behavior) -- trimmed downstream the same way block
                // ranges' trailing newlines are.
                let texts: Vec<&str> = cells.iter().map(|r| source[r.clone()].trim()).collect();
                assert_eq!(texts, vec!["a", "b", "1", "2"]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn segment_blocks_empty_source_is_empty() {
        assert!(segment_blocks("").is_empty());
    }

    // -- heal_open_markers --

    #[test]
    fn heal_open_markers_leaves_commonmark_emphasis_untouched() {
        // Spike-confirmed: StyledText::from_markdown already degrades
        // unterminated **/*/`/[...] to literal text gracefully on its
        // own -- heal_open_markers must not touch these at all.
        for partial in ["Hello **wor", "Hello *wor", "Hello `wor", "Hello [wor"] {
            assert_eq!(heal_open_markers(partial), partial);
        }
    }

    #[test]
    fn heal_open_markers_closes_unterminated_u_tag() {
        assert_eq!(heal_open_markers("Hello <u>world"), "Hello <u>world</u>");
    }

    #[test]
    fn heal_open_markers_closes_unterminated_font_tag() {
        assert_eq!(
            heal_open_markers("Hello <font color=\"red\">world"),
            "Hello <font color=\"red\">world</font>"
        );
    }

    #[test]
    fn heal_open_markers_closes_nested_tags_innermost_first() {
        assert_eq!(
            heal_open_markers("<u><font color=\"red\">text"),
            "<u><font color=\"red\">text</font></u>"
        );
    }

    #[test]
    fn heal_open_markers_is_a_no_op_for_already_closed_tags() {
        let closed = "Hello <u>world</u> and <font color=\"red\">red</font> text.";
        assert_eq!(heal_open_markers(closed), closed);
    }

    #[test]
    fn heal_open_markers_leaves_plain_text_unchanged() {
        assert_eq!(heal_open_markers("just plain text"), "just plain text");
    }
}
