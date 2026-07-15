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
mod appearance;
mod conversation;
mod models;
mod permission;
mod state_store;
mod theme;

use agent_bridge::{resolve_cache_dir, AgentBridge};
use appearance::{AppearanceState, ColorScheme, HostAppearance};
use models::{build_thread_items, describe_thread, to_message_model, ThreadState};
use rui_acp_client::{AgentEvent, ChatMessage, MessageKind};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ModelRc, SharedString, VecModel};
use state_store::{PanelDefaults, PanelStateStore};
use std::cell::{Cell, RefCell};
use std::os::raw::{c_int, c_uchar, c_uint};
use std::rc::Rc;

/// Truncation length for `models::describe_thread`'s sidebar preview --
/// matches the HTML source's short one-liners (e.g. "Trim clips and add
/// fades…").
const THREAD_DESCRIPTION_MAX_CHARS: usize = 48;

/// Fixed v1 set of chat threads -- each gets its own bound agent
/// connection via `AgentBridge` (Decision 4: per-thread static binding).
/// A dynamic thread list (create/rename/delete threads from the UI) is
/// follow-up work, not built here.
const DEFAULT_THREAD_NAMES: &[&str] = &[
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

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
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
    panel_state: Option<PanelStateStore>,
    appearance: RefCell<AppearanceState>,
    thread_names: RefCell<Vec<String>>,
    thread_state: RefCell<Vec<ThreadState>>,
    thread_errors: RefCell<Vec<String>>,
    /// Phase 2 (chat-panel-ui-theme-parity.md): current sidebar search
    /// filter, empty means "show all".
    search_query: RefCell<String>,
    /// Maps each currently-*visible* (post-filter) row index back to its
    /// real index into `thread_names`/`thread_state`/the agent bridge --
    /// filtering means `threads[i]` in Slint is no longer the same `i` as
    /// `bridge.history(i)`; every Rust-side handler that receives a
    /// `selected-thread`/`thread-selected(i)` value from Slint must
    /// translate it through this map first (`real_index`). Rebuilt by
    /// `refresh_threads_model` every time the filter or thread_state
    /// changes.
    visible_indices: RefCell<Vec<usize>>,
    /// Phase 3 (chat-panel-ui-theme-parity.md): UI-only collapse state
    /// for tool-call log bodies, parallel to whichever thread's messages
    /// are currently displayed -- see `refresh_messages_for`/
    /// `render_messages` below. Does not persist across a thread switch
    /// or a jsonl reload (render concern only, not part of
    /// `ChatMessage`).
    expanded: RefCell<Vec<bool>>,
    /// The real thread index whose history `expanded` currently
    /// describes -- `None` before the first message render. Used to
    /// decide whether switching to `real_idx` should reset `expanded`
    /// (different thread) or just grow it in place (same thread, new
    /// streamed messages).
    displayed_thread: Cell<Option<usize>>,
    /// Terminal-view addition: which terminal id (if any) the floating
    /// overlay is currently showing -- `None` means closed. Set by the
    /// `expand-terminal` callback, cleared by `close-terminal-overlay`;
    /// re-read every refresh so the overlay keeps showing live output
    /// while open (same "Rust owns the source of truth, Slint property
    /// is just a snapshot" convention `refresh_pending_request_for`
    /// already follows).
    expanded_terminal_id: RefCell<Option<String>>,
}

impl PanelSingleton {
    /// Rebuilds and pushes the `threads` model from the dynamic thread list +
    /// current `thread_state`, narrowed by `search_query` (Phase 2's
    /// real client-side filter -- see `models::build_thread_items`).
    /// Called any time a thread's status changes (send in flight, turn
    /// ended, error) or the search box is edited.
    fn refresh_threads_model(&self) {
        let state = self.thread_state.borrow();
        let query = self.search_query.borrow();
        // Phase 3: sidebar description is synthesized from each
        // thread's latest cached message (`models::describe_thread`) --
        // recomputed here rather than cached, since it must track the
        // live/bridge history, not just `thread_state`.
        let names = self.thread_names.borrow();
        let errors = self.thread_errors.borrow();
        let descriptions: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if let Some(error) = errors.get(i).filter(|error| !error.is_empty()) {
                    return format!("Error: {error}");
                }
                match &self.bridge {
                    Some(bridge) => {
                        describe_thread(&bridge.history(i), THREAD_DESCRIPTION_MAX_CHARS)
                    }
                    None => String::new(),
                }
            })
            .collect();
        let items = build_thread_items(&*names, &state, &descriptions, &query);
        *self.visible_indices.borrow_mut() = items.iter().map(|i| i.real_index).collect();
        let items: Vec<ThreadItem> = items.into_iter().map(|i| i.item).collect();
        self.component
            .set_threads(ModelRc::new(VecModel::from(items)));
    }

    /// Translates a Slint-side filtered-list index (what `thread-selected`
    /// callbacks and `get_selected_thread()` hand back) into the real
    /// index the agent bridge/`thread_state` use. `None` if out of range
    /// (e.g. the filter just emptied the list out from under a stale
    /// selection).
    fn real_index(&self, filtered_idx: usize) -> Option<usize> {
        self.visible_indices.borrow().get(filtered_idx).copied()
    }

    /// Rebuilds the `messages` model for `real_idx` from the agent
    /// bridge's current history plus whatever `expanded` state already
    /// exists -- does not touch `expanded`/`displayed_thread` itself
    /// (that's `refresh_messages_for`'s job). Used by the
    /// `toggle-expanded` callback, which only flips one bool and must
    /// not reset collapse state for every other message in the thread.
    fn render_messages(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let history = bridge.history(real_idx);
        let expanded = self.expanded.borrow();
        self.component
            .set_messages(to_message_model(history, &expanded));
    }

    /// Displays `real_idx`'s messages, first reconciling `expanded`
    /// against it: a genuine thread switch (different from
    /// `displayed_thread`) resets collapse state to all-collapsed
    /// (matches the HTML source's "new tool_use items default to
    /// collapsed" convention); staying on the same thread (e.g. a
    /// streamed message just arrived, growing history by one) only
    /// grows the vec, preserving whatever the user already
    /// expanded/collapsed. Every Rust-side call site that changes which
    /// thread's messages are visible goes through this, not
    /// `set_messages` directly.
    fn refresh_messages_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let history_len = bridge.history(real_idx).len();
        let is_thread_switch = self.displayed_thread.get() != Some(real_idx);
        {
            let mut expanded = self.expanded.borrow_mut();
            if is_thread_switch {
                *expanded = vec![false; history_len];
            } else if expanded.len() < history_len {
                expanded.resize(history_len, false);
            }
        }
        self.displayed_thread.set(Some(real_idx));
        self.render_messages(real_idx);
        self.refresh_pending_request_for(real_idx);
        self.refresh_terminals_for(real_idx);
        self.refresh_capabilities_for(real_idx);
    }

    /// Rebuilds the `available-modes`/`current-mode-id`/`config-option-
    /// rows` properties for `real_idx` from `AgentBridge::session_modes`/
    /// `config_options` -- see [`Self::refresh_terminals_for`]'s doc
    /// comment for the shared "this thread became the displayed one"
    /// hook convention this follows. Both a genuine `session/new`-time
    /// advertisement and any later live `current_mode_update`/`config_
    /// option_update` notification reach the UI purely by this being
    /// re-called on every event that touches the selected thread (see
    /// `apply_bridge_events`'s `AgentEvent::SessionModes`/`Current
    /// ModeChanged`/`ConfigOptions` arms).
    fn refresh_capabilities_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let modes = bridge.session_modes(real_idx);
        let current_mode_id = modes
            .as_ref()
            .map(|m| m.current_mode_id.clone())
            .unwrap_or_default();
        self.component
            .set_available_modes(models::to_mode_options(modes));
        self.component.set_current_mode_id(current_mode_id.into());
        self.component
            .set_config_option_rows(models::to_config_option_rows(bridge.config_options(real_idx)));
    }

    /// Rebuilds the `pending-request` property for `real_idx` from the
    /// agent bridge's current pending-request queue -- the request-card
    /// component's whole visibility/content is driven from this, not a
    /// separate boolean flag. Called from [`Self::refresh_messages_for`]
    /// (the shared "this thread became the displayed one" hook, covering
    /// both a genuine thread switch and a same-thread event refresh) so
    /// there's exactly one place that decides which thread's request (if
    /// any) is currently visible.
    fn refresh_pending_request_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let pending = bridge.pending_requests(real_idx);
        let item = match pending.first() {
            Some(event) => {
                let view = permission::to_pending_request_view(event);
                PendingRequestItem {
                    active: true,
                    relay_id: view.relay_id.into(),
                    method: view.method.into(),
                    title: view.title.into(),
                    summary: view.summary.into(),
                    supported: permission::is_supported_method(&event.method),
                }
            }
            None => PendingRequestItem {
                active: false,
                relay_id: SharedString::default(),
                method: SharedString::default(),
                title: SharedString::default(),
                summary: SharedString::default(),
                supported: false,
            },
        };
        self.component.set_pending_request(item);
    }

    /// Rebuilds the `terminals` row model for `real_idx` from the agent
    /// bridge's current terminal registry (`active_terminals` +
    /// `terminal_buffer`, see `agent_bridge.rs`) and, if the floating
    /// overlay is currently showing one of this thread's terminals,
    /// refreshes `expanded-terminal` too so live output keeps streaming
    /// into an already-open overlay. Called from
    /// [`Self::refresh_messages_for`] (the shared "this thread became
    /// the displayed one" hook) so terminal cards stay in sync with
    /// every event that touches the selected thread, same convention
    /// [`Self::refresh_pending_request_for`] follows.
    fn refresh_terminals_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let ids = bridge.active_terminals(real_idx);
        let entries: Vec<(String, Option<agent_bridge::TerminalBuffer>)> = ids
            .into_iter()
            .map(|id| {
                let buffer = bridge.terminal_buffer(real_idx, &id);
                (id, buffer)
            })
            .collect();
        let expanded_id = self.expanded_terminal_id.borrow().clone();
        if let Some(expanded_id) = &expanded_id {
            if let Some(buffer) = bridge.terminal_buffer(real_idx, expanded_id) {
                self.component.set_expanded_terminal(TerminalItem {
                    terminal_id: expanded_id.clone().into(),
                    output: buffer.output.into(),
                    truncated: buffer.truncated,
                    has_exited: buffer.exit_status.is_some(),
                    exit_code: buffer
                        .exit_status
                        .and_then(|(code, _signal)| code)
                        .unwrap_or_default(),
                });
            }
        }
        self.component
            .set_terminals(models::to_terminal_items(entries));
    }

    /// Answers the currently-displayed thread's first pending request
    /// with `approved`, then immediately re-renders the request card
    /// (which will hide it, since `AgentBridge::respond_to_request`
    /// removes the entry synchronously -- see that method's doc
    /// comment) so the UI reflects the decision without waiting for the
    /// next `apply_bridge_events` poll tick.
    fn answer_pending_request(&self, component: &ChatPanel, approved: bool) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(component.get_selected_thread() as usize) else {
            return;
        };
        let pending = bridge.pending_requests(real_idx);
        let Some(event) = pending.first() else { return };
        let response = permission::build_response(event, approved);
        bridge.respond_to_request(real_idx, &event.relay_id, response);
        self.refresh_pending_request_for(real_idx);
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
                        AgentEvent::TurnEnded(_) => {
                            *slot = ThreadState::Idle;
                            if let Some(error) = self.thread_errors.borrow_mut().get_mut(idx) {
                                error.clear();
                            }
                        }
                        AgentEvent::Error(error) => {
                            *slot = ThreadState::Error;
                            if let Some(slot_error) = self.thread_errors.borrow_mut().get_mut(idx) {
                                *slot_error = error.clone();
                            }
                        }
                        // Rendering itself is driven by re-reading
                        // `AgentBridge::pending_requests(idx)` below (see
                        // `refresh_pending_request_for`), same
                        // "event just signals staleness, state is
                        // re-read from the bridge's own source of
                        // truth" convention `Message` already follows
                        // for history -- this arm only needs to make
                        // sure `selected_touched` covers a request
                        // arriving on the currently-selected thread,
                        // which the loop above already does via `idx`.
                        AgentEvent::PermissionRequest(_) => {}
                        // Same "re-read the bridge's own source of
                        // truth" convention -- `AgentBridge::
                        // terminal_buffer` is what a future terminal
                        // view component would poll; this arm exists
                        // only so the match stays exhaustive and
                        // `selected_touched` covers a terminal thread's
                        // events like every other variant.
                        AgentEvent::TerminalOutput(_) => {}
                        // Same "re-read the bridge's own source of
                        // truth" convention -- `AgentBridge::
                        // session_modes`/`config_options` are what the
                        // settings-sheet mode/config selector polls;
                        // these arms exist only so the match stays
                        // exhaustive and `selected_touched` covers a
                        // capability-advertisement event on the
                        // currently-selected thread like every other
                        // variant.
                        AgentEvent::SessionModes(_)
                        | AgentEvent::CurrentModeChanged(_)
                        | AgentEvent::ConfigOptions(_) => {}
                    }
                }
            }
        }
        self.refresh_threads_model();
        if let (true, Some(selected)) = (selected_touched, selected) {
            self.refresh_messages_for(selected);
        }
        true
    }
}

thread_local! {
    static PANEL: RefCell<Option<PanelSingleton>> = const { RefCell::new(None) };
    // Slint permits one global platform per process. Keep the software
    // window alive across Qt item recreation so a later panel can reuse the
    // already-installed platform instead of calling set_platform again.
    static PLATFORM_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
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
                existing
                    .window
                    .set_size(slint::PhysicalSize::new(width, height));
                existing.buffer.replace(vec![
                    PremultipliedRgbaColor {
                        red: 0,
                        green: 0,
                        blue: 0,
                        alpha: 0
                    };
                    (width * height) as usize
                ]);
                existing.width = width;
                existing.height = height;
                existing.component.set_compact(width < 320);
                existing.window.window().request_redraw();
            }
            return &SENTINEL as *const PanelHandle as *mut PanelHandle;
        }

        let window = PLATFORM_WINDOW.with(|platform_window| {
            let mut platform_window = platform_window.borrow_mut();
            if let Some(window) = platform_window.as_ref() {
                return window.clone();
            }
            let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            slint::platform::set_platform(Box::new(SpikePlatform {
                window: window.clone(),
            }))
            .expect("panel-rust: set_platform must only be called once per process");
            *platform_window = Some(window.clone());
            window
        });
        window.set_size(slint::PhysicalSize::new(width, height));

        let component = match ChatPanel::new() {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        component.set_compact(width < 320);
        window.window().request_redraw();

        // Bridge init failure degrades gracefully rather than aborting
        // panel creation -- the UI still renders (thread list marked
        // "error", compose box becomes a no-op) instead of Shotcut losing
        // the whole dock over a missing/misconfigured agent binary. See
        // `agent_bridge::provision_gateway` and
        // `resolve_acpx_server_bin` determine how each thread's
        // acpx-gateway connection is chosen
        // (RUI_ACPX_<PROVIDER>_URL env override, else a local
        // dev-checkout `acpx-server` auto-spawned against
        // RUI_ACP_AGENT_CMD/the dev-checkout rui-mock-agent path).
        let initial_names: Vec<String> = DEFAULT_THREAD_NAMES
            .iter()
            .map(|name| (*name).into())
            .collect();
        let initial_name_refs: Vec<&str> = initial_names.iter().map(String::as_str).collect();
        let (bridge, initial_state) = match AgentBridge::new(&initial_name_refs) {
            Ok(b) => (Some(b), vec![ThreadState::Idle; initial_names.len()]),
            Err(e) => {
                eprintln!("panel-rust: agent bridge unavailable, chat panel is display-only: {e}");
                (None, vec![ThreadState::Error; initial_names.len()])
            }
        };
        let panel_state = {
            let path = resolve_cache_dir().join("panel-state.sqlite3");
            match PanelStateStore::open(path) {
                Ok(store) => Some(store),
                Err(error) => {
                    eprintln!("panel-rust: panel settings persistence unavailable: {error}");
                    None
                }
            }
        };

        let panel = PanelSingleton {
            window,
            component,
            buffer: RefCell::new(vec![
                PremultipliedRgbaColor {
                    red: 0,
                    green: 0,
                    blue: 0,
                    alpha: 0
                };
                (width * height) as usize
            ]),
            width,
            height,
            bridge,
            panel_state,
            appearance: RefCell::new(AppearanceState::default()),
            thread_names: RefCell::new(initial_names),
            thread_state: RefCell::new(initial_state),
            thread_errors: RefCell::new(Vec::new()),
            search_query: RefCell::new(String::new()),
            visible_indices: RefCell::new(Vec::new()),
            expanded: RefCell::new(Vec::new()),
            displayed_thread: Cell::new(None),
            expanded_terminal_id: RefCell::new(None),
        };
        panel.refresh_threads_model();
        if let Some(store) = panel.panel_state.as_ref() {
            if let Ok(defaults) = store.defaults() {
                panel.component.set_default_profile(
                    defaults.profile_name.unwrap_or_default().into(),
                );
                panel.component.set_permission_profile(
                    defaults.permission_profile.unwrap_or_default().into(),
                );
                panel.component
                    .set_background_default(defaults.background_session);
            }
        }
        if !panel.thread_names.borrow().is_empty() {
            panel.refresh_messages_for(0);
        }

        let component_weak = panel.component.as_weak();
        panel.component.on_thread_selected(move |idx| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    // `idx` is a filtered-list index (Phase 2) -- translate
                    // to the real thread index before touching the bridge.
                    let Some(real_idx) = panel.real_index(idx as usize) else {
                        return;
                    };
                    panel.refresh_messages_for(real_idx);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    if let Some(store) = panel.panel_state.as_ref() {
                        if let Ok(defaults) = store.defaults() {
                            component.set_default_profile(
                                defaults.profile_name.unwrap_or_default().into(),
                            );
                            component.set_permission_profile(
                                defaults.permission_profile.unwrap_or_default().into(),
                            );
                           component.set_background_default(defaults.background_session);
                       }
                   }
                    // Profile-picker addition: populate the chip row
                    // from a real `profiles/list` call against thread
                    // 0's bound gateway -- profiles are gateway-wide
                    // (registered once per acpx-server process, not
                    // per-thread), so any bound thread's connection can
                    // answer this; thread 0 always exists once the
                    // fixed v1 thread list has been created.
                    if let Some(bridge) = &panel.bridge {
                        component.set_available_profiles(models::to_profile_options(
                            bridge.list_profiles(0),
                        ));
                    }
                    component.set_settings_open(true);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_save(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let defaults = PanelDefaults {
                        profile_name: non_empty(component.get_default_profile().to_string()),
                        permission_profile: non_empty(component.get_permission_profile().to_string()),
                        background_session: component.get_background_default(),
                    };
                    if let Some(store) = panel.panel_state.as_ref() {
                        if let Err(error) = store.save_defaults(&defaults) {
                            eprintln!("panel-rust: failed to save chat defaults: {error}");
                            return;
                        }
                    }
                    component.set_settings_open(false);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_close(move || {
            if let Some(component) = component_weak.upgrade() {
                component.set_settings_open(false);
            }
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_new_thread_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                let mut slot = cell.borrow_mut();
                let Some(panel) = slot.as_mut() else {
                    return;
                };
                let next_number = panel.thread_names.borrow().len() + 1;
                let name = format!("New thread {next_number}");
               let Some(bridge) = panel.bridge.as_mut() else {
                   return;
               };
                // Profile-picker addition: a new thread opens with
                // whichever profile is currently set as the settings
                // sheet's default (empty means native/unmanaged mode,
                // matching `add_thread`'s prior always-`None` behavior).
                let default_profile = component.get_default_profile().to_string();
                let profile = non_empty(default_profile);
                let Ok(real_idx) = bridge.add_thread_with_profile(&name, profile.as_deref())
                else {
                    return;
                };
                panel.thread_names.borrow_mut().push(name);
                panel.thread_state.borrow_mut().push(ThreadState::Idle);
                panel.thread_errors.borrow_mut().push(String::new());
                panel.search_query.borrow_mut().clear();
                panel.refresh_threads_model();
                let filtered_idx = {
                    let visible_indices = panel.visible_indices.borrow();
                    visible_indices.iter().position(|idx| *idx == real_idx)
                };
                if let Some(filtered_idx) = filtered_idx {
                    component.set_selected_thread(filtered_idx as i32);
                    panel.refresh_messages_for(real_idx);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_send_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            let text = component.get_compose_text().to_string();
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let filtered_idx = component.get_selected_thread() as usize;
            component.set_compose_text("".into());
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(filtered_idx) else {
                        return;
                    };
                    let Some(bridge) = &panel.bridge else { return };
                    if panel
                        .thread_state
                        .borrow()
                        .get(idx)
                        .is_some_and(|state| *state == ThreadState::Loading)
                    {
                        return;
                    }
                    if let Some(error) = panel.thread_errors.borrow_mut().get_mut(idx) {
                        error.clear();
                    }
                    bridge.push_local(
                        idx,
                        ChatMessage {
                            kind: MessageKind::User,
                            text: text.to_string(),
                            status: None,
                        },
                    );
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        *slot = ThreadState::Loading;
                    }
                    panel.refresh_threads_model();
                    if Some(idx) == panel.real_index(component.get_selected_thread() as usize) {
                        panel.refresh_messages_for(idx);
                    }
                    bridge.send_prompt(idx, text.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_stop_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    if !matches!(
                        panel.thread_state.borrow().get(idx),
                        Some(ThreadState::Loading)
                    ) {
                        return;
                    }
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        *slot = ThreadState::Cancelling;
                    }
                    panel.refresh_threads_model();
                    panel
                        .bridge
                        .as_ref()
                        .map(|bridge| bridge.cancel_prompt(idx));
                }
            });
        });

        // Interactive agent-request relay addition: approve/reject
        // buttons on the request card built by `refresh_pending_request_
        // for`. Both handlers re-read the exact `AgentRequestEvent` from
        // `AgentBridge::pending_requests` (rather than trusting only the
        // Slint-side `PendingRequestItem` snapshot's `relay-id` string)
        // so `permission::build_response` gets the real, untruncated
        // `raw_request` needed to build a native `session/request_
        // permission` reply -- the Slint struct only carries a
        // human-readable summary, not the full JSON.
        let component_weak = panel.component.as_weak();
        panel.component.on_approve_request(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    panel.answer_pending_request(&component, true);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_reject_request(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    panel.answer_pending_request(&component, false);
                }
            });
        });

        // Terminal-view addition: expand a card into the floating
        // overlay, and close it. `refresh_terminals_for` (called from
        // every `refresh_messages_for`) keeps whichever terminal is
        // currently expanded live-updating; these two callbacks only
        // own which id (if any) is expanded.
        let component_weak = panel.component.as_weak();
        panel.component.on_expand_terminal(move |terminal_id| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    *panel.expanded_terminal_id.borrow_mut() = Some(terminal_id.to_string());
                    let Some(real_idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    panel.refresh_terminals_for(real_idx);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_close_terminal_overlay(move || {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    *panel.expanded_terminal_id.borrow_mut() = None;
                }
            });
        });

        // Mode/config selector addition: dispatch `session/set_mode`/
        // `session/set_config_option` on the *currently displayed*
        // thread. Neither callback optimistically updates `current-
        // mode-id`/`config-option-rows` itself -- both wait for the
        // real backend's own confirmation (`AgentEvent::
        // CurrentModeChanged`/`ConfigOptions`, applied by `apply_bridge_
        // events` -> `refresh_capabilities_for`), matching `AgentBridge::
        // set_mode`/`set_config_option`'s own "requested, not applied"
        // doc comment -- a backend can reject/ignore the request or
        // resolve to a different value than requested (config options
        // especially: changing one can change others), and this UI
        // should never show a selection the backend didn't actually
        // confirm.
        let component_weak = panel.component.as_weak();
        panel.component.on_mode_selected(move |mode_id| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else { return };
                    let Some(real_idx) =
                        panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    bridge.set_mode(real_idx, mode_id.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_config_option_selected(move |option_id, value| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        let Some(bridge) = &panel.bridge else { return };
                        let Some(real_idx) =
                            panel.real_index(component.get_selected_thread() as usize)
                        else {
                            return;
                        };
                        bridge.set_config_option(
                            real_idx,
                            option_id.to_string(),
                            serde_json::Value::String(value.to_string()),
                        );
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel.component.on_search_changed(move |query| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
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
                        Some(real_idx) => panel.refresh_messages_for(real_idx),
                        None => component
                            .set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new()))),
                    }
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_toggle_expanded(move |index| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(real_idx) = panel.displayed_thread.get() else {
                        return;
                    };
                    let idx = index as usize;
                    let mut expanded = panel.expanded.borrow_mut();
                    if let Some(slot) = expanded.get_mut(idx) {
                        *slot = !*slot;
                    }
                    drop(expanded);
                    panel.render_messages(real_idx);
                }
            });
        });

        *slot = Some(panel);
        &SENTINEL as *const PanelHandle as *mut PanelHandle
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_destroy(_handle: *mut PanelHandle) {
    // The C ABI handle is a process-local sentinel; the actual ownership is
    // the thread-local singleton. Clearing it drops AgentBridge and stops
    // local actors when Qt destroys or recreates the dock.
    PANEL.with(|cell| {
        cell.borrow_mut().take();
    });
}

/// Forward a click at physical pixel coordinates, as a press+release pair.
#[no_mangle]
pub extern "C" fn panel_rust_input_click(_handle: *mut PanelHandle, x: c_uint, y: c_uint) -> bool {
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
        return false;
    };
    let pos = slint::LogicalPosition::new(x as f32, y as f32);
    let win = window.window();
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
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
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
    let win = window.window();
    if pressed {
        win.dispatch_event(WindowEvent::KeyPressed { text: key_text });
    } else {
        win.dispatch_event(WindowEvent::KeyReleased { text: key_text });
    }
    true
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

/// Applies a generation-ordered host appearance snapshot. The host owns only
/// selector values; the panel retains its component palette and tokens.
#[no_mangle]
pub extern "C" fn panel_rust_apply_appearance(
    _handle: *mut PanelHandle,
    generation: u64,
    dark: bool,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let appearance = HostAppearance {
            generation,
            color_scheme: if dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            },
            language_tag: String::new(),
            bundled_font: String::new(),
            font_scale: 1.0,
            density: 1.0,
        };
        if !panel.appearance.borrow_mut().apply(appearance) {
            return false;
        }
        Palette::get(&panel.component).set_theme(if dark { "dark" } else { "light" }.into());
        panel.window.window().request_redraw();
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
            Some(panel) => {
                panel.buffer.borrow().len() * std::mem::size_of::<PremultipliedRgbaColor>()
            }
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn panel_create_destroy_create_reuses_slint_platform() {
        // Force the bridge into its documented display-only fallback so
        // this lifecycle regression test does not depend on a developer's
        // running gateways or mutate an external session.
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let previous = [
            (
                "RUI_ACPX_CODEX_URL",
                std::env::var("RUI_ACPX_CODEX_URL").ok(),
            ),
            (
                "RUI_ACPX_CLAUDE_URL",
                std::env::var("RUI_ACPX_CLAUDE_URL").ok(),
            ),
            ("RUI_ACP_CACHE_DIR", std::env::var("RUI_ACP_CACHE_DIR").ok()),
        ];
        std::env::set_var("RUI_ACPX_CODEX_URL", "http://127.0.0.1:1");
        std::env::set_var("RUI_ACPX_CLAUDE_URL", "http://127.0.0.1:1");
        std::env::set_var("RUI_ACP_CACHE_DIR", cache_dir.path());

        let first = panel_rust_create(96, 64);
        assert!(!first.is_null());
        assert_eq!(panel_rust_width(first), 96);
        assert_eq!(panel_rust_height(first), 64);
        assert!(panel_rust_render(first));
        panel_rust_destroy(first);
        assert_eq!(panel_rust_width(first), 0);

        let second = panel_rust_create(128, 72);
        assert!(!second.is_null());
        assert_eq!(panel_rust_width(second), 128);
        assert_eq!(panel_rust_height(second), 72);
        assert!(panel_rust_render(second));
        panel_rust_destroy(second);
        assert_eq!(panel_rust_width(second), 0);

        for (key, value) in previous {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
