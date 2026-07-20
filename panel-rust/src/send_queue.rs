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
use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueEntryId(u64);

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
pub struct SendQueue {
    entries: VecDeque<QueueEntry>,
    processing_state: ProcessingState,
    can_fast_track: bool,
    next_id: u64,
    persist_path: Option<PathBuf>,
}

impl Default for SendQueue {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            processing_state: ProcessingState::AutoProcess,
            can_fast_track: false,
            next_id: 0,
            persist_path: None,
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
    /// In-memory only, no restart survival -- used by callers that manage
    /// their own persistence path (or tests).
    pub fn new() -> Self {
        Self::default()
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
        tmp.sync_all()?;
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
        if !self.can_fast_track {
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
}
