//! Regression coverage for native markdown blocks after a panel resize.
//!
//! Architecture-v2 markdown uses one `StyledText` per block. The element
//! must receive the row's live width; otherwise its implicit content width
//! can remain tied to the previous window size and the body appears clipped.

use i_slint_backend_testing::ElementHandle;
use panel_rust::{ChatPanel, MarkdownBlock, MessageItem};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn long_agent_message(body: &str) -> MessageItem {
    MessageItem {
        kind: "agent".into(),
        text: body.into(),
        markdown_blocks: ModelRc::new(VecModel::from(vec![MarkdownBlock {
            kind: "text".into(),
            text: slint::StyledText::from_plain_text(body),
            default_font_size: 0.0,
            indent: 0,
            table_cells: ModelRc::new(VecModel::<slint::StyledText>::default()),
            table_col_count: 0,
            code_text: SharedString::default(),
        }])),
        ..MessageItem::default()
    }
}

#[test]
fn markdown_text_block_tracks_narrower_window_width() {
    i_slint_backend_testing::init_no_event_loop();
    let panel = ChatPanel::new().expect("construct chat panel");
    panel.set_sidebar_expanded(false);
    let body = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty";
    panel.set_messages(ModelRc::new(VecModel::from(vec![long_agent_message(body)])));

    panel
        .window()
        .set_size(slint::LogicalSize::new(1400.0, 800.0));
    let styled = || {
        let matches: Vec<_> =
            ElementHandle::find_by_element_type_name(&panel, "StyledText").collect();
        assert_eq!(matches.len(), 1, "expected one markdown StyledText element");
        matches.into_iter().next().unwrap()
    };
    let wide = styled().size().width;
    let wide_height = styled().size().height;

    panel
        .window()
        .set_size(slint::LogicalSize::new(420.0, 800.0));
    let narrow = styled().size().width;
    let narrow_height = styled().size().height;
    assert!(wide > 1.0, "markdown StyledText must have a live width");
    assert!(
        narrow < wide - 50.0,
        "markdown width did not follow resize: wide={wide} narrow={narrow}"
    );
    assert!(
        narrow < 420.0,
        "markdown block overflows narrow window: {narrow}"
    );
    assert!(
        narrow_height > wide_height + 20.0,
        "markdown did not rewrap vertically: wide_height={wide_height} narrow_height={narrow_height}"
    );
}
