//! Phase 4: real chat panel, wired to `rui-acp-client` for genuine
//! ACP-backed session data via [`agent_bridge::AgentBridge`] -- no more
//! static placeholders (phase 2 had layout-only, static arrays; see git
//! history / the phase-2 state doc for that snapshot). Rendered by the
//! same proven render bridge from phase 1
//! (rust-qt-cross-render-option-b.md).
//!
//! Threading note (see phase 1 finding, and `agent_bridge`'s module docs
//! for how phase 4 respects it): the whole Slint side must stay on one OS
//! thread. This process must be launched with `QSG_RENDER_LOOP=basic` so
//! Qt's paint() and input dispatch share a thread -- otherwise this
//! thread_local singleton silently forks into two never-synchronized
//! copies (confirmed the hard way in phase 1). The agent bridge's
//! background tokio runtime runs on its own worker threads but never
//! touches Slint state directly -- see `agent_bridge.rs`.

mod agent_bridge;
mod models;
mod theme;

use agent_bridge::AgentBridge;
use models::{build_thread_items, to_message_model, ThreadState};
use rui_acp_client::{AgentEvent, ChatMessage, MessageKind};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::os::raw::{c_int, c_uchar, c_uint};
use std::rc::Rc;

/// Fixed v1 set of chat threads -- each gets its own bound agent
/// connection via `AgentBridge` (Decision 4: per-thread static binding).
/// A dynamic thread list (create/rename/delete threads from the UI) is
/// follow-up work, not built here.
const THREAD_NAMES: &[&str] = &[
    "Fix timeline crash",
    "Add fade transition",
    "Refactor filters",
    "Export pipeline bug",
];

/// Maps a Qt key event (`QKeyEvent::key()`'s `int` plus `QKeyEvent::text()`)
/// to a Slint key-event `SharedString`. Qt::Key special codes below are the
/// stable `qnamespace.h` values for the handful of editing/navigation keys
/// a single-line chat compose box needs; anything else falls back to the
/// already-localized `text` Qt hands us (correct for regular printable
/// input, including non-ASCII layouts -- Qt has already done the keymap
/// work by the time `text()` is populated). Returns `None` for pure
/// modifier presses (empty text, no special mapping) which Slint doesn't
/// need forwarded as a `KeyPressed`/`KeyReleased` text event.
fn map_qt_key(qt_key: c_int, text: &str) -> Option<SharedString> {
    let special = match qt_key {
        0x0100_0000 => Some(Key::Escape),
        0x0100_0001 => Some(Key::Tab),
        0x0100_0003 => Some(Key::Backspace),
        0x0100_0004 | 0x0100_0005 => Some(Key::Return),
        0x0100_0007 => Some(Key::Delete),
        0x0100_0010 => Some(Key::Home),
        0x0100_0011 => Some(Key::End),
        0x0100_0012 => Some(Key::LeftArrow),
        0x0100_0013 => Some(Key::UpArrow),
        0x0100_0014 => Some(Key::RightArrow),
        0x0100_0015 => Some(Key::DownArrow),
        _ => None,
    };
    if let Some(k) = special {
        return Some(k.into());
    }
    if text.is_empty() {
        None
    } else {
        Some(SharedString::from(text))
    }
}

// Slint UI markup moved to `panel-rust/ui/*.slint` (Phase 1 of
// chat-panel-ui-theme-parity.md's modularity requirement) -- compiled by
// `build.rs` via `slint_build::compile`. `ChatPanel`, `ThreadItem`, and
// `MessageItem` below are the same generated Rust bindings the inline
// `slint::slint! { ... }` macro used to produce; nothing downstream in
// this file needed to change.
slint::include_modules!();

struct SpikePlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for SpikePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

struct PanelSingleton {
    window: Rc<MinimalSoftwareWindow>,
    component: ChatPanel,
    buffer: RefCell<Vec<PremultipliedRgbaColor>>,
    width: u32,
    height: u32,
    bridge: Option<AgentBridge>,
    thread_state: RefCell<Vec<ThreadState>>,
    /// Phase 2 (chat-panel-ui-theme-parity.md): current sidebar search
    /// filter, empty means "show all". `THREAD_NAMES` never changes at
    /// runtime, so this is enough state to fully rebuild the filtered
    /// model on every keystroke -- see `refresh_threads_model`.
    search_query: RefCell<String>,
    /// Maps each currently-*visible* (post-filter) row index back to its
    /// real index into `THREAD_NAMES`/`thread_state`/the agent bridge --
    /// filtering means `threads[i]` in Slint is no longer the same `i` as
    /// `bridge.history(i)`; every Rust-side handler that receives a
    /// `selected-thread`/`thread-selected(i)` value from Slint must
    /// translate it through this map first (`real_index`). Rebuilt by
    /// `refresh_threads_model` every time the filter or thread_state
    /// changes.
    visible_indices: RefCell<Vec<usize>>,
}

impl PanelSingleton {
    /// Rebuilds and pushes the `threads` model from `THREAD_NAMES` +
    /// current `thread_state`, narrowed by `search_query` (Phase 2's
    /// real client-side filter -- see `models::build_thread_items`).
    /// Called any time a thread's status changes (send in flight, turn
    /// ended, error) or the search box is edited.
    fn refresh_threads_model(&self) {
        let state = self.thread_state.borrow();
        let query = self.search_query.borrow();
        let items = build_thread_items(THREAD_NAMES, &state, &query);
        *self.visible_indices.borrow_mut() = items.iter().map(|i| i.real_index).collect();
        let items: Vec<ThreadItem> = items.into_iter().map(|i| i.item).collect();
        self.component.set_threads(ModelRc::new(VecModel::from(items)));
    }

    /// Translates a Slint-side filtered-list index (what `thread-selected`
    /// callbacks and `get_selected_thread()` hand back) into the real
    /// index the agent bridge/`thread_state` use. `None` if out of range
    /// (e.g. the filter just emptied the list out from under a stale
    /// selection).
    fn real_index(&self, filtered_idx: usize) -> Option<usize> {
        self.visible_indices.borrow().get(filtered_idx).copied()
    }

    /// Applies queued agent-bridge events to `thread_state` and, if the
    /// currently-selected thread is affected, refreshes the visible
    /// `messages` model too. Returns whether anything visibly changed.
    fn apply_bridge_events(&self) -> bool {
        let Some(bridge) = &self.bridge else {
            return false;
        };
        let events = bridge.poll();
        if events.is_empty() {
            return false;
        }
        // `selected-thread` is a *filtered-list* index (Phase 2) --
        // translate to the real thread index before comparing against
        // `ev.thread_index`, which always refers to the real index the
        // agent bridge knows about.
        let selected = self.real_index(self.component.get_selected_thread() as usize);
        let mut selected_touched = false;
        {
            let mut state = self.thread_state.borrow_mut();
            for ev in &events {
                let idx = ev.thread_index;
                if Some(idx) == selected {
                    selected_touched = true;
                }
                if let Some(slot) = state.get_mut(idx) {
                    match &ev.event {
                        AgentEvent::Message(_) => {} // status unchanged while streaming
                        AgentEvent::TurnEnded(_) => *slot = ThreadState::Idle,
                        AgentEvent::Error(_) => *slot = ThreadState::Error,
                    }
                }
            }
        }
        self.refresh_threads_model();
        if let (true, Some(selected)) = (selected_touched, selected) {
            self.component.set_messages(to_message_model(bridge.history(selected)));
        }
        true
    }
}

thread_local! {
    static PANEL: RefCell<Option<PanelSingleton>> = const { RefCell::new(None) };
}

pub struct PanelHandle {
    _private: (),
}

static SENTINEL: PanelHandle = PanelHandle { _private: () };

/// Create (or resize, if already created) the process's single panel
/// instance. See module docs: must only be called from one OS thread, and
/// this process must run with `QSG_RENDER_LOOP=basic`.
#[no_mangle]
pub extern "C" fn panel_rust_create(width: c_uint, height: c_uint) -> *mut PanelHandle {
    PANEL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(existing) = slot.as_mut() {
            if existing.width != width || existing.height != height {
                existing.window.set_size(slint::PhysicalSize::new(width, height));
                existing
                    .buffer
                    .replace(vec![
                        PremultipliedRgbaColor { red: 0, green: 0, blue: 0, alpha: 0 };
                        (width * height) as usize
                    ]);
                existing.width = width;
                existing.height = height;
                existing.window.window().request_redraw();
            }
            return &SENTINEL as *const PanelHandle as *mut PanelHandle;
        }

        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::set_platform(Box::new(SpikePlatform {
            window: window.clone(),
        }))
        .expect("panel-rust: set_platform must only be called once per process");

        let component = match ChatPanel::new() {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        window.window().request_redraw();

        // Bridge init failure degrades gracefully rather than aborting
        // panel creation -- the UI still renders (thread list marked
        // "error", compose box becomes a no-op) instead of Shotcut losing
        // the whole dock over a missing/misconfigured agent binary. See
        // `agent_bridge::resolve_agent_command` for how the command is
        // chosen (RUI_ACP_AGENT_CMD env override, else the dev-checkout
        // rui-mock-agent path).
        let (bridge, initial_state) = match AgentBridge::new(THREAD_NAMES) {
            Ok(b) => (Some(b), vec![ThreadState::Idle; THREAD_NAMES.len()]),
            Err(e) => {
                eprintln!("panel-rust: agent bridge unavailable, chat panel is display-only: {e}");
                (None, vec![ThreadState::Error; THREAD_NAMES.len()])
            }
        };

        let panel = PanelSingleton {
            window,
            component,
            buffer: RefCell::new(vec![
                PremultipliedRgbaColor { red: 0, green: 0, blue: 0, alpha: 0 };
                (width * height) as usize
            ]),
            width,
            height,
            bridge,
            thread_state: RefCell::new(initial_state),
            search_query: RefCell::new(String::new()),
            visible_indices: RefCell::new(Vec::new()),
        };
        panel.refresh_threads_model();
        if !THREAD_NAMES.is_empty() {
            if let Some(bridge) = &panel.bridge {
                panel.component.set_messages(to_message_model(bridge.history(0)));
            }
        }

        let component_weak = panel.component.as_weak();
        panel.component.on_thread_selected(move |idx| {
            let Some(component) = component_weak.upgrade() else { return };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    // `idx` is a filtered-list index (Phase 2) -- translate
                    // to the real thread index before touching the bridge.
                    let Some(real_idx) = panel.real_index(idx as usize) else { return };
                    if let Some(bridge) = &panel.bridge {
                        component.set_messages(to_message_model(bridge.history(real_idx)));
                    }
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_send_requested(move || {
            let Some(component) = component_weak.upgrade() else { return };
            let text = component.get_compose_text().to_string();
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let filtered_idx = component.get_selected_thread() as usize;
            component.set_compose_text("".into());
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(filtered_idx) else { return };
                    let Some(bridge) = &panel.bridge else { return };
                    bridge.push_local(idx, ChatMessage { kind: MessageKind::User, text: text.to_string() });
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        *slot = ThreadState::Loading;
                    }
                    panel.refresh_threads_model();
                    if Some(idx) == panel.real_index(component.get_selected_thread() as usize) {
                        component.set_messages(to_message_model(bridge.history(idx)));
                    }
                    bridge.send_prompt(idx, text.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_search_changed(move |query| {
            let Some(component) = component_weak.upgrade() else { return };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    *panel.search_query.borrow_mut() = query.to_string();
                    panel.refresh_threads_model();
                    // The filter can move/remove the previously-selected
                    // row entirely -- reset to the first still-visible
                    // thread (Phase 2 UX decision, documented in the
                    // theme-parity plan's Phase 2 section) rather than
                    // leaving a stale/out-of-range selection.
                    component.set_selected_thread(0);
                    match panel.real_index(0) {
                        Some(real_idx) => {
                            if let Some(bridge) = &panel.bridge {
                                component.set_messages(to_message_model(bridge.history(real_idx)));
                            }
                        }
                        None => component.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new()))),
                    }
                }
            });
        });

        *slot = Some(panel);
        &SENTINEL as *const PanelHandle as *mut PanelHandle
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_destroy(_handle: *mut PanelHandle) {}

/// Forward a click at physical pixel coordinates, as a press+release pair.
#[no_mangle]
pub extern "C" fn panel_rust_input_click(_handle: *mut PanelHandle, x: c_uint, y: c_uint) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let pos = slint::LogicalPosition::new(x as f32, y as f32);
        let win = panel.window.window();
        win.dispatch_event(WindowEvent::PointerMoved { position: pos });
        win.dispatch_event(WindowEvent::PointerPressed {
            position: pos,
            button: PointerEventButton::Left,
        });
        win.dispatch_event(WindowEvent::PointerReleased {
            position: pos,
            button: PointerEventButton::Left,
        });
        true
    })
}

/// Forward a keyboard event -- `qt_key` is `QKeyEvent::key()`, `text` is
/// `QKeyEvent::text()` UTF-8 encoded (may be empty for pure modifiers).
/// See `map_qt_key` for the Qt -> Slint key mapping. Needed for the chat
/// compose box (`TextInput` in the markup above); clicking into it via
/// `panel_rust_input_click` already gives it focus the same way any Slint
/// app would.
#[no_mangle]
pub extern "C" fn panel_rust_input_key(
    _handle: *mut PanelHandle,
    qt_key: c_int,
    text_ptr: *const c_uchar,
    text_len: usize,
    pressed: bool,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let text = if text_ptr.is_null() || text_len == 0 {
            ""
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len) };
            std::str::from_utf8(bytes).unwrap_or("")
        };
        let Some(key_text) = map_qt_key(qt_key, text) else {
            return false;
        };
        let win = panel.window.window();
        if pressed {
            win.dispatch_event(WindowEvent::KeyPressed { text: key_text });
        } else {
            win.dispatch_event(WindowEvent::KeyReleased { text: key_text });
        }
        true
    })
}

/// Sets the theme variant ("dark"/"light"/anything else treated as dark),
/// per `MainWindow::changeTheme()`'s resolved theme name -- see
/// `ChatRustDock::applyTheme` on the C++ side. Returns whether the panel
/// exists to apply it to.
#[no_mangle]
pub extern "C" fn panel_rust_set_theme(
    _handle: *mut PanelHandle,
    text_ptr: *const c_uchar,
    text_len: usize,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let text = if text_ptr.is_null() || text_len == 0 {
            "dark"
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len) };
            std::str::from_utf8(bytes).unwrap_or("dark")
        };
        Palette::get(&panel.component).set_theme(text.into());
        true
    })
}

/// Drains any pending agent-bridge events (streamed message chunks,
/// turn-end, errors) and applies them to the Slint model. Must be called
/// periodically from the C++ side (a `QTimer`, see `ChatRustDock`) since
/// nothing else drives the single-threaded Slint/Qt loop to notice
/// background agent activity -- see `agent_bridge` module docs. Returns
/// whether anything changed (caller should then call
/// `panel_rust_render` + trigger a Qt repaint).
#[no_mangle]
pub extern "C" fn panel_rust_poll(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        panel.apply_bridge_events()
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_render(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let width = panel.width;
        panel.window.draw_if_needed(|renderer| {
            let mut buffer = panel.buffer.borrow_mut();
            renderer.render(&mut buffer, width as usize);
        })
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_buffer_ptr(_handle: *mut PanelHandle) -> *const c_uchar {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        match slot.as_ref() {
            Some(panel) => panel.buffer.borrow().as_ptr() as *const c_uchar,
            None => std::ptr::null(),
        }
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_buffer_len(_handle: *mut PanelHandle) -> usize {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        match slot.as_ref() {
            Some(panel) => panel.buffer.borrow().len() * std::mem::size_of::<PremultipliedRgbaColor>(),
            None => 0,
        }
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_width(_handle: *mut PanelHandle) -> c_uint {
    PANEL.with(|cell| cell.borrow().as_ref().map(|p| p.width).unwrap_or(0))
}

#[no_mangle]
pub extern "C" fn panel_rust_height(_handle: *mut PanelHandle) -> c_uint {
    PANEL.with(|cell| cell.borrow().as_ref().map(|p| p.height).unwrap_or(0))
}
