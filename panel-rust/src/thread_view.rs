//! Stable host-side ownership for one retained chat view per durable thread.
//!
//! This is the ownership seam for the multi-ChatView migration. It is kept
//! separate from the current shared Slint model until the UI boundary and all
//! update routes can move together.

use crate::MessageItem;
use slint::{Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

pub(crate) type ThreadMessageModel = Rc<VecModel<MessageItem>>;

#[derive(Clone)]
struct ThreadMessageState {
    model: ThreadMessageModel,
    tool_groups: Rc<VecModel<crate::ToolGroupItem>>,
    tool_group_models: RefCell<HashMap<String, Rc<VecModel<MessageItem>>>>,
    tool_group_slots: RefCell<Vec<Option<(String, usize)>>>,
    tool_group_slot_keys: RefCell<Vec<Option<String>>>,
    keys: RefCell<Vec<String>>,
    row_index: RefCell<HashMap<String, usize>>,
    content_hash: RefCell<HashMap<String, u64>>,
}

#[derive(Clone, Default)]
pub(crate) struct ThreadViewModels {
    by_thread_id: HashMap<String, ThreadMessageState>,
}

impl ThreadViewModels {
    pub(crate) fn ensure(&mut self, thread_id: &str) -> ThreadMessageModel {
        self.by_thread_id
            .entry(thread_id.to_owned())
            .or_insert_with(|| ThreadMessageState {
                model: Rc::new(VecModel::default()),
                tool_groups: Rc::new(VecModel::default()),
                tool_group_models: RefCell::new(HashMap::new()),
                tool_group_slots: RefCell::new(Vec::new()),
                tool_group_slot_keys: RefCell::new(Vec::new()),
                keys: RefCell::new(Vec::new()),
                row_index: RefCell::new(HashMap::new()),
                content_hash: RefCell::new(HashMap::new()),
            })
            .model
            .clone()
    }

    pub(crate) fn get(&self, thread_id: &str) -> Option<ThreadMessageModel> {
        self.by_thread_id
            .get(thread_id)
            .map(|state| state.model.clone())
    }

    pub(crate) fn tool_groups(&self, thread_id: &str) -> Option<ModelRc<crate::ToolGroupItem>> {
        self.by_thread_id
            .get(thread_id)
            .map(|state| ModelRc::from(state.tool_groups.clone()))
    }

    /// Keep a compact, indexed group model beside the durable message model.
    /// Each group row model contains only that contiguous tool run, while the
    /// outer model has one cheap slot per message index so Slint can resolve
    /// `tool-groups[msg.index]` without scanning all groups.
    pub(crate) fn reconcile_tool_groups(
        &self,
        thread_id: &str,
        keys: &[String],
        rows: &[MessageItem],
    ) {
        let Some(state) = self.by_thread_id.get(thread_id) else {
            return;
        };
        let mut active = std::collections::HashSet::new();
        let mut slots = Vec::with_capacity(rows.len());
        let mut slot_keys = vec![None; rows.len()];
        let mut row_slots = vec![None; rows.len()];
        let mut start = 0usize;
        while start < rows.len() {
            let len = rows[start].tool_group_len.max(0) as usize;
            if len == 0 {
                slots.push(crate::ToolGroupItem {
                    label: String::new().into(),
                    item_count: 0,
                    messages: ModelRc::default(),
                });
                start += 1;
                continue;
            }
            let end = (start + len).min(rows.len());
            let base_key = keys
                .get(start)
                .cloned()
                .unwrap_or_else(|| start.to_string());
            // Keep malformed duplicate message/group keys from aliasing the
            // same retained VecModel. A deterministic position suffix makes
            // each group independent while preserving identity across
            // frames as long as the group remains at that position.
            let key = if active.contains(&base_key) {
                format!("{base_key}#dup{start}")
            } else {
                base_key
            };
            active.insert(key.clone());
            let group_model = state
                .tool_group_models
                .borrow_mut()
                .entry(key.clone())
                .or_insert_with(|| Rc::new(VecModel::default()))
                .clone();
            // Tool output streams are reconciled on every frame. Keep the
            // retained group model stable and only publish rows whose view
            // fingerprint changed; this avoids broadcasting a VecModel
            // change for every unchanged tool call on every poll tick.
            replace_rows_bounded(&group_model, &rows[start..end]);
            let label = if rows[start].kind == "mcp_server_call" {
                "MCP CALL"
            } else {
                "TOOL USE"
            };
            slots.resize_with(end, || crate::ToolGroupItem {
                label: String::new().into(),
                item_count: 0,
                messages: ModelRc::default(),
            });
            slots[start] = crate::ToolGroupItem {
                label: label.into(),
                item_count: (end - start) as i32,
                messages: ModelRc::from(group_model),
            };
            slot_keys[start] = Some(key.clone());
            for (offset, slot) in row_slots[start..end].iter_mut().enumerate() {
                *slot = Some((
                    key.clone(),
                    offset,
                ));
            }
            start = end;
        }
        *state.tool_group_slots.borrow_mut() = row_slots;
        state
            .tool_group_models
            .borrow_mut()
            .retain(|key, _| active.contains(key));
        while state.tool_groups.row_count() > slots.len() {
            state.tool_groups.remove(state.tool_groups.row_count() - 1);
        }
        for (index, slot) in slots.into_iter().enumerate() {
            if index < state.tool_groups.row_count() {
                let expected_key = slot_keys.get(index).and_then(Clone::clone);
                let current_key = state
                    .tool_group_slot_keys
                    .borrow()
                    .get(index)
                    .and_then(Clone::clone);
                let unchanged = state.tool_groups.row_data(index).is_some_and(|current| {
                    current_key == expected_key
                        && current.label == slot.label
                        && current.item_count == slot.item_count
                });
                if !unchanged {
                    state.tool_groups.set_row_data(index, slot);
                }
            } else {
                state.tool_groups.push(slot);
            }
        }
        *state.tool_group_slot_keys.borrow_mut() = slot_keys;
    }

    pub(crate) fn update_tool_group_row(
        &self,
        thread_id: &str,
        row_index: usize,
        row: MessageItem,
    ) {
        let Some(state) = self.by_thread_id.get(thread_id) else {
            return;
        };
        let Some(Some((key, group_index))) =
            state.tool_group_slots.borrow().get(row_index).cloned()
        else {
            return;
        };
        if let Some(group) = state.tool_group_models.borrow().get(&key) {
            if group_index < group.row_count() {
                group.set_row_data(group_index, row);
            }
        }
    }

    pub(crate) fn keys(&self, thread_id: &str) -> Option<Vec<String>> {
        self.by_thread_id
            .get(thread_id)
            .map(|state| state.keys.borrow().clone())
    }

    pub(crate) fn rows(&self, thread_id: &str) -> Option<Vec<MessageItem>> {
        self.by_thread_id.get(thread_id).map(|state| {
            (0..state.model.row_count())
                .filter_map(|index| state.model.row_data(index))
                .collect()
        })
    }

    pub(crate) fn row_index_for(&self, thread_id: &str, key: &str) -> Option<usize> {
        self.by_thread_id
            .get(thread_id)
            .and_then(|state| state.row_index.borrow().get(key).copied())
    }

    /// Return the last projected content fingerprint for a message key. The
    /// index is deliberately separate from markdown parsing: it is only a
    /// cheap host-side guard for keyed row updates.
    pub(crate) fn content_hash_for(&self, thread_id: &str, key: &str) -> Option<u64> {
        self.by_thread_id
            .get(thread_id)
            .and_then(|state| state.content_hash.borrow().get(key).copied())
    }

    pub(crate) fn set_content_hashes(
        &self,
        thread_id: &str,
        rows: impl IntoIterator<Item = (String, MessageItem)>,
    ) {
        if let Some(state) = self.by_thread_id.get(thread_id) {
            let hashes = rows
                .into_iter()
                .map(|(key, row)| (key, message_content_hash(&row)))
                .collect();
            *state.content_hash.borrow_mut() = hashes;
        }
    }

    pub(crate) fn set_keys(&self, thread_id: &str, keys: Vec<String>) {
        if let Some(state) = self.by_thread_id.get(thread_id) {
            *state.keys.borrow_mut() = keys;
            let index = state
                .keys
                .borrow()
                .iter()
                .enumerate()
                .fold(HashMap::new(), |mut index, (row, key)| {
                    // A malformed/partially streamed transcript can
                    // briefly produce duplicate fallback keys. Keep the
                    // first row authoritative; overwriting here silently
                    // redirected keyed updates to the last duplicate.
                    index.entry(key.clone()).or_insert(row);
                    index
                });
            *state.row_index.borrow_mut() = index;
        }
    }

    pub(crate) fn remove(&mut self, thread_id: &str) -> Option<ThreadMessageModel> {
        self.by_thread_id.remove(thread_id).map(|state| state.model)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_thread_id.len()
    }

    pub(crate) fn ensure_for_thread_ids<'a>(
        &mut self,
        thread_ids: impl IntoIterator<Item = &'a str>,
    ) {
        for thread_id in thread_ids {
            if !thread_id.is_empty() {
                self.ensure(thread_id);
            }
        }
    }

    /// Drop retained models whose durable thread no longer exists. This is
    /// the lifecycle boundary for closed/project-removed threads; selected
    /// thread switches never call it, so A/B/A identity remains stable.
    pub(crate) fn retain_thread_ids<'a>(&mut self, thread_ids: impl IntoIterator<Item = &'a str>) {
        let keep = thread_ids
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        self.by_thread_id
            .retain(|thread_id, _| keep.contains(thread_id.as_str()));
    }
}

fn replace_rows_bounded(model: &VecModel<MessageItem>, rows: &[MessageItem]) {
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for (index, row) in rows.iter().cloned().enumerate() {
        if index < model.row_count() {
            let unchanged = model.row_data(index).is_some_and(|current| {
                message_content_hash(&current) == message_content_hash(&row)
            });
            if !unchanged {
                model.set_row_data(index, row);
            }
        } else {
            model.push(row);
        }
    }
}

fn message_content_hash(row: &MessageItem) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    row.kind.as_str().hash(&mut hasher);
    row.text.as_str().hash(&mut hasher);
    row.status.as_str().hash(&mut hasher);
    row.raw_input.as_str().hash(&mut hasher);
    row.raw_output.as_str().hash(&mut hasher);
    row.expanded.hash(&mut hasher);
    row.index.hash(&mut hasher);
    row.queued.hash(&mut hasher);
    row.can_edit.hash(&mut hasher);
    row.can_send_now.hash(&mut hasher);
    row.sending.hash(&mut hasher);
    row.first_use.hash(&mut hasher);
    // Tool grouping is rendered through the retained nested model. Include
    // its span in the fingerprint or a streamed tool-call update that only
    // changes grouping metadata will leave Slint with stale group slots.
    row.tool_group_len.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    #[test]
    fn repeated_lookup_preserves_model_identity() {
        let mut views = ThreadViewModels::default();
        let first = views.ensure("thread-a");
        first.push(MessageItem::default());

        let second = views.ensure("thread-a");

        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(second.row_count(), 1);
    }

    #[test]
    fn different_threads_have_independent_models() {
        let mut views = ThreadViewModels::default();
        let thread_a = views.ensure("thread-a");
        let thread_b = views.ensure("thread-b");

        thread_a.push(MessageItem::default());

        assert!(!Rc::ptr_eq(&thread_a, &thread_b));
        assert_eq!(thread_a.row_count(), 1);
        assert_eq!(thread_b.row_count(), 0);
        assert_eq!(views.len(), 2);
    }

    #[test]
    fn removing_a_thread_drops_its_registry_entry() {
        let mut views = ThreadViewModels::default();
        let model = views.ensure("thread-a");

        assert!(views.remove("thread-a").is_some());
        assert!(views.get("thread-a").is_none());
        assert_eq!(views.len(), 0);

        let replacement = views.ensure("thread-a");
        assert!(!Rc::ptr_eq(&model, &replacement));
    }

    #[test]
    fn keyed_content_hash_changes_without_changing_model_identity() {
        let mut views = ThreadViewModels::default();
        let model = views.ensure("thread-a");
        let row = MessageItem {
            text: "before".into(),
            ..MessageItem::default()
        };
        views.set_content_hashes("thread-a", [("assistant:m1".into(), row.clone())]);
        let before = views.content_hash_for("thread-a", "assistant:m1");

        let changed = MessageItem {
            text: "after".into(),
            ..row
        };
        views.set_content_hashes("thread-a", [("assistant:m1".into(), changed)]);

        assert_ne!(before, views.content_hash_for("thread-a", "assistant:m1"));
        assert!(Rc::ptr_eq(&model, &views.ensure("thread-a")));
    }

    #[test]
    fn tool_group_slot_key_change_replaces_same_shaped_group() {
        let mut views = ThreadViewModels::default();
        views.ensure("thread-a");
        let first = vec![MessageItem {
            kind: "tool_use".into(),
            text: "first".into(),
            tool_group_len: 1,
            ..MessageItem::default()
        }];
        views.reconcile_tool_groups("thread-a", &["tool:one".into()], &first);

        let second = vec![MessageItem {
            kind: "tool_use".into(),
            text: "second".into(),
            tool_group_len: 1,
            ..MessageItem::default()
        }];
        views.reconcile_tool_groups("thread-a", &["tool:two".into()], &second);

        let group = views.tool_groups("thread-a").unwrap().row_data(0).unwrap();
        assert_eq!(group.messages.row_data(0).unwrap().text, "second");
    }

    #[test]
    fn duplicate_keys_keep_first_row_index() {
        let mut views = ThreadViewModels::default();
        views.ensure("thread-a");
        views.set_keys("thread-a", vec!["same".into(), "other".into(), "same".into()]);
        assert_eq!(views.row_index_for("thread-a", "same"), Some(0));
        assert_eq!(views.row_index_for("thread-a", "other"), Some(1));
    }

    #[test]
    fn retaining_thread_ids_drops_closed_views_but_keeps_live_identity() {
        let mut views = ThreadViewModels::default();
        let retained = views.ensure("thread-a");
        views.ensure("thread-b");

        views.retain_thread_ids(["thread-a"]);

        assert!(views.get("thread-b").is_none());
        assert!(Rc::ptr_eq(&retained, &views.get("thread-a").unwrap()));
        assert_eq!(views.len(), 1);
    }

    #[test]
    fn retained_thread_budget_fixture_scales_to_runtime_gate_sizes() {
        for count in [50usize, 200, 1000] {
            let mut views = ThreadViewModels::default();
            for index in 0..count {
                views.ensure(&format!("thread-{index}"));
            }
            assert_eq!(views.len(), count);
            assert!(views.get(&format!("thread-{}", count - 1)).is_some());
        }
    }

    #[test]
    fn tool_groups_use_compact_models_and_patch_streaming_rows_in_place() {
        let mut views = ThreadViewModels::default();
        views.ensure("thread-a");
        let rows = vec![
            MessageItem {
                kind: "tool_use".into(),
                text: "call 1".into(),
                index: 0,
                tool_group_len: 2,
                ..MessageItem::default()
            },
            MessageItem {
                kind: "tool_use".into(),
                text: "call 2".into(),
                index: 1,
                ..MessageItem::default()
            },
            MessageItem {
                kind: "agent".into(),
                text: "answer".into(),
                index: 2,
                ..MessageItem::default()
            },
        ];
        let keys = vec!["tool:1".into(), "tool:2".into(), "assistant:3".into()];
        views.reconcile_tool_groups("thread-a", &keys, &rows);

        let groups = views.tool_groups("thread-a").unwrap();
        assert_eq!(groups.row_count(), rows.len());
        let group = groups.row_data(0).unwrap();
        assert_eq!(group.item_count, 2);
        assert_eq!(group.messages.row_count(), 2);
        assert_eq!(groups.row_data(2).unwrap().item_count, 0);

        // A second projection with identical rows must preserve the compact
        // group contents; the reconciler should not publish redundant row
        // notifications on every frame.
        views.reconcile_tool_groups("thread-a", &keys, &rows);
        assert_eq!(
            groups
                .row_data(0)
                .unwrap()
                .messages
                .row_data(1)
                .unwrap()
                .text,
            "call 2"
        );

        let mut changed_rows = rows.clone();
        changed_rows[1].text = "call 2 updated".into();
        views.reconcile_tool_groups("thread-a", &keys, &changed_rows);
        assert_eq!(
            groups
                .row_data(0)
                .unwrap()
                .messages
                .row_data(1)
                .unwrap()
                .text,
            "call 2 updated"
        );

        views.update_tool_group_row(
            "thread-a",
            1,
            MessageItem {
                text: "call 2 streamed".into(),
                index: 1,
                ..rows[1].clone()
            },
        );
        assert_eq!(
            groups
                .row_data(0)
                .unwrap()
                .messages
                .row_data(1)
                .unwrap()
                .text,
            "call 2 streamed"
        );
    }
}
