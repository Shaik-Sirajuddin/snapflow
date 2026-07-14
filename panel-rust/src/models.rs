//! Conversions between `rui-acp-client`'s ACP-facing types and the
//! generated Slint `ThreadItem`/`MessageItem` structs, kept apart from
//! `agent_bridge.rs`'s actual ACP/jsonl orchestration logic and from
//! `lib.rs`'s FFI/event-wiring glue (modularity requirement,
//! chat-panel-ui-theme-parity.md). Pure data transforms only -- nothing
//! here touches the Slint runtime beyond the generated struct types
//! themselves, so it's straightforward to unit test without a live
//! `ChatPanel` component.

use crate::{MessageItem, ThreadItem};
use rui_acp_client::{ChatMessage, MessageKind};
use slint::{ModelRc, VecModel};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    Idle,
    Loading,
    Error,
}

impl ThreadState {
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadState::Idle => "idle",
            ThreadState::Loading => "loading",
            ThreadState::Error => "error",
        }
    }
}

fn message_kind_str(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::User => "user",
        MessageKind::Agent => "agent",
        MessageKind::Thinking => "thinking",
        MessageKind::ToolCall => "tool-call",
    }
}

pub fn to_message_model(msgs: Vec<ChatMessage>) -> ModelRc<MessageItem> {
    let items: Vec<MessageItem> = msgs
        .into_iter()
        .map(|m| MessageItem {
            kind: message_kind_str(&m.kind).into(),
            text: m.text.into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// One row of the (possibly filtered) sidebar list, paired with its
/// real index into `names`/`state`/the agent bridge -- callers must
/// carry `real_index` alongside the row so a later Slint-side selection
/// (`thread-selected(filtered_idx)`) can be translated back to the
/// actual thread the bridge/`thread_state` know about. See
/// `PanelSingleton::visible_indices`/`real_index` in `lib.rs`.
pub struct VisibleThreadItem {
    pub real_index: usize,
    pub item: ThreadItem,
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
pub fn build_thread_items(
    names: &[&str],
    state: &[ThreadState],
    query: &str,
) -> Vec<VisibleThreadItem> {
    let query_lower = query.trim().to_lowercase();
    names
        .iter()
        .enumerate()
        .zip(state.iter())
        .filter(|((_, name), _)| {
            query_lower.is_empty() || name.to_lowercase().contains(&query_lower)
        })
        .map(|((real_index, name), st)| VisibleThreadItem {
            real_index,
            item: ThreadItem {
                name: (*name).into(),
                status: st.as_str().into(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn empty_query_returns_every_thread_in_order() {
        let items = build_thread_items(NAMES, STATE, "");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].item.name, "Fix timeline crash");
        assert_eq!(items[0].real_index, 0);
        assert_eq!(items[3].item.name, "Export pipeline bug");
        assert_eq!(items[3].real_index, 3);
    }

    #[test]
    fn substring_match_is_case_insensitive() {
        let items = build_thread_items(NAMES, STATE, "FADE");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
        // Real index must survive filtering -- "Add fade transition" is
        // THREAD_NAMES[1], even though it's now row 0 of the filtered
        // list. This is exactly the mismatch `real_index` exists to fix.
        assert_eq!(items[0].real_index, 1);

        let items = build_thread_items(NAMES, STATE, "fade");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item.name, "Add fade transition");
    }

    #[test]
    fn multiple_matches_preserve_original_order_no_resort() {
        // "x" appears in 2 non-adjacent names (index 0 and 3); must come
        // back in the same relative order as NAMES, not re-sorted, and
        // must skip the non-matching ones in between.
        let items = build_thread_items(NAMES, STATE, "x");
        let matched_names: Vec<&str> = items.iter().map(|i| i.item.name.as_str()).collect();
        assert_eq!(matched_names, vec!["Fix timeline crash", "Export pipeline bug"]);
        let real_indices: Vec<usize> = items.iter().map(|i| i.real_index).collect();
        assert_eq!(real_indices, vec![0, 3]);
    }

    #[test]
    fn no_match_returns_empty_not_error() {
        let items = build_thread_items(NAMES, STATE, "zzz-no-such-thread");
        assert!(items.is_empty());
    }

    #[test]
    fn whitespace_only_query_behaves_like_empty() {
        let items = build_thread_items(NAMES, STATE, "   ");
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn status_is_carried_through_unfiltered() {
        let items = build_thread_items(NAMES, STATE, "");
        assert_eq!(items[1].item.status, "loading");
        assert_eq!(items[2].item.status, "error");
    }
}
