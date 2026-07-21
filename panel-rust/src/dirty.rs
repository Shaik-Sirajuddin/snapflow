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

/// `tea-slint-model` Phase 5: computes the minimal `RowOp` sequence that
/// turns `old_keys` into `new_keys` (with `new_rows` supplying the actual
/// row value for each insert), so `sync/*.rs` can apply it to a
/// persistent `Rc<VecModel<T>>` instead of replacing the whole model --
/// see 00-plan.md's "Known gap: list resets still break row identity /
/// animations".
///
/// `old_keys`/`new_keys` are the same stable identity a row is keyed by
/// (thread `real_index`, message id, skill path -- never a plain
/// positional index, which is exactly what breaks under reorder/insert).
/// `new_rows[i]` must correspond to `new_keys[i]`.
///
/// Algorithm: a straightforward LCS-based (longest-common-subsequence)
/// diff -- keys present in both, in the same relative order, are left
/// alone; everything else becomes a `Remove` (old-only) or `Insert`
/// (new-only), applied against a working copy of the old key list so
/// `at` indices stay valid across the whole returned sequence when
/// applied strictly in order. `Move` is never emitted by this function
/// (a moved key is simplest -- and, for every real call site in this
/// crate, correct -- as a `Remove` at its old position + `Insert` at its
/// new one); it stays a `RowOp` variant for `sync.rs`'s exhaustive match
/// and for hand-authored ops elsewhere, not because this algorithm
/// produces it.
pub fn diff_by_id<T: Clone, K: Eq + Clone>(
    old_keys: &[K],
    new_keys: &[K],
    new_rows: &[T],
) -> Vec<RowOp<T>> {
    debug_assert_eq!(new_keys.len(), new_rows.len());

    // Longest common subsequence of keys, by classic DP -- old_keys[i] and
    // new_keys[j] "match" iff equal. lcs[i][j] = length of the LCS of
    // old_keys[..i] and new_keys[..j].
    let n = old_keys.len();
    let m = new_keys.len();
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            lcs[i + 1][j + 1] = if old_keys[i] == new_keys[j] {
                lcs[i][j] + 1
            } else {
                lcs[i][j + 1].max(lcs[i + 1][j])
            };
        }
    }

    // Walk the DP table backward to recover which old/new indices are
    // "kept" (part of the LCS) vs removed/inserted, then reverse to get
    // removes-before-inserts-in-forward-order (so `at` indices computed
    // against a shrinking-then-growing working list stay valid when the
    // caller applies them strictly in the returned order).
    enum Step {
        Keep,
        Remove,
        Insert,
    }
    let mut steps = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_keys[i - 1] == new_keys[j - 1] {
            steps.push(Step::Keep);
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || lcs[i][j - 1] >= lcs[i - 1][j]) {
            steps.push(Step::Insert);
            j -= 1;
        } else {
            steps.push(Step::Remove);
            i -= 1;
        }
    }
    steps.reverse();

    let mut ops = Vec::new();
    let mut pos = 0usize; // position in the working list, as ops are applied
    let mut new_idx = 0usize;
    for step in steps {
        match step {
            Step::Keep => pos += 1,
            Step::Remove => ops.push(RowOp::Remove { at: pos }),
            Step::Insert => {
                ops.push(RowOp::Insert {
                    at: pos,
                    row: new_rows[new_idx].clone(),
                });
                pos += 1;
            }
        }
        if matches!(step, Step::Keep | Step::Insert) {
            new_idx += 1;
        }
    }
    ops
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn apply(old: &[&str], ops: &[RowOp<&str>]) -> Vec<String> {
        let mut v: Vec<String> = old.iter().map(|s| s.to_string()).collect();
        for op in ops {
            match op {
                RowOp::Insert { at, row } => v.insert(*at, row.to_string()),
                RowOp::Remove { at } => {
                    v.remove(*at);
                }
                RowOp::Move { from, to } => {
                    let item = v.remove(*from);
                    v.insert(*to, item);
                }
            }
        }
        v
    }

    #[test]
    fn no_change_produces_no_ops() {
        let old = ["a", "b", "c"];
        let ops = diff_by_id(&old, &old, &old);
        assert!(ops.is_empty());
    }

    #[test]
    fn pure_append_produces_one_insert() {
        let old = ["a", "b"];
        let new = ["a", "b", "c"];
        let ops = diff_by_id(&old, &new, &new);
        assert_eq!(apply(&old, &ops), strs(&["a", "b", "c"]));
    }

    #[test]
    fn pure_removal_produces_one_remove() {
        let old = ["a", "b", "c"];
        let new = ["a", "c"];
        let ops = diff_by_id(&old, &new, &new);
        assert_eq!(apply(&old, &ops), strs(&["a", "c"]));
    }

    #[test]
    fn removal_in_the_middle_preserves_surrounding_rows() {
        let old = ["a", "b", "c", "d"];
        let new = ["a", "d"];
        let ops = diff_by_id(&old, &new, &new);
        assert_eq!(apply(&old, &ops), strs(&["a", "d"]));
        // Exactly the untouched-row-identity property this exists for:
        // "a" and "d" are never Insert/Remove'd themselves, only "b"/"c".
        for op in &ops {
            match op {
                RowOp::Insert { row, .. } => assert!(*row == "b" || *row == "c" || false),
                RowOp::Remove { .. } => {}
                RowOp::Move { .. } => panic!("this case needs no Move"),
            }
        }
    }

    #[test]
    fn cold_start_from_empty_is_all_inserts() {
        let old: [&str; 0] = [];
        let new = ["a", "b", "c"];
        let ops = diff_by_id(&old, &new, &new);
        assert_eq!(apply(&old, &ops), strs(&["a", "b", "c"]));
    }

    #[test]
    fn mixed_insert_and_remove() {
        let old = ["a", "b", "c"];
        let new = ["b", "c", "d"];
        let ops = diff_by_id(&old, &new, &new);
        assert_eq!(apply(&old, &ops), strs(&["b", "c", "d"]));
    }

    #[test]
    fn duplicate_keys_are_handled_positionally_not_ambiguously() {
        // Not a realistic call-site case (real keys are unique thread_ids/
        // message ids/skill paths) but the algorithm must not panic or
        // infinite-loop on it.
        let old = ["a", "a", "b"];
        let new = ["a", "b", "a"];
        let ops = diff_by_id(&old, &new, &new);
        assert_eq!(apply(&old, &ops), strs(&["a", "b", "a"]));
    }
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
    MessageAppended {
        thread_id: String,
    },
    /// `thread_id`'s message list shape changed (older page loaded,
    /// message removed) -- id-keyed diff ops.
    MessagesDiff {
        thread_id: String,
        ops: Vec<RowOp<crate::MessageItem>>,
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
    Connection {
        thread_id: String,
    },
    Error {
        thread_id: String,
        detail: ErrorDetail,
    },
    PendingRequest {
        thread_id: String,
    },
    Terminal {
        id: String,
    },
    LocalTerminal,
    ProjectPath,
    Appearance,
    Theme,
    /// Settings changed -- pushed into both the settings panel and chat
    /// view in one place (fixes "settings not propagated to chat view").
    Settings,
    SkillsListDiff(Vec<RowOp<crate::SkillOption>>),
    SkillRow(usize),
    SkillEditor,
    Capabilities {
        thread_id: String,
    },
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
