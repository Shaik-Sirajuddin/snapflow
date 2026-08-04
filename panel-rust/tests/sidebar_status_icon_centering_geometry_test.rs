//! Headless geometry verification for the collapsed sidebar rail's
//! status-icon centering, following up on a report that `5b3cd157`
//! ("sidebar_thread_row: fix collapsed status-icon centering") had
//! regressed again.
//!
//! Uses the same `i_slint_backend_testing::ElementHandle` geometry APIs
//! (`absolute_position()`/`size()`) as the interactive-click coverage in
//! `slint_component_e2e_test.rs`, but measures real pixel geometry instead
//! of just invoking callbacks.

use i_slint_backend_testing::ElementHandle;
use panel_rust::{ChatPanel, ThreadItem};
use slint::ComponentHandle;

fn thread_item(name: &str) -> ThreadItem {
    ThreadItem {
        name: name.into(),
        status: "idle".into(),
        busy: false,
        open: true,
        background: false,
        description: "".into(),
        closed: false,
        archived: false,
        unread: false,
        provider: "".into(),
        model: "".into(),
        project_name: "".into(),
        project_path: "".into(),
        project_instance_live: false,
        profile_name: "".into(),
        has_session: false,
        relative_time: "now".into(),
    }
}

/// Collapsed rail's own declared width (`sidebar.slint`:
/// `width: expanded ? (...) : 48px;`).
const RAIL_WIDTH: f32 = 48.0;

#[test]
fn collapsed_status_dot_is_centered_in_the_rail() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
    panel
        .set_threads(std::rc::Rc::new(slint::VecModel::from(vec![thread_item("Thread A")])).into());
    panel.set_selected_thread(0);
    // Sidebar starts collapsed by default; assert that explicitly so this
    // test fails loudly (not silently) if that default ever changes.
    assert!(
        !panel.get_sidebar_expanded(),
        "expected sidebar to start collapsed"
    );

    // Force a layout pass against the real window size.
    let size = panel.window().size();
    panel.window().set_size(size);

    let sidebar = ElementHandle::find_by_element_type_name(&panel, "Sidebar")
        .next()
        .expect("Sidebar element must exist");
    let sidebar_x = sidebar.absolute_position().x;
    let sidebar_w = sidebar.size().width;
    assert!(
        (sidebar_w - RAIL_WIDTH).abs() < 1.0,
        "sidebar rail width drifted from the expected collapsed 48px: got {sidebar_w}"
    );

    // The row's status dot: `if !row-busy && thread.status != "error" :
    // Rectangle { width: 6px; Rectangle { width: 6px; height: 6px; ... } }`
    // in sidebar_thread_row.slint. Grab the innermost 6x6 dot by type name
    // uniqueness within the row -- there's only one visible status glyph
    // per row for an idle, non-error thread.
    let rows =
        ElementHandle::find_by_element_type_name(&panel, "SidebarThreadRow").collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "expected exactly one thread row");
    let row = &rows[0];

    let dot = row
        .query_descendants()
        .match_type_name("Rectangle")
        .find_all()
        .into_iter()
        .find(|el| (el.size().width - 6.0).abs() < 0.5 && (el.size().height - 6.0).abs() < 0.5)
        .expect("status dot (6x6 Rectangle) must exist in the collapsed row");

    let dot_center_x = dot.absolute_position().x + dot.size().width / 2.0;
    let rail_center_x = sidebar_x + sidebar_w / 2.0;
    let offset = dot_center_x - rail_center_x;

    eprintln!(
        "sidebar_x={sidebar_x} sidebar_w={sidebar_w} rail_center_x={rail_center_x} \
         dot_x={} dot_w={} dot_center_x={dot_center_x} offset={offset}",
        dot.absolute_position().x,
        dot.size().width,
    );

    // Allow half a pixel of float slop; anything beyond that is a real
    // mis-centering regression, not rounding noise.
    assert!(
        offset.abs() < 0.5,
        "collapsed status dot is not centered in the rail: offset={offset}px \
         (dot_center_x={dot_center_x}, rail_center_x={rail_center_x})"
    );
}
