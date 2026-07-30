//! Per-thread stable-key row index with content hashes, and the owner of
//! each row's rendered markdown.
//!
//! Building block for `memory/acpx/gen/plans/per-thread-chatview-model/
//! 00-plan.md`'s Phase 4 ("route message updates to the owning thread
//! model" / "message-ID-to-row-index lookup") and
//! `memory/acpx/gen/plans/markdown-render-cache-layer/00-plan.md`'s
//! keyed render cache. Three separate questions, one structure:
//! - Stable message key -> "which row is this?" (`row_index`, keyed by
//!   the same `transcript_row_key` format `models.rs`/`dirty::diff_by_id`
//!   already use).
//! - Content hash -> "does this row need re-rendering?" (`check`,
//!   below -- a fingerprint of the row's raw source text).
//! - The rendered payload itself -> "what do I show without
//!   re-parsing?" (`rendered_blocks`/`rendered_lines`, populated by
//!   `models.rs`'s `markdown_blocks_for`/`markdown_lines_for`, which
//!   used to live behind two separate global, text-content-keyed
//!   `MARKDOWN_CACHE`/`MARKDOWN_BLOCK_CACHE` thread_locals -- retired in
//!   favor of this per-thread, message-key-scoped structure so a
//!   worker-delivered background render has somewhere durable and
//!   per-thread to land, per the render-cache-layer plan's "Unification
//!   decision": one map per thread, not `ThreadMessageIndex` plus a
//!   second parallel `MarkdownRenderEntry` map keyed by the same string).
//!
//! O(1) amortized per key for `check`/`record`/`set_rendered_*`/
//! `rendered_*_for`; O(N) only for `rebuild`, which should run only on a
//! genuine structural transcript change (insert/remove/reorder/cold
//! hydration), never on every poll tick or every streamed token.
//!
//! NOT wired into `update.rs`'s live dispatch path for background-worker
//! delivery yet (the `Msg::MarkdownBlocksReady` half of the render-cache-
//! layer plan) -- same reasoning as `markdown_worker.rs`: that control
//! flow (`switched_thread` / `transcript_changed` / `force_list_install`)
//! is fragile and extensively commented, and wiring delivery into it is
//! real Phase-2 work, not a blind bolt-on. The *synchronous* render path
//! (`models.rs`'s `to_message_rows_from_transcript`, called every poll
//! tick for every thread) IS wired to this module already -- that's
//! Phase 1 and Phase 3 of that plan (keyed identity + frame-projection
//! reuse), landed in this pass.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use slint::ModelRc;

/// One row's bookkeeping: where it currently sits in the projected row
/// list, a fingerprint of the source text last used to build it, and the
/// already-rendered markdown for that exact text (if any render has
/// completed since the hash last changed). `rendered_blocks`/
/// `rendered_lines` are `None` immediately after `record()` -- a content
/// change implicitly invalidates any previous rendering, and the caller
/// is expected to re-render and call `set_rendered_blocks`/
/// `set_rendered_lines` right after. `ModelRc` is not `Send`, so this
/// type (like the rest of this module) is UI-thread-only; a background
/// worker must never construct or hold one.
#[derive(Debug, Clone)]
pub struct MessageEntry {
    pub key: String,
    pub content_hash: u64,
    pub row_index: usize,
    pub rendered_blocks: Option<ModelRc<crate::MarkdownBlock>>,
    pub rendered_lines: Option<ModelRc<crate::MarkdownLine>>,
}

/// Per-thread key -> row bookkeeping. Rebuild wholesale only when the
/// transcript's key set/order changed (insert/remove/reorder/cold
/// hydration, i.e. `keys_match` returns `false`) -- see this type's own
/// tests for exactly which changes those are. A same-position content
/// change (streaming text growing, a tool status flipping) is a single
/// `check` + `record` call, not a rebuild.
#[derive(Debug, Clone, Default)]
pub struct ThreadMessageIndex {
    by_key: HashMap<String, MessageEntry>,
}

/// Deterministic within a process (unlike `HashMap`'s own default
/// `RandomState`, `DefaultHasher::new()` starts from a fixed internal
/// state, not a per-process random seed) -- callers only ever compare
/// hashes produced by this same running process in the same session, so
/// that's exactly the guarantee needed here, not cross-process/on-disk
/// stability.
///
/// `pub(crate)`, not private: `markdown_worker.rs` computes a
/// `source_hash` per message on the background thread (markdown-render-
/// cache-layer plan Phase 2) that must be produced by this *exact* same
/// algorithm, or a delivered result's hash would never match what
/// `content_hash_for` has recorded here even when the source text is
/// genuinely identical -- one hash function, two call sites, not two
/// implementations that could drift.
pub(crate) fn hash_content(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Outcome of asking the index whether a key's row needs re-rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowChange {
    /// Key not present in the index yet. This is a structural change
    /// (an insert) -- the caller must rebuild/insert a fresh entry via
    /// `record`, not treat this as a plain content patch.
    New,
    /// Key present, content hash differs from what's recorded. Patch
    /// only this `row_index`.
    Changed(usize),
    /// Key present, content hash identical. Skip -- no re-render
    /// (and no markdown re-parse) needed for this row.
    Unchanged(usize),
}

impl ThreadMessageIndex {
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// O(1) amortized: `key`'s current row_index, if it has an entry.
    /// markdown-render-cache-layer plan Phase 2: lets a background
    /// worker's delivery resolve `key -> row_index` at apply time,
    /// without a linear scan over `thread.message_rows`.
    pub fn row_index_for(&self, key: &str) -> Option<usize> {
        self.by_key.get(key).map(|e| e.row_index)
    }

    /// O(1) amortized: `key`'s currently-recorded content hash, if any.
    /// The correctness guard for a worker-delivered render: a delivered
    /// `source_hash` that doesn't match this is stale (the source text
    /// changed, or streamed further, since the render was requested) and
    /// must be dropped, not applied -- see 00-plan.md's "Correctness
    /// rules for delivery".
    pub fn content_hash_for(&self, key: &str) -> Option<u64> {
        self.by_key.get(key).map(|e| e.content_hash)
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// O(1) amortized: does `key`'s content (`text`) still match what
    /// this index last recorded for it?
    pub fn check(&self, key: &str, text: &str) -> RowChange {
        match self.by_key.get(key) {
            None => RowChange::New,
            Some(entry) => {
                if hash_content(text) == entry.content_hash {
                    RowChange::Unchanged(entry.row_index)
                } else {
                    RowChange::Changed(entry.row_index)
                }
            }
        }
    }

    /// O(1) amortized: record that `key`'s row at `row_index` now
    /// reflects `text`. Call after applying a targeted patch for a
    /// `RowChange::Changed`/`New` result -- does not touch any other
    /// entry, so streaming a single message's text never costs more
    /// than this one call plus the `check` that preceded it.
    ///
    /// If an entry already exists for `key` with the SAME content_hash,
    /// this only refreshes `row_index` and leaves `rendered_blocks`/
    /// `rendered_lines` untouched -- it does NOT unconditionally wipe
    /// them. This matters because `markdown_lines_for` and
    /// `markdown_blocks_for` (models.rs) both call `record()` for the
    /// *same key* on the *same shared index* while building one row
    /// (agent messages get both a lines and a blocks render). Live-
    /// verified regression, caught via a real e2e run, not a unit test:
    /// unconditionally wiping both fields on every `record()` call made
    /// the two callers permanently ping-pong -- lines' call would wipe
    /// blocks' just-cached payload and vice versa, so `check()` kept
    /// reporting the row as needing a fresh render forever even though
    /// its text never actually changed, defeating this cache's entire
    /// purpose for every "agent" row. Only a genuinely new key or an
    /// actual content_hash change wipes rendered_blocks/rendered_lines
    /// (correctly -- a real content change invalidates both).
    pub fn record(&mut self, key: &str, row_index: usize, text: &str) {
        let content_hash = hash_content(text);
        if let Some(entry) = self.by_key.get_mut(key) {
            if entry.content_hash == content_hash {
                entry.row_index = row_index;
                return;
            }
        }
        self.by_key.insert(
            key.to_string(),
            MessageEntry {
                key: key.to_string(),
                content_hash,
                row_index,
                rendered_blocks: None,
                rendered_lines: None,
            },
        );
    }

    /// O(1) amortized: attach a freshly-rendered `MarkdownBlock` model to
    /// `key`'s existing entry. No-op if `key` has no entry yet -- callers
    /// must `record` first (this deliberately never creates a bare entry
    /// with no `row_index`/`content_hash`, which would make `check`
    /// return nonsensical results for it later).
    pub fn set_rendered_blocks(&mut self, key: &str, model: ModelRc<crate::MarkdownBlock>) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.rendered_blocks = Some(model);
        }
    }

    /// O(1) amortized: attach a freshly-rendered `MarkdownLine` model to
    /// `key`'s existing entry. Same no-op-without-`record`-first rule as
    /// `set_rendered_blocks`.
    pub fn set_rendered_lines(&mut self, key: &str, model: ModelRc<crate::MarkdownLine>) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.rendered_lines = Some(model);
        }
    }

    /// O(1) amortized: the cheap `ModelRc` clone (not a re-render) for
    /// `key`'s already-rendered blocks, if any render has completed
    /// since its content was last recorded.
    pub fn rendered_blocks_for(&self, key: &str) -> Option<ModelRc<crate::MarkdownBlock>> {
        self.by_key.get(key).and_then(|e| e.rendered_blocks.clone())
    }

    /// O(1) amortized: the cheap `ModelRc` clone for `key`'s
    /// already-rendered lines, mirroring `rendered_blocks_for`.
    pub fn rendered_lines_for(&self, key: &str) -> Option<ModelRc<crate::MarkdownLine>> {
        self.by_key.get(key).and_then(|e| e.rendered_lines.clone())
    }

    /// O(N): rebuild from a fresh `(keys, texts)` pair, positional by
    /// index. The only path that should run when the transcript's
    /// structure changed (insert/remove/reorder) or on cold hydration --
    /// see `keys_match` for detecting when that's actually necessary.
    ///
    /// A merge, not a wipe: for a key whose text is unchanged from what
    /// `self` already had recorded (same key, same `content_hash`), the
    /// previously-rendered `ModelRc`s carry over into the new entry even
    /// though its `row_index` may have shifted. Only genuinely new keys
    /// or keys whose text actually changed start with `rendered_*: None`.
    /// This is the reason a structural change elsewhere in the
    /// transcript (e.g. one new message appended) does not force
    /// re-rendering every other already-rendered row -- the exact O(N+M)
    /// cost this cache exists to avoid. Call on `self` (not as a bare
    /// constructor) so there's always a prior index to merge against;
    /// use `ThreadMessageIndex::default().rebuild(...)` for the very
    /// first build (cold hydration).
    pub fn rebuild<'a>(&self, keys: &[String], texts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut by_key = HashMap::with_capacity(keys.len());
        for (row_index, (key, text)) in keys.iter().zip(texts).enumerate() {
            let content_hash = hash_content(text);
            let (rendered_blocks, rendered_lines) = match self.by_key.get(key) {
                Some(prev) if prev.content_hash == content_hash => {
                    (prev.rendered_blocks.clone(), prev.rendered_lines.clone())
                }
                _ => (None, None),
            };
            by_key.insert(
                key.clone(),
                MessageEntry {
                    key: key.clone(),
                    content_hash,
                    row_index,
                    rendered_blocks,
                    rendered_lines,
                },
            );
        }
        Self { by_key }
    }

    /// O(N): whether `new_keys` (in order) matches what this index was
    /// last built/recorded from -- both same set *and* same positions.
    /// `false` means a structural change happened (insert/remove/
    /// reorder) and `rebuild` is required; `true` means every key is
    /// still at the row index this index already has recorded, so
    /// per-key `check`/`record` calls are sufficient.
    pub fn keys_match(&self, new_keys: &[String]) -> bool {
        if new_keys.len() != self.by_key.len() {
            return false;
        }
        new_keys
            .iter()
            .enumerate()
            .all(|(row_index, key)| self.by_key.get(key).map(|e| e.row_index) == Some(row_index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rebuild_records_row_index_and_content_hash_per_key() {
        let k = keys(&["user:1", "assistant:1"]);
        let idx = ThreadMessageIndex::default().rebuild(&k, ["hello", "world"]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.check("user:1", "hello"), RowChange::Unchanged(0));
        assert_eq!(idx.check("assistant:1", "world"), RowChange::Unchanged(1));
    }

    #[test]
    fn check_reports_new_for_a_key_never_recorded() {
        let idx = ThreadMessageIndex::default();
        assert_eq!(idx.check("user:1", "hello"), RowChange::New);
    }

    #[test]
    fn check_reports_unchanged_when_content_hash_matches() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "partial");
        assert_eq!(idx.check("assistant:1", "partial"), RowChange::Unchanged(0));
    }

    #[test]
    fn check_reports_changed_when_content_hash_differs_streaming_case() {
        // The exact case this exists for: an agent message's text grows
        // as tokens stream in, same key, same row_index, different text.
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 3, "partial");
        assert_eq!(idx.check("assistant:1", "partial text grew"), RowChange::Changed(3));
    }

    #[test]
    fn record_after_changed_makes_the_next_check_unchanged() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "partial");
        assert_eq!(idx.check("assistant:1", "partial more"), RowChange::Changed(0));
        idx.record("assistant:1", 0, "partial more");
        assert_eq!(idx.check("assistant:1", "partial more"), RowChange::Unchanged(0));
    }

    #[test]
    fn record_does_not_disturb_other_entries() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("user:1", 0, "hello");
        idx.record("assistant:1", 1, "world");
        idx.record("assistant:1", 1, "world changed");
        assert_eq!(idx.check("user:1", "hello"), RowChange::Unchanged(0));
        assert_eq!(idx.check("assistant:1", "world changed"), RowChange::Unchanged(1));
    }

    #[test]
    fn keys_match_true_when_same_keys_same_positions() {
        let k = keys(&["user:1", "assistant:1"]);
        let idx = ThreadMessageIndex::default().rebuild(&k, ["a", "b"]);
        assert!(idx.keys_match(&k));
    }

    #[test]
    fn keys_match_false_on_insert() {
        let old = keys(&["user:1", "assistant:1"]);
        let idx = ThreadMessageIndex::default().rebuild(&old, ["a", "b"]);
        let new = keys(&["user:1", "tool:1", "assistant:1"]);
        assert!(!idx.keys_match(&new));
    }

    #[test]
    fn keys_match_false_on_remove() {
        let old = keys(&["user:1", "assistant:1"]);
        let idx = ThreadMessageIndex::default().rebuild(&old, ["a", "b"]);
        let new = keys(&["assistant:1"]);
        assert!(!idx.keys_match(&new));
    }

    #[test]
    fn keys_match_false_on_reorder_same_set() {
        let old = keys(&["user:1", "assistant:1"]);
        let idx = ThreadMessageIndex::default().rebuild(&old, ["a", "b"]);
        let reordered = keys(&["assistant:1", "user:1"]);
        assert!(!idx.keys_match(&reordered));
    }

    #[test]
    fn hash_content_is_stable_within_a_process_and_sensitive_to_content() {
        assert_eq!(hash_content("same"), hash_content("same"));
        assert_ne!(hash_content("same"), hash_content("different"));
    }

    #[test]
    fn empty_index_reports_empty_and_zero_length() {
        let idx = ThreadMessageIndex::default();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn rebuild_on_empty_keys_is_empty() {
        let idx = ThreadMessageIndex::default().rebuild(&[], std::iter::empty());
        assert!(idx.is_empty());
        assert!(idx.keys_match(&[]));
    }

    #[test]
    fn realistic_streaming_sequence_only_patches_the_tail_row() {
        // Mirrors the actual shape of a live turn: transcript grows by
        // one user + one assistant row, then the assistant row's text
        // streams in place across several ticks, then a new user turn
        // starts (structural change again).
        let k1 = keys(&["user:1"]);
        let mut idx = ThreadMessageIndex::default().rebuild(&k1, ["hi"]);
        assert!(idx.keys_match(&k1));

        // Structural: assistant row appears.
        let k2 = keys(&["user:1", "assistant:1"]);
        assert!(!idx.keys_match(&k2));
        idx = idx.rebuild(&k2, ["hi", ""]);

        // Streaming: same keys/positions, only assistant:1's text grows.
        for text in ["h", "he", "hel", "hell", "hello"] {
            assert!(idx.keys_match(&k2));
            match idx.check("assistant:1", text) {
                RowChange::Changed(row_index) => {
                    assert_eq!(row_index, 1);
                    idx.record("assistant:1", row_index, text);
                }
                other => panic!("expected Changed(1), got {other:?}"),
            }
        }
        // Settled: re-checking the same final text is a no-op signal.
        assert_eq!(idx.check("assistant:1", "hello"), RowChange::Unchanged(1));
        assert_eq!(idx.check("user:1", "hi"), RowChange::Unchanged(0));
    }

    // -- rendered-payload storage (markdown-render-cache-layer Phase 1) --

    fn dummy_blocks_model() -> ModelRc<crate::MarkdownBlock> {
        ModelRc::new(slint::VecModel::from(vec![crate::MarkdownBlock::default()]))
    }

    fn dummy_lines_model() -> ModelRc<crate::MarkdownLine> {
        ModelRc::new(slint::VecModel::from(vec![crate::MarkdownLine::default()]))
    }

    #[test]
    fn rendered_blocks_for_is_none_before_any_render() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "hello");
        assert!(idx.rendered_blocks_for("assistant:1").is_none());
        assert!(idx.rendered_lines_for("assistant:1").is_none());
    }

    #[test]
    fn set_rendered_blocks_then_rendered_blocks_for_returns_it() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "hello");
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());
        assert!(idx.rendered_blocks_for("assistant:1").is_some());
    }

    #[test]
    fn set_rendered_lines_then_rendered_lines_for_returns_it() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "hello");
        idx.set_rendered_lines("assistant:1", dummy_lines_model());
        assert!(idx.rendered_lines_for("assistant:1").is_some());
    }

    #[test]
    fn set_rendered_blocks_without_a_prior_record_is_a_no_op() {
        // No entry exists for this key -- must not fabricate one with a
        // bogus row_index/content_hash.
        let mut idx = ThreadMessageIndex::default();
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());
        assert!(idx.is_empty());
        assert!(idx.rendered_blocks_for("assistant:1").is_none());
    }

    #[test]
    fn record_clears_previously_rendered_payload_for_the_same_key() {
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "hello");
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());
        assert!(idx.rendered_blocks_for("assistant:1").is_some());

        // Content changed -- record() must invalidate the stale render.
        idx.record("assistant:1", 0, "hello world");
        assert!(idx.rendered_blocks_for("assistant:1").is_none());
    }

    #[test]
    fn record_with_matching_hash_does_not_wipe_an_already_rendered_payload() {
        // Live-caught regression (real e2e run, not originally caught by
        // any unit test): models.rs's markdown_lines_for and
        // markdown_blocks_for both call record() for the *same key* on
        // the *same shared index* while building one "agent" row (one
        // needs a lines render, the other a blocks render). The old
        // record() unconditionally wiped both rendered_blocks and
        // rendered_lines on every call, so lines' record() call wiped
        // out blocks' just-cached payload and vice versa -- an infinite
        // ping-pong where check() reported the row as changed forever
        // even though its text never actually changed, defeating the
        // whole cache for every agent message in production. This test
        // reproduces the exact call sequence live investigation found:
        // record+set_rendered_blocks, then a second, unrelated record()
        // call for the same key/same text (standing in for the other
        // caller), must NOT clear the first render.
        let mut idx = ThreadMessageIndex::default();
        idx.record("assistant:1", 0, "hello");
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());
        assert!(idx.rendered_blocks_for("assistant:1").is_some());

        // Second caller's record() for the identical key/identical text
        // (same content_hash) -- must be a no-op with respect to the
        // already-cached blocks payload.
        idx.record("assistant:1", 0, "hello");
        assert!(
            idx.rendered_blocks_for("assistant:1").is_some(),
            "a second record() call with unchanged content must not wipe an already-rendered payload"
        );
        idx.set_rendered_lines("assistant:1", dummy_lines_model());

        // Simulates the next poll tick: both callers run again with the
        // exact same settled text. Neither should ever report anything
        // other than Unchanged from this point on, and both payloads
        // must still be present together.
        idx.record("assistant:1", 0, "hello");
        assert_eq!(idx.check("assistant:1", "hello"), RowChange::Unchanged(0));
        assert!(idx.rendered_blocks_for("assistant:1").is_some());
        assert!(idx.rendered_lines_for("assistant:1").is_some());
    }

    #[test]
    fn rebuild_preserves_rendered_payload_when_text_is_unchanged() {
        // The actual point of this whole module: appending a brand-new
        // message elsewhere in the transcript is a structural change
        // (keys_match -> false, rebuild required), but it must NOT force
        // every already-rendered row to re-render -- only the new row
        // should come back without a cached render.
        let k1 = keys(&["user:1", "assistant:1"]);
        let mut idx = ThreadMessageIndex::default().rebuild(&k1, ["hi", "hello world"]);
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());
        assert!(idx.rendered_blocks_for("assistant:1").is_some());

        // Structural: a new user turn appended after assistant:1.
        let k2 = keys(&["user:1", "assistant:1", "user:2"]);
        assert!(!idx.keys_match(&k2));
        let idx2 = idx.rebuild(&k2, ["hi", "hello world", "another question"]);

        // assistant:1's text is byte-for-byte unchanged -> render reused.
        assert!(
            idx2.rendered_blocks_for("assistant:1").is_some(),
            "unrelated structural change must not drop an unchanged row's cached render"
        );
        // The brand-new row has nothing rendered yet.
        assert!(idx2.rendered_blocks_for("user:2").is_none());
        assert_eq!(idx2.check("assistant:1", "hello world"), RowChange::Unchanged(1));
    }

    #[test]
    fn rebuild_drops_rendered_payload_when_text_actually_changed() {
        // Same key, same nominal slot, but the content itself changed
        // (e.g. a tool-call row whose title/body was replaced) --
        // carrying the old render over here would show stale content.
        let k = keys(&["assistant:1"]);
        let mut idx = ThreadMessageIndex::default().rebuild(&k, ["draft"]);
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());

        let idx2 = idx.rebuild(&k, ["final"]);
        assert!(idx2.rendered_blocks_for("assistant:1").is_none());
        assert_eq!(idx2.check("assistant:1", "final"), RowChange::Unchanged(0));
    }

    #[test]
    fn rebuild_drops_rendered_payload_for_a_removed_key_entirely() {
        let k1 = keys(&["user:1", "assistant:1"]);
        let mut idx = ThreadMessageIndex::default().rebuild(&k1, ["hi", "hello"]);
        idx.set_rendered_blocks("assistant:1", dummy_blocks_model());

        let k2 = keys(&["user:1"]);
        let idx2 = idx.rebuild(&k2, ["hi"]);
        assert_eq!(idx2.len(), 1);
        assert!(idx2.rendered_blocks_for("assistant:1").is_none());
    }
}
