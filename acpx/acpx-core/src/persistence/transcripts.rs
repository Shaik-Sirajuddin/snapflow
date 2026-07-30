//! Server-owned append-only JSONL transcript storage.
//!
//! The repository is intentionally independent of the router so pagination and
//! reconciliation can be tested without spawning an ACP backend. File I/O is
//! always moved to `spawn_blocking`; callers never hold router or transport
//! locks while this repository performs disk work.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinError;

use acpx_proto::session_stream::{
    SessionPageResult, SessionTranscriptPatch, INITIAL_HISTORY_LIMIT, MAX_HISTORY_LIMIT,
    OLDER_HISTORY_LIMIT,
};

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("transcript I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcript JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("transcript task failed: {0}")]
    Task(String),
    #[error("invalid session id for transcript path: {0:?}")]
    InvalidSessionId(String),
    #[error("invalid transcript cursor: {0:?}")]
    InvalidCursor(String),
}

#[derive(Clone, Debug)]
pub struct TranscriptStore {
    root: Arc<PathBuf>,
}

impl TranscriptStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
        }
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn path_for(&self, session_id: &str) -> Result<PathBuf, TranscriptError> {
        if session_id.is_empty()
            || session_id == "."
            || session_id == ".."
            || session_id.contains('/')
            || session_id.contains('\\')
        {
            return Err(TranscriptError::InvalidSessionId(session_id.to_string()));
        }
        Ok(self.root.join(format!("{session_id}.jsonl")))
    }

    pub async fn append(
        &self,
        session_id: impl Into<String>,
        messages: Vec<Value>,
    ) -> Result<(), TranscriptError> {
        let path = self.path_for(&session_id.into())?;
        let root = Arc::clone(&self.root);
        run_blocking(move || {
            std::fs::create_dir_all(root.as_path())?;
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            for message in messages {
                serde_json::to_writer(&mut file, &message)?;
                file.write_all(b"\n")?;
            }
            file.sync_data()?;
            Ok(())
        })
        .await
    }

    pub async fn read_all(
        &self,
        session_id: impl Into<String>,
    ) -> Result<Vec<Value>, TranscriptError> {
        let path = self.path_for(&session_id.into())?;
        run_blocking(move || read_lines(&path)).await
    }

    pub async fn paginate(
        &self,
        session_id: impl Into<String>,
        before: Option<&str>,
        requested_limit: Option<u32>,
    ) -> Result<SessionPageResult, TranscriptError> {
        let session_id = session_id.into();
        let path = self.path_for(&session_id)?;
        let end = before.map(parse_cursor).transpose()?;
        let maximum = if end.is_none() {
            INITIAL_HISTORY_LIMIT
        } else {
            MAX_HISTORY_LIMIT
        };
        let limit = requested_limit
            .unwrap_or(if end.is_none() {
                INITIAL_HISTORY_LIMIT
            } else {
                OLDER_HISTORY_LIMIT
            })
            .min(maximum);
        let messages = run_blocking(move || read_lines(&path)).await?;
        let end = end.unwrap_or(messages.len()).min(messages.len());
        let start = end.saturating_sub(limit as usize);
        Ok(SessionPageResult {
            session_id,
            messages: messages[start..end].to_vec(),
            next_cursor: (start > 0).then(|| start.to_string()),
            has_more: start > 0,
        })
    }

    pub async fn patch_against(
        &self,
        session_id: impl Into<String>,
        known_message_count: Option<u64>,
        last_matched_message_id: Option<&str>,
        canonical: Vec<Value>,
    ) -> Result<SessionTranscriptPatch, TranscriptError> {
        let existing = self.read_all(session_id).await?;
        let mut start = known_message_count.unwrap_or(0).min(existing.len() as u64) as usize;
        if let Some(id) = last_matched_message_id {
            if let Some(index) = existing
                .iter()
                .rposition(|message| message_id(message) == Some(id))
            {
                start = index + 1;
            }
        }
        while start > 0 && existing.get(start - 1) == canonical.get(start - 1) {
            break;
        }
        Ok(SessionTranscriptPatch {
            start_index: start as u64,
            replace_count: existing.len().saturating_sub(start) as u64,
            messages: canonical.into_iter().skip(start).collect(),
        })
    }

    /// Apply a reconciliation patch without rewriting the already matched
    /// prefix. The suffix is replaced atomically via a sibling temporary
    /// file, then renamed over the session JSONL file.
    pub async fn apply_patch(
        &self,
        session_id: impl Into<String>,
        patch: &SessionTranscriptPatch,
    ) -> Result<(), TranscriptError> {
        let session_id = session_id.into();
        let path = self.path_for(&session_id)?;
        let root = Arc::clone(&self.root);
        let patch = patch.clone();
        run_blocking(move || {
            std::fs::create_dir_all(root.as_path())?;
            let existing = read_lines(&path)?;
            let start = patch.start_index.min(existing.len() as u64) as usize;
            let replace_end = start
                .saturating_add(patch.replace_count as usize)
                .min(existing.len());
            let mut merged = Vec::with_capacity(
                start + patch.messages.len() + existing.len().saturating_sub(replace_end),
            );
            merged.extend_from_slice(&existing[..start]);
            merged.extend(patch.messages);
            merged.extend_from_slice(&existing[replace_end..]);

            let temp = path.with_extension("jsonl.tmp");
            {
                use std::io::Write;
                let mut file = std::fs::File::create(&temp)?;
                for message in merged {
                    serde_json::to_writer(&mut file, &message)?;
                    file.write_all(b"\n")?;
                }
                file.sync_data()?;
            }
            std::fs::rename(temp, path)?;
            Ok(())
        })
        .await
    }
}

fn parse_cursor(cursor: &str) -> Result<usize, TranscriptError> {
    cursor
        .parse()
        .map_err(|_| TranscriptError::InvalidCursor(cursor.to_string()))
}

fn message_id(message: &Value) -> Option<&str> {
    message
        .get("messageId")
        .and_then(Value::as_str)
        .or_else(|| message.get("id").and_then(Value::as_str))
}

fn read_lines(path: &Path) -> Result<Vec<Value>, TranscriptError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(TranscriptError::from))
        .collect()
}

async fn run_blocking<T, F>(operation: F) -> Result<T, TranscriptError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, TranscriptError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(join_error)?
}

fn join_error(error: JoinError) -> TranscriptError {
    TranscriptError::Task(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn initial_page_is_fifty_and_older_pages_are_forty() {
        let store = TranscriptStore::new(tempdir().unwrap().path());
        let messages = (0..95).map(|i| serde_json::json!({"id": i})).collect();
        store.append("s1", messages).await.unwrap();
        let initial = store.paginate("s1", None, None).await.unwrap();
        assert_eq!(initial.messages.len(), 50);
        assert_eq!(initial.next_cursor.as_deref(), Some("45"));
        let older = store
            .paginate("s1", initial.next_cursor.as_deref(), None)
            .await
            .unwrap();
        assert_eq!(older.messages.len(), 40);
        assert_eq!(older.messages[0]["id"], 5);
    }

    #[tokio::test]
    async fn append_and_patch_preserve_matched_prefix() {
        let store = TranscriptStore::new(tempdir().unwrap().path());
        store
            .append("s1", vec![serde_json::json!({"id": "a"})])
            .await
            .unwrap();
        let patch = store
            .patch_against(
                "s1",
                Some(1),
                Some("a"),
                vec![
                    serde_json::json!({"id": "a"}),
                    serde_json::json!({"id": "b"}),
                ],
            )
            .await
            .unwrap();
        assert_eq!(patch.start_index, 1);
        assert_eq!(patch.replace_count, 0);
        assert_eq!(patch.messages, vec![serde_json::json!({"id": "b"})]);
    }
}
