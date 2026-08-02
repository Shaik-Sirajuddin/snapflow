//! Headless geometry verification for the collapsed sidebar rail's Settings
//! icon centering, following up on a report that `629e1f9a` ("sidebar:
//! convert Settings footer to layout-flow positioning") regressed it.
//!
//! `629e1f9a` wrapped `settings-footer`'s three conditional rows
//! (skills-config, expanded settings row, collapsed icon) in one shared
//! `VerticalLayout { padding: root.settings-margin; ... }`. That applied
//! `settings-margin` (20px in compact mode) as *horizontal* padding to the
//! collapsed icon row too -- but the pre-`629e1f9a` code never inset the
//! collapsed icon horizontally at all (`x: (parent.width - self.width) /
//! 2` against the full-width `settings-footer` Rectangle). With the rail
//! at 48px wide, subtracting `2 * settings-margin` (40px) left only 8px of
//! "available" width for a 28px icon, so centering degenerated to sit flush
//! against the left inset instead of the rail's true center.
//!
//! Fixed by splitting the outer `VerticalLayout`'s padding into
//! vertical-only (`padding-top`/`padding-bottom` + `spacing`), with the
//! horizontal `settings-margin` inset applied only via per-row
//! `HorizontalLayout` wrappers around skills-config/expanded-settings --
//! the collapsed icon row is deliberately left unwrapped so it centers
//! against the row's full available (rail) width.
//!
//! Uses the same `i_slint_backend_testing::ElementHandle` geometry APIs as
//! `sidebar_status_icon_centering_geometry_test.rs`.

use i_slint_backend_testing::ElementHandle;
use panel_rust::ChatPanel;
use slint::ComponentHandle;

/// Collapsed rail's own declared width (`sidebar.slint`:
/// `width: expanded ? (...) : 48px;`).
const RAIL_WIDTH: f32 = 48.0;

#[test]
fn collapsed_settings_icon_is_centered_in_the_rail() {
    i_slint_backend_testing::init_no_event_loop();

    let panel = ChatPanel::new().expect("construct chat panel");
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

    // The collapsed Settings row's icon button: `if !expanded : VerticalLayout
    // { ... HorizontalLayout { alignment: center; IconButton { btn-width:
    // 28px; btn-height: 28px; button-accessible-label: "Open settings"; ...
    // } } }` in sidebar.slint's settings-footer. It's the only 28x28
    // IconButton in the collapsed sidebar (the header collapse-toggle
    // IconButton defaults to 24x24), so match on that distinguishing size.
    let icon = sidebar
        .query_descendants()
        .match_type_name("IconButton")
        .find_all()
        .into_iter()
        .find(|el| (el.size().width - 28.0).abs() < 0.5 && (el.size().height - 28.0).abs() < 0.5)
        .expect("28x28 collapsed Settings IconButton must exist");

    let icon_center_x = icon.absolute_position().x + icon.size().width / 2.0;
    let rail_center_x = sidebar_x + sidebar_w / 2.0;
    let offset = icon_center_x - rail_center_x;

    eprintln!(
        "sidebar_x={sidebar_x} sidebar_w={sidebar_w} rail_center_x={rail_center_x} \
         icon_x={} icon_w={} icon_center_x={icon_center_x} offset={offset}",
        icon.absolute_position().x,
        icon.size().width,
    );

    // Allow half a pixel of float slop; anything beyond that is a real
    // mis-centering regression, not rounding noise.
    assert!(
        offset.abs() < 0.5,
        "collapsed Settings icon is not centered in the rail: offset={offset}px \
         (icon_center_x={icon_center_x}, rail_center_x={rail_center_x})"
    );
}
