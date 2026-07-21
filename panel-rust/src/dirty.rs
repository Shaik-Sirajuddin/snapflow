//! `tea-slint-model` Phase 1: fine-grained "what changed" markers returned
//! by `update()` and consumed by `sync()`. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md` for the full design.
//!
//! `sync()` (Phase 3) must match over `Dirty` **exhaustively, no wildcard
//! arm** -- see that plan's "Exhaustiveness requirement" section for why
//! this is a hard requirement, not a style preference.

/// An id-keyed diff op for list-shaped state (threads, messages, skills).
/// Carries the *identity* the row is keyed by (`thread_id`, message id,
/// skill path) rather than a plain positional index, so `sync()` can apply
/// it to a persistent `Rc<VecModel<T>>` via `.insert`/`.remove` without
/// tearing down unrelated rows' Slint-side identity (and therefore their
/// in-flight animations) -- see 00-plan.md's "Known gap: list resets
/// still break row identity / animations".
#[derive(Debug, Clone, PartialEq)]
pub enum RowOp<T> {
    Insert { at: usize, row: T },
    Remove { at: usize },
    Move { from: usize, to: usize },
}

/// Non-fatal, user-visible error surfaced by a failed `Effect` -- see
/// 00-plan.md's "Effect-result contracts": every `Effect` failure must
/// produce one of these, there is no silent-failure arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorDetail {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dirty {
    /// A single scalar/global property changed (selected thread, compose
    /// text, settings-open flag, etc.) -- `sync()` re-pushes just that
    /// setter.
    Scalar(ScalarField),
    /// One existing thread row changed shape-preservingly (rename,
    /// toggle-background, status) -- `set_row_data(idx, ..)`, no
    /// insert/remove.
    ThreadRow(usize),
    /// The thread list's *shape* changed (add/remove/reorder) -- id-keyed
    /// diff ops, never a full replace (see 00-plan.md's known gap).
    ThreadListDiff(Vec<RowOp<crate::models::VisibleThreadItem>>),
    /// A message was appended to `thread_id`'s history in a
    /// shape-preserving way (single push, no reshuffle upstream).
    MessageAppended { thread_id: String },
    /// `thread_id`'s message list shape changed (older page loaded,
    /// message removed) -- id-keyed diff ops.
    MessagesDiff {
        thread_id: String,
        ops: Vec<RowOp<crate::protocol_types::ChatMessage>>,
    },
    /// An in-progress streamed token/chunk for one message -- resolved by
    /// id at apply time in `sync/messages.rs`, never a cached row index
    /// (see 00-plan.md's "Known gap: streaming rows vs. list-shape
    /// diffs").
    MessageStreamingDelta {
        thread_id: String,
        message_id: String,
        delta: String,
    },
    /// `thread_id`'s connection/reconnect status changed -- updates the
    /// *existing* status row in place (fixes
    /// `reconnecting_message_and_acpx_settings_propagation`).
    Connection { thread_id: String },
    Error { thread_id: String, detail: ErrorDetail },
    PendingRequest { thread_id: String },
    Terminal { id: String },
    LocalTerminal,
    /// Settings changed -- pushed into both the settings panel and chat
    /// view in one place (fixes "settings not propagated to chat view").
    Settings,
    SkillsListDiff(Vec<RowOp<crate::skills_state::SkillEntry>>),
    SkillRow(usize),
    Capabilities { thread_id: String },
}

/// Scalar/global properties that can be marked dirty without a dedicated
/// `Dirty` variant of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarField {
    SelectedThread,
    ComposeText,
    SettingsOpen,
    SettingsScope,
    ExpandedTerminal,
    SearchQuery,
}
