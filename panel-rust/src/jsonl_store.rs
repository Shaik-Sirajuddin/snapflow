//! Per-thread jsonl session cache.
//!
//! Per `chat-panel-acp-rust-sdk.md`'s "everything else from the prior plan
//! still applies" section (Decision 1): this is a client-side *cache*, not
//! a durable store. Losing the file is a cache miss (next resync re-fills
//! it from the agent), never silent data loss, provided the bound agent
//! itself retains session history.
//!
//! One file per logical thread, named `<thread_id>.jsonl`. Each line is a
//! `ChatMessage` (see `session_client.rs`), in append order. A final
//! trailer line (`ThreadTrailer`, tagged distinctly so it round-trips
//! alongside message lines without ambiguity) records the metadata this
//! crate diffs against `session/list`'s response, per Decision 2's
//! local-first / lazy-verify sequencing.

use crate::protocol_types::{AgentRequestEvent, ChatMessage, ConfigOptionInfo, SessionModesEvent};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum CacheError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed jsonl line {line_no} in {path}: {source}")]
    Parse {
        path: PathBuf,
        line_no: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// The trailer metadata this crate diffs against a fresh `session/list`
/// response before trusting the cache is still current. Sourced from
/// `agent_client_protocol::schema::v1::SessionInfo`'s `title`/`updated_at`
/// fields -- deliberately not re-exported wire types here, so `panel-rust`
/// stays untouched by ACP schema churn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTrailer {
    pub acp_session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: usize,
}

/// One line of the jsonl file: either a cached message or the trailer.
/// Tagged so the two are unambiguous on read; the trailer is always the
/// last line written, but this crate does not rely on position alone --
/// `load` scans the whole file and keeps the last trailer seen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "line_kind", rename_all = "snake_case")]
enum Line {
    Message(ChatMessage),
    Trailer(ThreadTrailer),
}

#[derive(Debug, Clone, Default)]
pub struct CachedThread {
    pub messages: Vec<ChatMessage>,
    pub trailer: Option<ThreadTrailer>,
}

/// Durable typed UI state kept independently of the JSONL transcript and
/// trailer. The transcript remains the source for message/tool rows; this
/// sidecar preserves interaction state that cannot be reconstructed from a
/// replay alone, notably live terminal buffers, pending relayed requests,
/// and advertised configuration capabilities.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRuntimeSnapshot {
    #[serde(default)]
    pub pending_requests: Vec<AgentRequestEvent>,
    #[serde(default)]
    pub terminals: Vec<TerminalRuntimeSnapshot>,
    #[serde(default)]
    pub session_modes: Option<SessionModesEvent>,
    #[serde(default)]
    pub config_options: Vec<ConfigOptionInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRuntimeSnapshot {
    pub terminal_id: String,
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<(Option<i32>, Option<i32>)>,
}

/// Owns the cache directory; one `.jsonl` file per thread id.
#[derive(Debug, Clone)]
pub struct JsonlStore {
    dir: PathBuf,
}

impl JsonlStore {
    /// `dir` must already exist or be creatable; created eagerly so callers
    /// get an immediate, clear error rather than a lazy failure on first
    /// write.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|source| CacheError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self { dir })
    }

    fn path_for(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("{thread_id}.jsonl"))
    }

    /// Synchronous, fast-path read for the "render immediately from cache"
    /// step of Decision 2's resync sequence. Returns an empty
    /// [`CachedThread`] (not an error) if the file doesn't exist yet --
    /// that's simply a first-open / cache-miss, not a failure.
    pub fn load(&self, thread_id: &str) -> Result<CachedThread, CacheError> {
        let path = self.path_for(thread_id);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CachedThread::default())
            }
            Err(source) => return Err(CacheError::Io { path, source }),
        };
        let mut out = CachedThread::default();
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|source| CacheError::Io {
                path: path.clone(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Line = serde_json::from_str(&line).map_err(|source| CacheError::Parse {
                path: path.clone(),
                line_no: idx + 1,
                source,
            })?;
            match parsed {
                Line::Message(m) => out.messages.push(m),
                // Last trailer wins, in case a prior partial write left more
                // than one (append-only writer never does this in normal
                // operation, but a crash mid-resync could).
                Line::Trailer(t) => out.trailer = Some(t),
            }
        }
        Ok(out)
    }

    /// Overwrite the whole file with `messages` + `trailer`. Used on a
    /// detected diff (Decision 2: "append/overwrite tail"); for a full
    /// `session/load` resync we always have the complete replayed message
    /// set in hand already, so overwrite is simpler and safer than trying
    /// to reconcile a partial append.
    pub fn overwrite(
        &self,
        thread_id: &str,
        messages: &[ChatMessage],
        trailer: &ThreadTrailer,
    ) -> Result<(), CacheError> {
        let path = self.path_for(thread_id);
        let tmp_path = self.dir.join(format!("{thread_id}.jsonl.tmp"));
        let write = |path: &Path| -> std::io::Result<()> {
            let mut file = fs::File::create(path)?;
            for m in messages {
                serde_json::to_writer(&mut file, &Line::Message(m.clone()))?;
                file.write_all(b"\n")?;
            }
            serde_json::to_writer(&mut file, &Line::Trailer(trailer.clone()))?;
            file.write_all(b"\n")?;
            file.sync_all()
        };
        write(&tmp_path).map_err(|source| CacheError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &path).map_err(|source| CacheError::Io { path, source })?;
        // Atomic index rebuild (Phase 3 step 1) -- the jsonl file's
        // contents just changed wholesale, so any previously-computed
        // offsets are stale; recompute and atomically replace the
        // index file to match.
        self.rebuild_index(thread_id)?;
        // Standalone trailer file, kept in sync with the trailer line
        // `overwrite` just wrote into the jsonl file itself -- see
        // `Self::trailer`'s doc comment on why this exists.
        self.write_trailer(thread_id, trailer)?;
        Ok(())
    }

    /// Append one message, leaving the trailer as-is (the trailer is only
    /// meaningful post-resync; a bare append -- e.g. a message sent locally
    /// before its `session/update` echo arrives -- doesn't change what the
    /// next resync will diff against).
    pub fn append(&self, thread_id: &str, message: &ChatMessage) -> Result<(), CacheError> {
        let path = self.path_for(thread_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| CacheError::Io {
                path: path.clone(),
                source,
            })?;
        // The new message line will start at the file's current length
        // (an append-mode file always writes at EOF) -- captured before
        // writing so the index update below stays a single cheap
        // append-one-line operation, not a full re-scan of a
        // potentially large jsonl file on every message.
        let offset = file
            .metadata()
            .map_err(|source| CacheError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        serde_json::to_writer(&mut file, &Line::Message(message.clone())).map_err(|source| {
            CacheError::Parse {
                path: path.clone(),
                line_no: 0,
                source,
            }
        })?;
        file.write_all(b"\n")
            .map_err(|source| CacheError::Io { path, source })?;
        self.append_index_offset(thread_id, offset)?;
        Ok(())
    }

    /// Appends one offset to `<thread_id>.idx` in append mode -- the
    /// cheap-per-message counterpart to [`Self::rebuild_index`]'s
    /// full-file atomic replace, used by [`Self::append`] so a live,
    /// fast-growing thread's index update cost stays O(1) per message
    /// rather than O(file size).
    fn append_index_offset(&self, thread_id: &str, offset: u64) -> Result<(), CacheError> {
        let idx_path = self.index_path_for(thread_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&idx_path)
            .map_err(|source| CacheError::Io {
                path: idx_path.clone(),
                source,
            })?;
        writeln!(file, "{offset}").map_err(|source| CacheError::Io {
            path: idx_path,
            source,
        })?;
        Ok(())
    }

    /// Decision 2's diff check: does `remote` (freshly fetched
    /// `session/list` metadata) match what the local trailer last recorded?
    /// `true` means the cache is stale and `session/load` should run.
    pub fn is_stale(
        local: Option<&ThreadTrailer>,
        remote_title: &Option<String>,
        remote_updated_at: &Option<String>,
    ) -> bool {
        match local {
            None => true, // no cache yet -- always resync
            Some(t) => &t.title != remote_title || &t.updated_at != remote_updated_at,
        }
    }

    // -- Phase 3 addition (chat-panel-production-ui/execution-plan.md):
    // indexed tail/predecessor paging with atomic index rebuild. --

    fn index_path_for(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("{thread_id}.idx"))
    }

    fn trailer_path_for(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("{thread_id}.trailer.json"))
    }

    fn runtime_snapshot_path_for(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("{thread_id}.runtime.json"))
    }

    /// Loads one typed interaction-state snapshot. Missing files are a
    /// normal first-run condition; malformed snapshots degrade only this
    /// state surface and never prevent the transcript tail from rendering.
    pub fn runtime_snapshot(&self, thread_id: &str) -> Result<ThreadRuntimeSnapshot, CacheError> {
        let path = self.runtime_snapshot_path_for(thread_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ThreadRuntimeSnapshot::default())
            }
            Err(source) => return Err(CacheError::Io { path, source }),
        };
        serde_json::from_slice(&bytes).map_err(|source| CacheError::Parse {
            path,
            line_no: 0,
            source,
        })
    }

    /// Atomically replaces the typed interaction snapshot without touching
    /// transcript data, its index, or its trailer metadata.
    pub fn write_runtime_snapshot(
        &self,
        thread_id: &str,
        snapshot: &ThreadRuntimeSnapshot,
    ) -> Result<(), CacheError> {
        let path = self.runtime_snapshot_path_for(thread_id);
        let tmp_path = self.dir.join(format!("{thread_id}.runtime.json.tmp"));
        let bytes = serde_json::to_vec(snapshot).map_err(|source| CacheError::Parse {
            path: tmp_path.clone(),
            line_no: 0,
            source,
        })?;
        fs::write(&tmp_path, bytes).map_err(|source| CacheError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &path).map_err(|source| CacheError::Io { path, source })
    }

    /// Writes `trailer` to `<thread_id>.trailer.json`, atomically (same
    /// tmp-then-rename pattern as the jsonl/index files). A standalone
    /// file, not a line inside the jsonl file, specifically so
    /// [`Self::trailer`] can answer without touching (or even stat-ing
    /// the length of) the jsonl file at all -- the bounded-cold-start
    /// path (`AgentBridge`'s constructor calling `tail()` + `trailer()`
    /// together) must never need a full-file read just to learn the
    /// last known `acp_session_id`/`title`/`updated_at`.
    /// Updates just the trailer file, leaving the jsonl message content
    /// (and its index) completely untouched -- **the only safe way to
    /// refresh a thread's `acp_session_id`/`updated_at` when the
    /// caller's own in-memory `messages` is a bounded page, not the
    /// thread's full cached history** (Phase 3's cold-start paging: see
    /// `AgentBridge::persist_thread_snapshot`'s doc comment for why
    /// calling [`Self::overwrite`] with only a partial page would
    /// silently truncate away every older cached message on disk).
    /// Public (unlike the file-write internals above) since this is a
    /// real, intentional part of this store's API surface, not an
    /// implementation detail of [`Self::overwrite`].
    pub fn update_trailer(
        &self,
        thread_id: &str,
        trailer: &ThreadTrailer,
    ) -> Result<(), CacheError> {
        self.write_trailer(thread_id, trailer)
    }

    fn write_trailer(&self, thread_id: &str, trailer: &ThreadTrailer) -> Result<(), CacheError> {
        let path = self.trailer_path_for(thread_id);
        let tmp_path = self.dir.join(format!("{thread_id}.trailer.json.tmp"));
        let write = |path: &Path| -> std::io::Result<()> {
            let mut file = fs::File::create(path)?;
            serde_json::to_writer(&mut file, trailer)?;
            file.sync_all()
        };
        write(&tmp_path).map_err(|source| CacheError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &path).map_err(|source| CacheError::Io { path, source })?;
        Ok(())
    }

    /// The most recently written trailer, without reading the (possibly
    /// very large) jsonl file at all -- the cheap counterpart to
    /// [`Self::load`]'s trailer field, for the bounded cold-start path.
    /// `None` if no trailer has ever been written for this thread
    /// (fresh thread, or a pre-this-feature jsonl file with no
    /// standalone trailer file yet -- callers should fall back to
    /// treating this the same as "no cache yet", same as `load()`'s own
    /// `trailer: None` case, not fail).
    pub fn trailer(&self, thread_id: &str) -> Result<Option<ThreadTrailer>, CacheError> {
        let path = self.trailer_path_for(thread_id);
        match fs::read_to_string(&path) {
            Ok(content) => {
                let trailer =
                    serde_json::from_str(&content).map_err(|source| CacheError::Parse {
                        path: path.clone(),
                        line_no: 0,
                        source,
                    })?;
                Ok(Some(trailer))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CacheError::Io { path, source }),
        }
    }

    /// Scans `<thread_id>.jsonl` once, byte-for-byte, recording the
    /// starting offset of every *message* line (trailer lines excluded
    /// -- `tail`/`predecessor_page` only ever page through messages).
    /// Deliberately does not fully deserialize each line into a
    /// [`ChatMessage`] here -- only peeks the `line_kind` discriminator
    /// via `serde_json::Value`, same "operate on the raw shape, don't
    /// over-parse" convention `gateway_actor::classify_raw_update`
    /// follows -- so this stays a genuinely single-pass, allocation-light
    /// scan even for a large file. Returns an empty `Vec` (not an error)
    /// if the jsonl file doesn't exist yet.
    fn compute_offsets_from_jsonl(&self, thread_id: &str) -> Result<Vec<u64>, CacheError> {
        let path = self.path_for(thread_id);
        let content = match fs::read(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(CacheError::Io { path, source }),
        };
        let mut offsets = Vec::new();
        let mut i: usize = 0;
        while i < content.len() {
            let line_start = i;
            let newline = content[i..].iter().position(|&b| b == b'\n');
            let line_end = match newline {
                Some(p) => i + p,
                None => content.len(),
            };
            let line = &content[line_start..line_end];
            if !line.is_empty() {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                    if value.get("line_kind").and_then(|k| k.as_str()) == Some("message") {
                        offsets.push(line_start as u64);
                    }
                }
            }
            i = match newline {
                Some(p) => line_start + p + 1,
                None => content.len(),
            };
        }
        Ok(offsets)
    }

    /// Writes `offsets` to `<thread_id>.idx` (one decimal byte offset
    /// per line), atomically via the same tmp-file-then-rename pattern
    /// [`Self::overwrite`] uses for the jsonl file itself -- "atomic
    /// index rebuild" per this plan's Phase 3 step 1.
    fn write_index(&self, thread_id: &str, offsets: &[u64]) -> Result<(), CacheError> {
        let path = self.index_path_for(thread_id);
        let tmp_path = self.dir.join(format!("{thread_id}.idx.tmp"));
        let write = |path: &Path| -> std::io::Result<()> {
            let mut file = fs::File::create(path)?;
            for offset in offsets {
                writeln!(file, "{offset}")?;
            }
            file.sync_all()
        };
        write(&tmp_path).map_err(|source| CacheError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &path).map_err(|source| CacheError::Io { path, source })?;
        Ok(())
    }

    /// Recomputes the index from the jsonl file's current real contents
    /// and writes it atomically. Called unconditionally at the end of
    /// [`Self::overwrite`] (the jsonl file's own contents just changed
    /// wholesale) and as [`Self::ensure_index`]'s fallback when no
    /// index file exists yet (e.g. a jsonl file written before this
    /// paging feature existed) or looks inconsistent with the jsonl
    /// file's own presence.
    fn rebuild_index(&self, thread_id: &str) -> Result<Vec<u64>, CacheError> {
        let offsets = self.compute_offsets_from_jsonl(thread_id)?;
        self.write_index(thread_id, &offsets)?;
        Ok(offsets)
    }

    /// Reads the current index, rebuilding it first if it's missing or
    /// looks inconsistent (empty index alongside a non-empty jsonl file
    /// -- the one cheap, real corruption/pre-feature-cache signal this
    /// can check without a full re-scan on every call).
    fn ensure_index(&self, thread_id: &str) -> Result<Vec<u64>, CacheError> {
        let idx_path = self.index_path_for(thread_id);
        match fs::read_to_string(&idx_path) {
            Ok(content) => {
                let offsets: Vec<u64> = content
                    .lines()
                    .filter_map(|line| line.trim().parse().ok())
                    .collect();
                let jsonl_len = fs::metadata(self.path_for(thread_id))
                    .map(|m| m.len())
                    .unwrap_or(0);
                if offsets.is_empty() && jsonl_len > 0 {
                    self.rebuild_index(thread_id)
                } else {
                    Ok(offsets)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.rebuild_index(thread_id),
            Err(source) => Err(CacheError::Io {
                path: idx_path,
                source,
            }),
        }
    }

    /// Seeks directly to each of `offsets` in `<thread_id>.jsonl` and
    /// reads exactly the one message line starting there -- the file's
    /// other content (potentially very large) is never read or parsed.
    fn read_messages_at_offsets(
        &self,
        thread_id: &str,
        offsets: &[u64],
    ) -> Result<Vec<ChatMessage>, CacheError> {
        if offsets.is_empty() {
            return Ok(Vec::new());
        }
        let path = self.path_for(thread_id);
        let mut file = fs::File::open(&path).map_err(|source| CacheError::Io {
            path: path.clone(),
            source,
        })?;
        let mut out = Vec::with_capacity(offsets.len());
        for &offset in offsets {
            file.seek(SeekFrom::Start(offset))
                .map_err(|source| CacheError::Io {
                    path: path.clone(),
                    source,
                })?;
            // Fresh `BufReader` per line, created immediately after each
            // seek -- never carries buffered bytes across a seek, so
            // reading exactly one line per offset stays correct even
            // though the same underlying `File` handle is reused.
            let mut reader = BufReader::new(&mut file);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|source| CacheError::Io {
                    path: path.clone(),
                    source,
                })?;
            let trimmed = line.trim_end_matches(['\n', '\r']);
            let parsed: Line =
                serde_json::from_str(trimmed).map_err(|source| CacheError::Parse {
                    path: path.clone(),
                    line_no: 0,
                    source,
                })?;
            if let Line::Message(m) = parsed {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// The newest `page_size` messages, oldest-to-newest within the
    /// page -- what a freshly opened/resumed thread's transcript should
    /// render first (Phase 3 step 2: "load newest conversation page
    /// first"). `older_available` tells the caller whether a
    /// [`Self::predecessor_page`] call would return anything.
    pub fn tail(&self, thread_id: &str, page_size: usize) -> Result<MessagePage, CacheError> {
        let offsets = self.ensure_index(thread_id)?;
        let total = offsets.len();
        let start = total.saturating_sub(page_size);
        let messages = self.read_messages_at_offsets(thread_id, &offsets[start..total])?;
        Ok(MessagePage {
            messages,
            older_available: start > 0,
            oldest_loaded_index: start,
        })
    }

    /// The `page_size` messages immediately before `before_index` (an
    /// [`MessagePage::oldest_loaded_index`] from a prior [`Self::tail`]/
    /// [`Self::predecessor_page`] call) -- what a "fetch older pages
    /// asynchronously when ChatView reaches its top boundary" scroll-up
    /// request loads (Phase 3 step 2).
    pub fn predecessor_page(
        &self,
        thread_id: &str,
        before_index: usize,
        page_size: usize,
    ) -> Result<MessagePage, CacheError> {
        let offsets = self.ensure_index(thread_id)?;
        let before_index = before_index.min(offsets.len());
        let start = before_index.saturating_sub(page_size);
        let messages = self.read_messages_at_offsets(thread_id, &offsets[start..before_index])?;
        Ok(MessagePage {
            messages,
            older_available: start > 0,
            oldest_loaded_index: start,
        })
    }

    /// Total number of message lines currently indexed for `thread_id`
    /// -- lets a caller know a thread's full message count without
    /// reading any message content at all (index-file-only cost).
    pub fn message_count(&self, thread_id: &str) -> Result<usize, CacheError> {
        Ok(self.ensure_index(thread_id)?.len())
    }
}

/// One bounded page of a thread's cached transcript, as returned by
/// [`JsonlStore::tail`]/[`JsonlStore::predecessor_page`].
#[derive(Debug, Clone, Default)]
pub struct MessagePage {
    /// Oldest-to-newest within this page (matches `history`'s own
    /// append order, so callers can `splice`/prepend pages together
    /// without an extra reverse step).
    pub messages: Vec<ChatMessage>,
    /// Whether an older page (further back than this one) exists.
    pub older_available: bool,
    /// This page's first message's 0-based position in the thread's
    /// full ordered message list -- pass to a subsequent
    /// `predecessor_page` call to keep paging further back.
    pub oldest_loaded_index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol_types::{
        AgentRequestEvent, ConfigOptionInfo, ConfigOptionValue, MessageKind, SessionModeInfo,
    };

    fn msg(text: &str) -> ChatMessage {
        ChatMessage {
            kind: MessageKind::Agent,
            text: text.to_string(),
            status: None,
            id: None,
            raw_input: None,
            raw_output: None,
        }
    }

    fn trailer() -> ThreadTrailer {
        ThreadTrailer {
            acp_session_id: "s1".into(),
            title: None,
            updated_at: None,
            message_count: 0,
        }
    }

    #[test]
    fn load_round_trips_overwrite_and_reports_no_cache_as_empty_not_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");

        let empty = store.load("missing-thread").expect("load missing");
        assert!(empty.messages.is_empty());
        assert!(empty.trailer.is_none());

        let messages = vec![msg("hello"), msg("world")];
        store
            .overwrite("t1", &messages, &trailer())
            .expect("overwrite");
        let loaded = store.load("t1").expect("load t1");
        assert_eq!(loaded.messages, messages);
        assert_eq!(loaded.trailer.unwrap().acp_session_id, "s1");
    }

    #[test]
    fn append_grows_the_file_without_disturbing_the_trailer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        store
            .overwrite("t1", &[msg("first")], &trailer())
            .expect("overwrite");
        store.append("t1", &msg("second")).expect("append");
        let loaded = store.load("t1").expect("load");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].text, "second");
        assert_eq!(loaded.trailer.unwrap().acp_session_id, "s1");
    }

    #[test]
    fn tail_returns_the_newest_page_in_order_with_older_available_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        let messages: Vec<ChatMessage> = (0..10).map(|i| msg(&format!("m{i}"))).collect();
        store
            .overwrite("t1", &messages, &trailer())
            .expect("overwrite");

        let page = store.tail("t1", 3).expect("tail");
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m7", "m8", "m9"]
        );
        assert!(page.older_available);
        assert_eq!(page.oldest_loaded_index, 7);
    }

    #[test]
    fn tail_with_page_size_covering_everything_reports_no_older_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        store
            .overwrite("t1", &[msg("a"), msg("b")], &trailer())
            .expect("overwrite");
        let page = store.tail("t1", 50).expect("tail");
        assert_eq!(page.messages.len(), 2);
        assert!(!page.older_available);
        assert_eq!(page.oldest_loaded_index, 0);
    }

    #[test]
    fn predecessor_page_walks_backward_from_a_prior_page_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        let messages: Vec<ChatMessage> = (0..10).map(|i| msg(&format!("m{i}"))).collect();
        store
            .overwrite("t1", &messages, &trailer())
            .expect("overwrite");

        let tail_page = store.tail("t1", 3).expect("tail");
        let older = store
            .predecessor_page("t1", tail_page.oldest_loaded_index, 3)
            .expect("predecessor_page");
        assert_eq!(
            older
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m4", "m5", "m6"]
        );
        assert!(older.older_available);
        assert_eq!(older.oldest_loaded_index, 4);

        let earliest = store
            .predecessor_page("t1", older.oldest_loaded_index, 100)
            .expect("predecessor_page to start");
        assert_eq!(
            earliest
                .messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m0", "m1", "m2", "m3"]
        );
        assert!(!earliest.older_available);
        assert_eq!(earliest.oldest_loaded_index, 0);
    }

    #[test]
    fn appended_messages_are_immediately_visible_to_tail_via_the_incremental_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        store
            .overwrite("t1", &[msg("a")], &trailer())
            .expect("overwrite");
        store.append("t1", &msg("b")).expect("append");
        store.append("t1", &msg("c")).expect("append");

        let page = store.tail("t1", 2).expect("tail");
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert_eq!(store.message_count("t1").expect("message_count"), 3);
    }

    #[test]
    fn index_is_transparently_rebuilt_when_the_idx_file_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        store
            .overwrite("t1", &[msg("a"), msg("b"), msg("c")], &trailer())
            .expect("overwrite");
        std::fs::remove_file(dir.path().join("t1.idx")).expect("remove index file");

        let page = store.tail("t1", 2).expect("tail after index removed");
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert!(dir.path().join("t1.idx").is_file());
    }

    #[test]
    fn tail_stays_bounded_against_a_real_ten_thousand_message_jsonl_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        let big_text = "x".repeat(1024);
        let messages: Vec<ChatMessage> = (0..10_000)
            .map(|i| msg(&format!("m{i}-{big_text}")))
            .collect();
        store
            .overwrite("t1", &messages, &trailer())
            .expect("overwrite 10k messages");

        let jsonl_len = std::fs::metadata(dir.path().join("t1.jsonl"))
            .expect("jsonl metadata")
            .len();
        assert!(
            jsonl_len > 5 * 1024 * 1024,
            "expected a genuinely multi-megabyte jsonl file, got {jsonl_len} bytes"
        );

        let page = store.tail("t1", 50).expect("tail");
        assert_eq!(page.messages.len(), 50);
        assert_eq!(page.messages[0].text, "m9950-".to_string() + &big_text);
        assert_eq!(page.messages[49].text, "m9999-".to_string() + &big_text);
        assert!(page.older_available);
        assert_eq!(page.oldest_loaded_index, 9_950);

        let older = store
            .predecessor_page("t1", page.oldest_loaded_index, 20)
            .expect("predecessor_page");
        assert_eq!(older.messages.len(), 20);
        assert_eq!(older.messages[0].text, "m9930-".to_string() + &big_text);
        assert_eq!(older.messages[19].text, "m9949-".to_string() + &big_text);
    }

    #[test]
    fn trailer_is_readable_without_touching_the_jsonl_file_and_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        assert!(store.trailer("missing").expect("trailer missing").is_none());

        store
            .overwrite("t1", &[msg("a")], &trailer())
            .expect("overwrite");
        let read_back = store
            .trailer("t1")
            .expect("trailer t1")
            .expect("some trailer");
        assert_eq!(read_back.acp_session_id, "s1");

        // Deleting the jsonl file entirely must not affect trailer()'s
        // own answer -- proves this reads the standalone trailer file,
        // not the jsonl file itself.
        std::fs::remove_file(dir.path().join("t1.jsonl")).expect("remove jsonl");
        let still_readable = store.trailer("t1").expect("trailer after jsonl removed");
        assert_eq!(still_readable.unwrap().acp_session_id, "s1");
    }

    #[test]
    fn is_stale_matches_pre_existing_diff_semantics() {
        assert!(JsonlStore::is_stale(None, &None, &None));
        let t = ThreadTrailer {
            acp_session_id: "s".into(),
            title: Some("Title".into()),
            updated_at: Some("2026-01-01".into()),
            message_count: 1,
        };
        assert!(!JsonlStore::is_stale(
            Some(&t),
            &Some("Title".into()),
            &Some("2026-01-01".into())
        ));
        assert!(JsonlStore::is_stale(
            Some(&t),
            &Some("Title".into()),
            &Some("2026-02-01".into())
        ));
    }

    #[test]
    fn runtime_snapshot_round_trips_without_mutating_transcript_or_trailer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlStore::open(dir.path()).expect("open");
        store
            .overwrite("t1", &[msg("transcript")], &trailer())
            .expect("seed transcript");
        let snapshot = ThreadRuntimeSnapshot {
            pending_requests: vec![AgentRequestEvent {
                relay_id: "relay-1".into(),
                method: "terminal/create".into(),
                raw_request: serde_json::json!({"id": 7, "method": "terminal/create"}),
            }],
            terminals: vec![TerminalRuntimeSnapshot {
                terminal_id: "term-1".into(),
                output: "building\n".into(),
                truncated: false,
                exit_status: Some((Some(0), None)),
            }],
            session_modes: Some(SessionModesEvent {
                current_mode_id: "ask".into(),
                available: vec![SessionModeInfo {
                    id: "ask".into(),
                    name: "Ask".into(),
                    description: None,
                }],
            }),
            config_options: vec![ConfigOptionInfo {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: None,
                kind: "select".into(),
                current_value: Some("fast".into()),
                options: vec![ConfigOptionValue {
                    value: "fast".into(),
                    name: "Fast".into(),
                    description: None,
                }],
            }],
        };
        store
            .write_runtime_snapshot("t1", &snapshot)
            .expect("write runtime snapshot");

        assert_eq!(
            store.runtime_snapshot("t1").expect("read snapshot"),
            snapshot
        );
        assert_eq!(
            store.load("t1").expect("read transcript").messages,
            vec![msg("transcript")],
            "writing interaction state must not rewrite message JSONL"
        );
        assert_eq!(
            store
                .trailer("t1")
                .expect("read trailer")
                .expect("trailer")
                .acp_session_id,
            "s1",
            "writing interaction state must not rewrite transcript metadata"
        );
    }
}
