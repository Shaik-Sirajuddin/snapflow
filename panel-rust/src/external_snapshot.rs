//! External effect-source boundary for the TEA frame loop.
//!
//! This module only reads bridge/store/watcher state and packages it into a
//! `FrameInput`. It never mutates `Model` or calls a Slint setter. The
//! reducer remains responsible for folding the snapshot, and `sync()` remains
//! responsible for presentation.

use crate::{msg, AgentBridge, PanelSingleton};
use std::sync::atomic::Ordering;

pub(crate) struct ExternalSnapshotSource<'a> {
    panel: &'a PanelSingleton,
}

impl<'a> ExternalSnapshotSource<'a> {
    pub(crate) fn new(panel: &'a PanelSingleton) -> Self {
        Self { panel }
    }

    pub(crate) fn collect_frame_input(&self) -> msg::FrameInput {
        let bridge_events = self
            .panel
            .bridge
            .as_ref()
            .map(AgentBridge::poll)
            .unwrap_or_default();
        let bridge_event_thread_ids = bridge_events
            .iter()
            .map(|event| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(event.thread_index))
                    .map(|binding| binding.thread_id)
                    .or_else(|| {
                        self.panel
                            .model
                            .borrow()
                            .threads
                            .get(event.thread_index)
                            .map(|thread| thread.thread_id.clone())
                    })
                    .unwrap_or_default()
            })
            .collect();
        let thread_record_snapshots = self.panel.collect_thread_record_snapshots();
        let settings_reload_pending = self
            .panel
            .settings_reload_pending
            .swap(false, Ordering::SeqCst)
            && !self
                .panel
                .settings_ignore_watch_until
                .get()
                .is_some_and(|until| std::time::Instant::now() < until);

        msg::FrameInput {
            bridge_events_pending: !bridge_events.is_empty(),
            bridge_events,
            bridge_event_thread_ids,
            thread_record_snapshots,
            settings_reload_pending,
            local_terminal_snapshot: None,
            prepend_expanded_rows: 0,
            clear_selected_thread: false,
            thread_list_snapshot: Some(self.panel.collect_thread_list_snapshot()),
            selected_thread_snapshot: self.panel.collect_selected_thread_snapshot(),
            settings_preferences_snapshot: (self.panel.component.get_settings_open()
                || settings_reload_pending)
                .then(|| self.panel.collect_settings_preferences_snapshot(None)),
            settings_gateway_snapshot: self
                .panel
                .component
                .get_settings_open()
                .then(|| self.panel.collect_settings_gateway_snapshot()),
            skills_snapshot: None,
        }
    }
}
