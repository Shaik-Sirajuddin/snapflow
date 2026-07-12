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

use crate::session_client::ChatMessage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CachedThread::default()),
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
        serde_json::to_writer(&mut file, &Line::Message(message.clone())).map_err(|source| {
            CacheError::Parse {
                path: path.clone(),
                line_no: 0,
                source,
            }
        })?;
        file.write_all(b"\n").map_err(|source| CacheError::Io { path, source })?;
        Ok(())
    }

    /// Decision 2's diff check: does `remote` (freshly fetched
    /// `session/list` metadata) match what the local trailer last recorded?
    /// `true` means the cache is stale and `session/load` should run.
    pub fn is_stale(local: Option<&ThreadTrailer>, remote_title: &Option<String>, remote_updated_at: &Option<String>) -> bool {
        match local {
            None => true, // no cache yet -- always resync
            Some(t) => &t.title != remote_title || &t.updated_at != remote_updated_at,
        }
    }
}
