//! Client-side, per-thread queue for messages typed while a turn is still
//! in flight -- `chat-view-ui`'s "always allow typing + a persisted runtime
//! queue" task.
//!
//! Shape copied directly from Zed's own agent panel
//! (`agent_ui::conversation_view::message_queue::MessageQueue`), since
//! ACPX has no server-side steering RPC and Zed's own queue turns out not
//! to need one either: this is a plain in-memory `VecDeque` plus a small
//! state machine, and "steer" is just a per-entry flag meaning "cancel the
//! in-flight turn and send this one now" rather than a protocol feature.
//! See `memory/designa/gen/plans/chat-ui-responsive-polish/01-architecture.md`.
//!
//! Persistence is JSONL, not SQLite (unlike `state_store.rs`'s thread
//! identity table) -- one `<thread_id>.sendqueue.jsonl` file per thread,
//! alongside `jsonl_store.rs`'s `<thread_id>.jsonl` transcript cache in the
//! same cache directory. Every mutation rewrites the whole file: queue
//! depth is expected to stay small (a handful of pending messages at
//! most), so this is simpler and just as correct as true append-only
//! JSONL with tombstones, without the bookkeeping.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueEntryId(pub u64);

/// One pending message. Only the front entry's `steer` value matters,
/// since entries are delivered in FIFO order -- matches
/// `MessageQueue::front_wants_steer`'s doc comment in Zed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedEntry {
    text: String,
    steer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: QueueEntryId,
    pub text: String,
    pub steer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingState {
    AutoProcess,
    Paused,
    // Sending a message out of turn cancelled the current generation; we
    // must absorb the Stopped event from that cancellation before
    // resuming auto-processing, otherwise the queue would double-send.
    AbsorbingCancel,
}

/// Holds follow-up messages typed while the agent is generating, along
/// with the state machine that decides when they're auto-sent.
#[derive(Debug, Clone, PartialEq)]
pub struct SendQueue {
    entries: VecDeque<QueueEntry>,
    processing_state: ProcessingState,
    can_fast_track: bool,
    next_id: u64,
    persist_path: Option<PathBuf>,
    remote_ids: HashMap<QueueEntryId, String>,
}

impl Default for SendQueue {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            processing_state: ProcessingState::AutoProcess,
            can_fast_track: false,
            next_id: 0,
            persist_path: None,
            remote_ids: HashMap::new(),
        }
    }
}

/// `<thread_id>.sendqueue.jsonl` in the same cache directory
/// `jsonl_store.rs`'s `<thread_id>.jsonl` transcript files live in --
/// distinct suffix so the two never collide.
pub fn send_queue_path(cache_dir: &Path, thread_id: &str) -> PathBuf {
    cache_dir.join(format!("{thread_id}.sendqueue.jsonl"))
}

impl SendQueue {
    /// Replace the UI projection from the ACPX server-owned queue stream.
    /// This deliberately does not call `persist`: the server JSONL is the
    /// durable source of truth after the ACPX migration.
    pub fn replace_remote<I>(&mut self, entries: I, paused: bool)
    where
        I: IntoIterator<Item = String>,
    {
        self.entries.clear();
        self.next_id = 0;
        self.remote_ids.clear();
        for text in entries {
            let id = self.next_id();
            self.entries.push_back(QueueEntry {
                id,
                text,
                steer: false,
            });
        }
        self.processing_state = if paused {
            ProcessingState::Paused
        } else {
            ProcessingState::AutoProcess
        };
        self.can_fast_track = false;
    }

    /// Replaces the UI projection while retaining authoritative server ids.
    ///
    /// Local-only entries -- ones `enqueue`d optimistically that have never
    /// received a remote id, meaning their own `mutate_queue` "enqueue" RPC
    /// is still in flight -- survive a snapshot that doesn't mention them,
    /// instead of being dropped by it. `spawn_session_live_forwarder`
    /// (`gateway_actor/thread_actor.rs`) fetches this thread's queue
    /// snapshot via `acpx/sessions/queue/subscribe` the moment a session id
    /// becomes known -- which is *before* `AgentBridge::complete_attachment`
    /// fires, i.e. before the attachment gate `mutate_queue` itself waits on
    /// opens. So a message queued while the very first turn on a brand-new
    /// thread is still attaching reliably loses this race: the snapshot
    /// fetch reports the session's genuinely-empty pre-enqueue queue and
    /// arrives back well before the client's own enqueue mutation has even
    /// been sent, let alone confirmed. Blindly clearing on that snapshot
    /// wiped the QueuedMessageBar row the user was looking at for the rest
    /// of the in-flight turn (its real confirmation is often held behind
    /// the current turn's own update batch and only flushes once that turn
    /// ends) -- see `host_e2e_mcp_driver.py`'s `queue-during-init` debug
    /// scenario, which reproduced this every run. A `remote_ids`-confirmed
    /// entry that's absent from the snapshot is still trusted as a genuine
    /// removal (e.g. it was actually sent) -- only entries this client has
    /// never had confirmed are protected.
    ///
    /// Matching is by *multiset* of text, not plain set membership: a real
    /// user queuing two back-to-back messages with identical text (e.g. a
    /// double-tap, or genuinely repeating themselves) during this same
    /// attach-pending window used to lose one of them here -- a `HashSet`
    /// of incoming texts treats "confirms one" and "confirms both" the
    /// same way, so the *second* unconfirmed duplicate got wrongly read as
    /// "already accounted for" and silently dropped instead of surviving
    /// as its own still-unconfirmed entry. Each incoming occurrence of a
    /// given text now retires at most one matching local-only entry.
    ///
    /// Returns the text of every entry that was dropped *because it was
    /// already confirmed* (see the doc comment above: "a genuine drain,
    /// e.g. it was sent"), never a still-unconfirmed survivor. The caller
    /// is expected to treat this as "these queued messages just became
    /// real prompts" -- e.g. `dispatch.rs`'s auto-drain handling of a
    /// `server_queue` thread's `QueueChanged` event, which never gets an
    /// `Effect::SendPrompt` of its own for a server-owned queue (ACPX's
    /// `spawn_queue_dispatcher` sends the real `session/prompt` itself),
    /// so nothing else ever calls `AgentBridge::push_local` for that
    /// message the way the immediate-send path
    /// (`PanelSingleton::start_send_prompt`) does. Without surfacing this,
    /// the user's own queued prompt text is never recorded into the
    /// transcript at all once it silently vanishes from the queue
    /// projection here.
    pub fn replace_remote_items<I>(&mut self, entries: I, paused: bool) -> Vec<String>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let entries: Vec<(String, String)> = entries.into_iter().collect();
        let old_entries: Vec<QueueEntry> = self.entries.drain(..).collect();
        let old_remote_ids = std::mem::take(&mut self.remote_ids);
        let incoming_ids: std::collections::HashSet<&str> =
            entries.iter().map(|(id, _)| id.as_str()).collect();
        let mut dispatched_texts = Vec::new();
        let mut confirmed_by_id: HashMap<String, QueueEntry> = HashMap::new();
        let mut unconfirmed = Vec::new();
        for entry in old_entries {
            match old_remote_ids.get(&entry.id) {
                Some(remote_id) if !incoming_ids.contains(remote_id.as_str()) => {
                    dispatched_texts.push(entry.text.clone());
                }
                Some(remote_id) => {
                    confirmed_by_id.insert(remote_id.clone(), entry);
                }
                None => unconfirmed.push(entry),
            }
        }
        self.processing_state = if paused {
            ProcessingState::Paused
        } else {
            ProcessingState::AutoProcess
        };
        for (remote_id, text) in entries {
            let mut entry = if let Some(entry) = confirmed_by_id.remove(&remote_id) {
                entry
            } else if let Some(index) = unconfirmed.iter().position(|entry| entry.text == text) {
                unconfirmed.remove(index)
            } else {
                let id = self.next_id();
                QueueEntry {
                    id,
                    text: text.clone(),
                    steer: false,
                }
            };
            entry.text = text;
            self.remote_ids.insert(entry.id, remote_id);
            self.entries.push_back(entry);
        }
        // Preserve optimistic local entries that have not yet received a
        // server id; never let a delayed snapshot erase them.
        for entry in unconfirmed {
            self.entries.push_back(entry);
            self.can_fast_track = true;
        }
        if self.entries.is_empty() {
            self.can_fast_track = false;
        }
        dispatched_texts
    }

    pub fn remote_id_for(&self, id: QueueEntryId) -> Option<&str> {
        self.remote_ids.get(&id).map(String::as_str)
    }

    /// Removes the first server-owned row that has just arrived in the
    /// transcript as a user message.  The queue stream normally removes the
    /// row first, but `session/resume` may deliver the prompt before that
    /// notification; applying this idempotently keeps the UI from showing
    /// the same text both as queued and as a transcript message.
    pub fn dequeue_matching_remote_message(&mut self, text: &str) -> bool {
        let Some(index) = self.entries.iter().position(|entry| entry.text == text) else {
            return false;
        };
        let Some(removed) = self.entries.remove(index) else {
            return false;
        };
        // Keep the remaining local entry ids stable.  They are the keys used
        // by the reducer for later cancel/send-now actions; only the removed
        // row's authoritative remote id needs to disappear.
        self.remote_ids.remove(&removed.id);
        true
    }

    /// In-memory only, no restart survival -- used by callers that manage
    /// their own persistence path (or tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty queue that persists going forward, without reading
    /// anything from disk -- for a brand-new thread that provably has no
    /// prior queue file yet. No I/O (unlike `load`), so this is safe to
    /// call from `update()`'s pure reducer; use `load` instead when a
    /// thread's queue might already have content on disk (cold-start
    /// restoration).
    pub fn new_with_path(path: PathBuf) -> Self {
        Self {
            persist_path: Some(path),
            ..Self::default()
        }
    }

    /// Loads a previously-persisted queue, or an empty one if the file
    /// doesn't exist yet (a fresh thread, not an error). Entry ids are
    /// reassigned in file order on load -- ids are only ever compared
    /// within a single process's lifetime (e.g. `toggle_steer`,
    /// `send_now`), never persisted themselves.
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let mut queue = Self {
            persist_path: Some(path.clone()),
            ..Self::default()
        };
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(queue),
            Err(e) => return Err(e),
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let persisted: PersistedEntry = serde_json::from_str(&line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let id = queue.next_id();
            queue.entries.push_back(QueueEntry {
                id,
                text: persisted.text,
                steer: persisted.steer,
            });
        }
        queue.can_fast_track = false;
        Ok(queue)
    }

    fn persist(&self) -> io::Result<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let mut out = String::new();
        for entry in &self.entries {
            let persisted = PersistedEntry {
                text: entry.text.clone(),
                steer: entry.steer,
            };
            out.push_str(&serde_json::to_string(&persisted)?);
            out.push('\n');
        }
        // Whole-file rewrite (see module doc) -- write to a temp file and
        // rename so a crash mid-write never leaves a truncated queue file
        // behind for the next `load` to choke on.
        let tmp_path = path.with_extension("jsonl.tmp");
        let mut tmp = fs::File::create(&tmp_path)?;
        tmp.write_all(out.as_bytes())?;
        // audit-fixes offload_persist_sync_all: avoid sync_all on UI path.
        drop(tmp);
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn first(&self) -> Option<&QueueEntry> {
        self.entries.front()
    }

    pub fn first_id(&self) -> Option<QueueEntryId> {
        self.entries.front().map(|entry| entry.id)
    }

    pub fn last_id(&self) -> Option<QueueEntryId> {
        self.entries.back().map(|entry| entry.id)
    }

    /// Whether the next message should interrupt the agent at the next
    /// turn boundary instead of waiting for generation to complete.
    pub fn front_wants_steer(&self) -> bool {
        self.entries.front().is_some_and(|entry| entry.steer)
    }

    pub fn toggle_steer(&mut self, id: QueueEntryId) -> io::Result<()> {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.steer = !entry.steer;
        }
        self.persist()
    }

    pub fn iter(&self) -> impl Iterator<Item = &QueueEntry> {
        self.entries.iter()
    }

    pub fn can_fast_track(&self) -> bool {
        self.can_fast_track && !self.entries.is_empty()
    }

    pub fn entry_by_id(&self, id: QueueEntryId) -> Option<&QueueEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    fn next_id(&mut self) -> QueueEntryId {
        let id = QueueEntryId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Queuing a message is active engagement, so it also resumes
    /// auto-processing if the queue was paused.
    pub fn enqueue(&mut self, text: String, steer: bool) -> io::Result<QueueEntryId> {
        let id = self.next_id();
        self.entries.push_back(QueueEntry { id, text, steer });
        self.processing_state = ProcessingState::AutoProcess;
        self.can_fast_track = true;
        self.persist()?;
        Ok(id)
    }

    pub fn remove(&mut self, id: QueueEntryId) -> io::Result<Option<QueueEntry>> {
        let index = self.entries.iter().position(|entry| entry.id == id);
        let removed = index.and_then(|i| self.entries.remove(i));
        self.persist()?;
        Ok(removed)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        self.can_fast_track = false;
        self.persist()
    }

    /// Pops the front entry if a fast-track send is allowed (the user just
    /// queued a message and pressed Enter on an empty compose box). Works
    /// even while paused -- pressing Enter is an explicit user action,
    /// distinct from auto-processing.
    pub fn try_fast_track(&mut self, is_generating: bool) -> io::Result<Option<QueueEntry>> {
        // Must go through the entries-aware `can_fast_track()` getter, not
        // the raw field: `remove`/`send_now`/`on_generation_stopped` all
        // pop or drop entries without resetting `self.can_fast_track`, so
        // the flag alone can stay stuck `true` after the queue has already
        // gone empty (e.g. the armed entry auto-drained via
        // `on_generation_stopped`, or the user deleted it). Gating on the
        // raw field let a later empty-compose Return arm
        // `ProcessingState::AbsorbingCancel` for a cancel that never
        // happens (no entry to promote means `update()`'s `Ok(None)` arm
        // emits no `CancelGeneration`), which then silently swallowed the
        // *next* real turn-end's queue auto-drain -- see
        // `queue_fast_track_stale_flag_with_empty_queue_is_a_true_no_op`.
        if !self.can_fast_track() {
            self.can_fast_track = false;
            return Ok(None);
        }
        self.can_fast_track = false;
        let entry = self.entries.pop_front();
        self.processing_state = if is_generating {
            ProcessingState::AbsorbingCancel
        } else {
            ProcessingState::AutoProcess
        };
        self.persist()?;
        Ok(entry)
    }

    /// Handles a generation-stopped event, returning the entry to
    /// auto-send, if any.
    pub fn on_generation_stopped(
        &mut self,
        is_compose_focused: bool,
    ) -> io::Result<Option<QueueEntry>> {
        let popped = match self.processing_state {
            ProcessingState::AbsorbingCancel => {
                // This Stopped event came from a cancellation we
                // initiated ourselves (e.g. steer/"send now"); swallow it
                // and resume, do not also pop the next entry.
                self.processing_state = ProcessingState::AutoProcess;
                None
            }
            ProcessingState::Paused => None,
            ProcessingState::AutoProcess => {
                // Don't auto-send while the user is actively editing the
                // next message.
                if is_compose_focused {
                    None
                } else {
                    self.entries.pop_front()
                }
            }
        };
        if popped.is_some() {
            self.persist()?;
        }
        Ok(popped)
    }

    /// Removes an entry for an explicit "send now" / steer. If a
    /// generation is in flight, the caller must cancel it; the resulting
    /// Stopped event needs absorbing so the queue doesn't double-send.
    pub fn send_now(
        &mut self,
        id: QueueEntryId,
        is_generating: bool,
    ) -> io::Result<Option<QueueEntry>> {
        let index = self.entries.iter().position(|entry| entry.id == id);
        let entry = index.and_then(|i| self.entries.remove(i));
        if entry.is_some() && is_generating {
            self.processing_state = ProcessingState::AbsorbingCancel;
        }
        if entry.is_some() {
            self.persist()?;
        }
        Ok(entry)
    }

    /// Called when the user manually stops generation; queued messages
    /// stay put until the user re-engages.
    pub fn pause(&mut self) {
        self.processing_state = ProcessingState::Paused;
    }

    /// Called when the user sends a new message, re-enabling
    /// auto-processing -- un-freezes the queue after a manual stop.
    pub fn resume(&mut self) {
        self.processing_state = ProcessingState::AutoProcess;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_then_on_generation_stopped_delivers_in_fifo_order() {
        let mut q = SendQueue::new();
        q.enqueue("first".into(), false).unwrap();
        q.enqueue("second".into(), false).unwrap();
        assert_eq!(q.len(), 2);
        let popped = q.on_generation_stopped(false).unwrap().unwrap();
        assert_eq!(popped.text, "first");
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn on_generation_stopped_does_not_pop_while_compose_focused() {
        let mut q = SendQueue::new();
        q.enqueue("queued".into(), false).unwrap();
        assert!(q.on_generation_stopped(true).unwrap().is_none());
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn paused_queue_does_not_auto_advance() {
        let mut q = SendQueue::new();
        q.enqueue("queued".into(), false).unwrap();
        q.pause();
        assert!(q.on_generation_stopped(false).unwrap().is_none());
        assert_eq!(q.len(), 1);
        q.resume();
        assert!(q.on_generation_stopped(false).unwrap().is_some());
    }

    #[test]
    fn absorbing_cancel_swallows_one_stopped_event_without_popping() {
        let mut q = SendQueue::new();
        q.enqueue("a".into(), false).unwrap();
        q.enqueue("b".into(), false).unwrap();
        // Simulate a fast-track send while generating: pops "a" and
        // arms AbsorbingCancel.
        let sent = q.try_fast_track(true).unwrap().unwrap();
        assert_eq!(sent.text, "a");
        // The cancellation this triggers eventually fires a Stopped event;
        // it must be absorbed, not treated as a real turn completion that
        // pops "b" too.
        assert!(q.on_generation_stopped(false).unwrap().is_none());
        assert_eq!(q.len(), 1);
        // Now a real Stopped event pops "b" normally.
        let popped = q.on_generation_stopped(false).unwrap().unwrap();
        assert_eq!(popped.text, "b");
    }

    // Regression for the empty-Return "cancels the previous turn and forces
    // some unknown behaviour" report: `can_fast_track` (the raw field) used
    // to be gated on directly, but it goes stale -- `remove`/`send_now`/
    // `on_generation_stopped` can all empty out `entries` without resetting
    // it. A later empty-compose Return then saw the stale `true` and armed
    // `AbsorbingCancel` for a cancel that was never actually going to
    // happen (no entry to promote, so `update()`'s `Ok(None)` arm never
    // emits `CancelGeneration`), which then swallowed the *next* real
    // turn's completion instead of letting it auto-drain normally.
    #[test]
    fn queue_fast_track_stale_flag_with_empty_queue_is_a_true_no_op() {
        let mut q = SendQueue::new();
        q.enqueue("a".into(), false).unwrap();
        // Drains "a" via a real turn completion -- `can_fast_track` is left
        // stale `true` on the raw field (only `try_fast_track`/`enqueue`/
        // `replace_remote`/`clear` reset it; a plain auto-drain doesn't).
        let popped = q.on_generation_stopped(false).unwrap().unwrap();
        assert_eq!(popped.text, "a");
        assert!(q.is_empty());
        // A second, unrelated turn is now generating. Pressing empty-Enter
        // here must be a genuine no-op: nothing to promote, and it must
        // NOT arm AbsorbingCancel for a cancel effect that will never be
        // emitted (checked directly on the internal state machine below --
        // the buggy version left `try_fast_track` unable to tell a real
        // fast-track from this stale-flag/empty-queue case since both
        // return `Ok(None)` from the caller's point of view).
        assert!(q.try_fast_track(true).unwrap().is_none());
        assert_eq!(
            q.processing_state,
            ProcessingState::AutoProcess,
            "empty-queue fast-track attempt must not arm AbsorbingCancel for a \
             cancel that update() will never actually emit"
        );
    }

    #[test]
    fn front_wants_steer_reflects_only_the_front_entry() {
        let mut q = SendQueue::new();
        let first = q.enqueue("a".into(), false).unwrap();
        q.enqueue("b".into(), true).unwrap();
        assert!(!q.front_wants_steer());
        q.toggle_steer(first).unwrap();
        assert!(q.front_wants_steer());
    }

    #[test]
    fn send_now_removes_regardless_of_position() {
        let mut q = SendQueue::new();
        q.enqueue("a".into(), false).unwrap();
        let second = q.enqueue("b".into(), false).unwrap();
        q.enqueue("c".into(), false).unwrap();
        let removed = q.send_now(second, false).unwrap().unwrap();
        assert_eq!(removed.text, "b");
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn persists_and_reloads_across_a_fresh_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = send_queue_path(dir.path(), "thread-1");
        let mut q = SendQueue::load(path.clone()).unwrap();
        assert!(q.is_empty());
        q.enqueue("survives a restart".into(), true).unwrap();
        q.enqueue("second entry".into(), false).unwrap();

        let reloaded = SendQueue::load(path).unwrap();
        assert_eq!(reloaded.len(), 2);
        let first = reloaded.first().unwrap();
        assert_eq!(first.text, "survives a restart");
        assert!(first.steer);
    }

    #[test]
    fn load_of_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = send_queue_path(dir.path(), "never-seen");
        let q = SendQueue::load(path).unwrap();
        assert!(q.is_empty());
    }

    #[test]
    fn remove_persists_the_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = send_queue_path(dir.path(), "thread-1");
        let mut q = SendQueue::load(path.clone()).unwrap();
        let id = q.enqueue("drop me".into(), false).unwrap();
        q.enqueue("keep me".into(), false).unwrap();
        q.remove(id).unwrap();

        let reloaded = SendQueue::load(path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.first().unwrap().text, "keep me");
    }

    #[test]
    fn resume_user_message_dequeues_matching_remote_row_once() {
        let mut q = SendQueue::new();
        q.replace_remote_items(
            [
                ("remote-a".into(), "first".into()),
                ("remote-b".into(), "second".into()),
            ],
            false,
        );
        assert!(q.dequeue_matching_remote_message("first"));
        assert_eq!(q.len(), 1);
        assert_eq!(q.first().unwrap().text, "second");
        assert_eq!(q.remote_id_for(q.first_id().unwrap()), Some("remote-b"));
        assert!(!q.dequeue_matching_remote_message("first"));
    }

    #[test]
    fn resume_user_message_removes_the_matching_remote_id_after_prior_dequeue() {
        let mut q = SendQueue::new();
        q.replace_remote_items(
            [
                ("remote-a".into(), "first".into()),
                ("remote-b".into(), "second".into()),
                ("remote-c".into(), "third".into()),
            ],
            false,
        );
        assert!(q.dequeue_matching_remote_message("first"));
        assert!(q.dequeue_matching_remote_message("third"));
        assert_eq!(q.remote_id_for(q.first_id().unwrap()), Some("remote-b"));
    }

    // Regression for the "queue disappeared after session initialization"
    // report: `spawn_session_live_forwarder`'s one-time `acpx/sessions/
    // queue/subscribe` snapshot fetch (`gateway_actor/thread_actor.rs`)
    // fires the instant a session id is known -- *before*
    // `AgentBridge::complete_attachment`, i.e. before the same attachment
    // gate `mutate_queue` waits on for its own "enqueue" RPC. A message
    // queued while that very first turn is still attaching therefore
    // reliably lost this race: the snapshot fetch reports the session's
    // genuinely-empty pre-enqueue queue and its response can land back at
    // the client before the local enqueue mutation was even sent, let
    // alone confirmed -- see `host_e2e_mcp_driver.py`'s `queue-during-init`
    // debug scenario, which reproduced this on every run against a real
    // acpx-server. `replace_remote_items` used to clear unconditionally,
    // wiping the optimistically-added row for the rest of that turn (often
    // ~20s in the real repro) even though the message was never lost.
    #[test]
    fn replace_remote_items_preserves_unconfirmed_local_entry_not_in_snapshot() {
        let mut q = SendQueue::new();
        q.enqueue("queued during attach".into(), false).unwrap();
        assert_eq!(q.len(), 1);

        // A stale/early snapshot (e.g. the pre-enqueue queue state) reports
        // no items at all for this session.
        q.replace_remote_items(std::iter::empty::<(String, String)>(), false);

        // The unconfirmed local entry must survive -- it was never given a
        // remote id, so this snapshot cannot be trusted to postdate it.
        assert_eq!(q.len(), 1);
        assert_eq!(q.first().unwrap().text, "queued during attach");
        assert!(q.can_fast_track(), "a surviving entry must stay steerable");

        // The real confirmation later arrives with the entry included --
        // it must be adopted normally (remote id attached, no duplicate).
        q.replace_remote_items([("remote-1".into(), "queued during attach".into())], false);
        assert_eq!(q.len(), 1);
        assert_eq!(
            q.remote_id_for(q.first_id().unwrap()),
            Some("remote-1"),
            "the survivor must adopt its real remote id once confirmed"
        );
    }

    #[test]
    fn replace_remote_items_still_drops_a_confirmed_entry_absent_from_snapshot() {
        let mut q = SendQueue::new();
        q.replace_remote_items(
            [
                ("remote-a".into(), "first".into()),
                ("remote-b".into(), "second".into()),
            ],
            false,
        );
        // A later snapshot omits "first" -- since it already had a
        // confirmed remote id, this is a genuine drain (e.g. it was sent),
        // not a stale pre-mutation race, so it must actually be removed.
        let dispatched = q.replace_remote_items([("remote-b".into(), "second".into())], false);
        assert_eq!(q.len(), 1);
        assert_eq!(q.first().unwrap().text, "second");
        // Regression (queued-message-not-becoming-transcript-entry bug):
        // dropping a confirmed entry this way must surface its text so the
        // caller (`update.rs`'s `QueueChanged` handler) can record it into
        // the transcript the way `start_send_prompt`'s `push_local` does
        // for an immediate, non-queued send -- a `server_queue` thread's
        // auto-drained entry never gets its own `Effect::SendPrompt`
        // (ACPX's `spawn_queue_dispatcher` sends the real prompt itself),
        // so without this the user's own queued prompt text was silently
        // dropped from the UI once the queue row disappeared, even though
        // the agent visibly replied to it.
        assert_eq!(
            dispatched,
            vec!["first".to_string()],
            "a confirmed entry draining out of the snapshot must be reported so the caller can \
             record it into the transcript"
        );
    }

    /// Same regression as above, but for the still-unconfirmed survivor
    /// path: an entry that was never given a remote id must never be
    /// reported as "dispatched" just because a stale/early snapshot
    /// omits it -- it hasn't gone anywhere, it's still genuinely queued
    /// (see `replace_remote_items_preserves_unconfirmed_local_entry_not_in_snapshot`
    /// above), so recording it into the transcript here would fabricate a
    /// duplicate message that was never actually sent.
    #[test]
    fn replace_remote_items_never_reports_an_unconfirmed_survivor_as_dispatched() {
        let mut q = SendQueue::new();
        q.enqueue("queued during attach".into(), false).unwrap();
        let dispatched = q.replace_remote_items(std::iter::empty::<(String, String)>(), false);
        assert_eq!(q.len(), 1, "the unconfirmed entry must still survive");
        assert!(
            dispatched.is_empty(),
            "an unconfirmed survivor must never be reported as dispatched"
        );
    }

    /// Duplicate-text edge case: two confirmed entries sharing identical
    /// text must be told apart by their own remote id, not the text
    /// multiset -- otherwise draining one of two identical-text confirmed
    /// entries could be missed (or double-counted) because the other's
    /// text still "covers" it in a naive text-only comparison.
    #[test]
    fn replace_remote_items_reports_the_right_one_of_two_confirmed_duplicates() {
        let mut q = SendQueue::new();
        q.replace_remote_items(
            [
                ("remote-a".into(), "hello".into()),
                ("remote-b".into(), "hello".into()),
            ],
            false,
        );
        // Only "remote-a" drains; "remote-b" (identical text) is still
        // present in the new snapshot and must not be reported.
        let dispatched = q.replace_remote_items([("remote-b".into(), "hello".into())], false);
        assert_eq!(q.len(), 1);
        assert_eq!(
            dispatched,
            vec!["hello".to_string()],
            "exactly the drained duplicate's text must be reported, once"
        );
    }

    #[test]
    fn replace_remote_items_tracks_rows_by_remote_id_across_reorder() {
        let mut q = SendQueue::new();
        q.replace_remote_items(
            [
                ("remote-a".into(), "first".into()),
                ("remote-b".into(), "second".into()),
            ],
            false,
        );
        // A send-now/reorder changes positions but not queueEntryId. The
        // local rows must retain their identities and text.
        let dispatched = q.replace_remote_items(
            [
                ("remote-b".into(), "second".into()),
                ("remote-a".into(), "first".into()),
            ],
            false,
        );
        assert!(dispatched.is_empty());
        let rows: Vec<_> = q.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(rows, ["second", "first"]);
        assert_eq!(q.remote_id_for(q.first_id().unwrap()), Some("remote-b"));
    }

    /// Regression: a plain `HashSet` of incoming texts (the first version
    /// of the "queue disappeared" fix) treated "the snapshot confirms one
    /// occurrence of this text" and "confirms both" identically, so a
    /// second, still-genuinely-unconfirmed duplicate got misread as
    /// "already accounted for" and silently dropped -- a real user queuing
    /// two back-to-back messages with identical text (a double-tap, or
    /// genuinely repeating themselves) during the same attach-pending
    /// window would lose one of them. Matching must be by multiset: each
    /// incoming occurrence retires at most one matching local-only entry.
    #[test]
    fn replace_remote_items_preserves_both_unconfirmed_duplicates_with_identical_text() {
        let mut q = SendQueue::new();
        q.enqueue("hello".into(), false).unwrap();
        q.enqueue("hello".into(), false).unwrap();
        assert_eq!(q.len(), 2);

        // A stale/early snapshot confirms neither -- both duplicates must
        // survive, not just one.
        q.replace_remote_items(std::iter::empty::<(String, String)>(), false);
        assert_eq!(q.len(), 2);

        // The real confirmation arrives for only the FIRST occurrence --
        // the second must still survive as its own unconfirmed entry, not
        // be treated as already covered by the one confirmation.
        q.replace_remote_items([("remote-1".into(), "hello".into())], false);
        assert_eq!(
            q.len(),
            2,
            "confirming one occurrence of a duplicated text must not also \
             drop the still-unconfirmed second occurrence"
        );
        let confirmed_count = q
            .iter()
            .filter(|entry| q.remote_id_for(entry.id) == Some("remote-1"))
            .count();
        assert_eq!(
            confirmed_count, 1,
            "exactly one entry should have adopted the remote id"
        );
    }

    /// Three messages queued back-to-back (realistic rapid human typing,
    /// not just the single-entry case the original fix's own test
    /// covered) must all survive a stale/early snapshot together.
    #[test]
    fn replace_remote_items_preserves_multiple_distinct_unconfirmed_entries() {
        let mut q = SendQueue::new();
        q.enqueue("first message".into(), false).unwrap();
        q.enqueue("second message".into(), false).unwrap();
        q.enqueue("third message".into(), false).unwrap();
        assert_eq!(q.len(), 3);

        q.replace_remote_items(std::iter::empty::<(String, String)>(), false);
        assert_eq!(q.len(), 3);
        let texts: Vec<&str> = q.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["first message", "second message", "third message"]
        );
    }
}
