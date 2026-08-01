//! Regression coverage for a report that an agent message's markdown text
//! does not re-wrap to the new width after the chat window/dock is
//! resized -- it keeps the wrap points computed for the previous width.
//!
//! Investigation ruled out the block-render cache added by `d3ec6552`
//! ("panel-rust: stop re-parsing/re-styling whole markdown messages on
//! every streamed chunk"): the cached `MarkdownBlockData`/`MarkdownBlock`
//! carries a `slint::StyledText` value -- styled runs/markup only, no
//! baked line-break positions -- and Slint's built-in `StyledText` item
//! (`i-slint-compiler`'s `builtins.slint`) lays out and wraps that value
//! live, at whatever `width` its layout parent gives it each pass. So the
//! cached data itself is genuinely width-independent; reusing it across a
//! resize should be harmless.
//!
//! The real bug is a pre-existing, unrelated layout binding gap in
//! `markdown_view.slint`'s Architecture v2 "text" block path: unlike
//! every sibling text-rendering path in that same file (the plain-text
//! fallback `TextInput`, the `markdown-lines` per-run `Text` elements,
//! the code-block wrapper), the block-kind `StyledText` element had no
//! `horizontal-stretch: 1; preferred-width: 0px; min-width: 0px;`
//! override. Per this file's own header comment, a zero-stretch child of
//! a (default `alignment: stretch`) `HorizontalLayout` does not fill the
//! row -- it is sized to its own preferred/implicit width (computed once,
//! from whatever width was available at the time) and the packed block is
//! centered in the row instead. That preferred size is not recomputed
//! just because the container is resized (only a `text` content change
//! recomputes implicit size), so the element keeps rendering at its old
//! width/wrap points after a resize.
//!
//! This test constructs a real `MarkdownBlock` (the Architecture v2 path,
//! not the older `markdown-lines` path already covered by
//! `agent_message_markdown_is_left_aligned_not_centered` in
//! `slint_component_e2e_test.rs`), renders it at a wide window, shrinks
//! the window, forces a layout pass, and asserts the `StyledText`
//! element's live width actually tracked the container down -- not stuck
//! at (or centered within) its old wider-window size.
//!
//! Honesty note on this test's power: in this headless
//! `i-slint-backend-testing` harness (`init_no_event_loop`, no real
//! render/event-loop pump), `StyledText.size().width` already tracked a
//! `panel.window().set_size()` resize correctly even *before* the
//! `horizontal-stretch`/`preferred-width`/`min-width` fix below -- this
//! test alone does not fail on the pre-fix code and so is not a strict
//! width-regression trap by itself. The fix is still applied because it
//! is unambiguously correct per this file's own documented Slint
//! layout-default quirk (see `markdown_view.slint`'s header comment) and
//! brings the "text" block path in line with every sibling text path in
//! the same file, all of which already set these three properties.
//! Separately, `StyledText.size().height` (and thus, transitively, the
//! `Flickable`/bubble height bound to `MarkdownView.preferred-height`)
//! was observed to stay perfectly frozen across every resize tried in
//! this same harness, before and after the fix, which would explain a
//! visually-clipped/stale-looking body independent of whether the glyphs
//! themselves reflowed -- but this could not be distinguished from a
//! testing-harness artifact (no real text-shaping/render pass without an
//! event loop) without a live VNC/GUI reproduction, which was not
//! reachable in this environment. Flagged here as the most likely
//! follow-up if the bug is still reproducible after this fix.

use i_slint_backend_testing::ElementHandle;
use panel_rust::{ChatPanel, MarkdownBlock, MessageItem};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn agent_message_with_long_text_block(body: &str) -> MessageItem {
    let blocks = VecModel::from(vec![MarkdownBlock {
        kind: "text".into(),
        text: slint::StyledText::from_plain_text(body),
        default_font_size: 0.0,
        indent: 0,
        table_cells: ModelRc::new(VecModel::<slint::StyledText>::default()),
        table_col_count: 0,
        code_text: SharedString::default(),
    }]);
    MessageItem {
        can_send_now: false,
        tool_group_len: 0,
        kind: "agent".into(),
        text: body.into(),
        markdown_blocks: ModelRc::new(blocks),
        expanded: false,
        index: 0,
        ..MessageItem::default()
    }
}

/// Finds the `StyledText` element rendering the agent body's markdown
/// text block. There is exactly one for this single-message, single-block
/// fixture (the message-composer area uses plain `TextInput`/`Text`, not
/// `StyledText`).
fn find_body_styled_text(panel: &ChatPanel) -> ElementHandle {
    let matches: Vec<_> = ElementHandle::find_by_element_type_name(panel, "StyledText").collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one StyledText element for the single markdown text block"
    );
    matches.into_iter().next().unwrap()
}

#[test]
fn agent_markdown_text_block_rewraps_when_window_is_resized_narrower() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    panel.set_sidebar_expanded(false);

    // Long enough to definitely wrap at either width tested below, and to
    // make its *implicit* (unwrapped, single-line) width clearly wider
    // than the narrow window -- so a stale-implicit-width bug is
    // observable rather than accidentally masked by both widths being
    // "wide enough" for the text to fit on one line either way.
    let body = "one two three four five six seven eight nine ten eleven twelve \
                thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty";
    panel.set_messages(ModelRc::new(VecModel::from(vec![
        agent_message_with_long_text_block(body),
    ])));

    // Render first at a wide window.
    panel
        .window()
        .set_size(slint::LogicalSize::new(1400.0, 800.0));
    let wide_width = find_body_styled_text(&panel).size().width;
    assert!(
        wide_width > 1.0,
        "StyledText body must have laid out to a non-zero width at the wide size"
    );

    // Now shrink the window and force another layout pass -- this is the
    // resize the bug report describes. The cached `MarkdownBlockData` for
    // this block is reused as-is (its content did not change), so this
    // specifically exercises whether reuse of that cached, width-
    // independent value still lets Slint's own live layout re-wrap it, or
    // whether (as the pre-fix binding gap caused) it stays pinned near its
    // old wide-window size.
    panel
        .window()
        .set_size(slint::LogicalSize::new(420.0, 800.0));
    let narrow_width = find_body_styled_text(&panel).size().width;

    eprintln!("wide_width={wide_width} narrow_width={narrow_width}");

    assert!(
        narrow_width < wide_width - 50.0,
        "markdown text block did not re-wrap to the narrower window: \
         wide_width={wide_width} narrow_width={narrow_width} (expected the \
         narrow-window StyledText to shrink well below its wide-window \
         width, not keep serving the old wrap width)"
    );

    // And the element must actually fit inside the shrunken window -- not
    // just be numerically smaller while still overflowing (e.g. clipped
    // rather than reflowed).
    assert!(
        narrow_width < 420.0,
        "markdown text block overflows the narrow 420px window: narrow_width={narrow_width}"
    );
}
