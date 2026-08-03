//! Durable per-session FIFO prompt queue.

use acpx_proto::session_stream::{
    QueueItem, QueueMutation, QueueMutationParams, QueueMutationResult, QueueOperation,
    QueueSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use super::transcripts::{TranscriptError, TranscriptStore};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum QueueRecordOperation {
    Enqueue,
    SendNow,
    Cancel,
    Pause,
    Resume,
    /// Durable dispatch reservation. An unresolved claim is recovered back
    /// into the FIFO after a daemon restart.
    Claim,
    Complete,
    Release,
}

impl From<QueueOperation> for QueueRecordOperation {
    fn from(operation: QueueOperation) -> Self {
        match operation {
            QueueOperation::Enqueue => Self::Enqueue,
            QueueOperation::SendNow => Self::SendNow,
            QueueOperation::Cancel => Self::Cancel,
            QueueOperation::Pause => Self::Pause,
            QueueOperation::Resume => Self::Resume,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueRecord {
    operation: QueueRecordOperation,
    #[serde(default)]
    item: Option<QueueItem>,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    queue_entry_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueueStateEvent {
    pub session_id: String,
    pub queue_entry_id: String,
    pub mutation: QueueMutation,
    pub idempotency_key: Option<String>,
    pub position: Option<u32>,
    /// Prompt text for an inserted/sent lifecycle delta.  Queue deltas are
    /// consumed by clients without necessarily having the mutation response
    /// (observers and reconnecting clients), so an inserted row must carry
    /// its text rather than forcing the client to manufacture an empty row.
    pub text: Option<String>,
    /// True only for the durable completion of a claimed prompt. Cancel and
    /// release mutations are not turn completion notifications.
    pub completed: bool,
}

#[derive(Clone, Debug)]
pub struct QueueStore {
    root: Arc<PathBuf>,
    actors: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl QueueStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            actors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn actor(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut actors = self.actors.lock().await;
        actors
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn path(&self, session_id: &str) -> Result<PathBuf, TranscriptError> {
        // Reuse the transcript path-safety rule without sharing transcript
        // files or making either repository authoritative for the other.
        let validator = TranscriptStore::new(self.root.as_ref());
        let transcript_path = validator.root().join(format!("{session_id}.jsonl"));
        if session_id.is_empty()
            || session_id == "."
            || session_id == ".."
            || session_id.contains('/')
            || session_id.contains('\\')
        {
            return Err(TranscriptError::InvalidSessionId(session_id.to_string()));
        }
        Ok(transcript_path.with_file_name(format!("{session_id}.queue.jsonl")))
    }

    pub async fn mutate(
        &self,
        session_id: impl Into<String>,
        params: QueueMutationParams,
    ) -> Result<(QueueMutationResult, QueueStateEvent), TranscriptError> {
        let session_id = session_id.into();
        let actor = self.actor(&session_id).await;
        let _guard = actor.lock().await;
        let path = self.path(&session_id)?;
        let root = Arc::clone(&self.root);
        let task_params = params.clone();
        let result = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(root.as_path())?;
            let records = read_records(&path)?;
            let replay = replay_records(records);
            let mut queue = replay.queue;
            let claimed = replay.claimed;
            let mut paused = replay.paused;

            let duplicate = if matches!(
                task_params.operation,
                QueueOperation::Enqueue | QueueOperation::SendNow
            ) {
                queue.iter().chain(claimed.iter()).find(|item| {
                    item.idempotency_key == task_params.idempotency_key
                        || task_params.queue_entry_id.as_deref()
                            == Some(item.queue_entry_id.as_str())
                })
            } else {
                None
            };
            let queue_entry_id = duplicate
                .map(|item| item.queue_entry_id.clone())
                .or_else(|| task_params.queue_entry_id.clone())
                .or_else(|| Some(format!("queue-{}", task_params.idempotency_key)));
            if let Some(duplicate) = duplicate {
                if task_params.operation == QueueOperation::SendNow {
                    let entry_id = duplicate.queue_entry_id.clone();
                    if let Some(index) = queue
                        .iter()
                        .position(|item| item.queue_entry_id == entry_id)
                    {
                        let item = queue.remove(index);
                        queue.insert(0, item);
                    }
                }
            } else {
                match task_params.operation {
                    QueueOperation::Enqueue | QueueOperation::SendNow => {
                        let item = QueueItem {
                            queue_entry_id: queue_entry_id.clone().unwrap_or_default(),
                            idempotency_key: task_params.idempotency_key.clone(),
                            text: task_params.text.clone().unwrap_or_default(),
                            state: "queued".into(),
                            position: 0,
                        };
                        if matches!(task_params.operation, QueueOperation::SendNow) {
                            queue.insert(0, item);
                        } else {
                            queue.push(item);
                        }
                    }
                    QueueOperation::Cancel => {
                        if let Some(entry_id) = task_params.queue_entry_id.as_deref() {
                            queue.retain(|item| item.queue_entry_id != entry_id);
                        }
                    }
                    QueueOperation::Pause => paused = true,
                    QueueOperation::Resume => paused = false,
                }
            }
            for (position, item) in queue.iter_mut().enumerate() {
                item.position = position as u32;
            }
            let record = QueueRecord {
                operation: task_params.operation.into(),
                item: queue_entry_id.as_ref().and_then(|id| {
                    queue
                        .iter()
                        .find(|item| item.queue_entry_id == *id)
                        .cloned()
                }),
                idempotency_key: task_params.idempotency_key.clone(),
                queue_entry_id: task_params.queue_entry_id.clone(),
            };
            append_record(&path, &record)?;
            Ok::<_, TranscriptError>((queue, paused, queue_entry_id))
        })
        .await
        .map_err(|error| TranscriptError::Task(error.to_string()))??;

        let (queue, paused, queue_entry_id) = result;
        let response = QueueMutationResult {
            session_id: session_id.clone(),
            idempotency_key: params.idempotency_key.clone(),
            accepted: true,
            queue_entry_id: queue_entry_id.clone(),
            queue: queue.clone(),
            paused,
        };
        let mutation = match params.operation {
            QueueOperation::Cancel => QueueMutation::Removed,
            _ => QueueMutation::Inserted,
        };
        let position = queue_entry_id.as_ref().and_then(|id| {
            queue
                .iter()
                .find(|item| &item.queue_entry_id == id)
                .map(|item| item.position)
        });
        let event = QueueStateEvent {
            session_id,
            queue_entry_id: queue_entry_id.clone().unwrap_or_default(),
            mutation,
            idempotency_key: Some(params.idempotency_key),
            position,
            text: queue_entry_id
                .as_ref()
                .and_then(|id| {
                    queue
                        .iter()
                        .find(|item| &item.queue_entry_id == id)
                        .map(|item| item.text.clone())
                })
                .or_else(|| params.text.clone()),
            completed: false,
        };
        Ok((response, event))
    }

    /// Read the authoritative queue projection for reconnect/preload without
    /// creating a JSONL record or a broadcast echo.
    pub async fn snapshot(
        &self,
        session_id: impl Into<String>,
    ) -> Result<QueueSnapshot, TranscriptError> {
        let session_id = session_id.into();
        let actor = self.actor(&session_id).await;
        let _guard = actor.lock().await;
        let path = self.path(&session_id)?;
        let result = tokio::task::spawn_blocking(move || {
            let records = read_records(&path)?;
            Ok::<_, TranscriptError>(replay_records(records))
        })
        .await
        .map_err(|error| TranscriptError::Task(error.to_string()))??;
        Ok(QueueSnapshot {
            session_id,
            queue: result.queue,
            paused: result.paused,
        })
    }

    /// Atomically claim the FIFO head for the server dispatcher. The entry
    /// is removed from the queued projection before the backend prompt is
    /// sent, so concurrent clients cannot promote it twice.
    pub async fn take_next(
        &self,
        session_id: impl Into<String>,
    ) -> Result<Option<(QueueItem, QueueStateEvent)>, TranscriptError> {
        let session_id = session_id.into();
        let actor = self.actor(&session_id).await;
        let _guard = actor.lock().await;
        let path = self.path(&session_id)?;
        let root = Arc::clone(&self.root);
        let result = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(root.as_path())?;
            let records = read_records(&path)?;
            let mut replay = replay_records(records);
            if replay.paused || replay.queue.is_empty() {
                return Ok::<_, TranscriptError>(None);
            }
            let item = replay.queue.remove(0);
            for (position, item) in replay.queue.iter_mut().enumerate() {
                item.position = position as u32;
            }
            append_record(
                &path,
                &QueueRecord {
                    operation: QueueRecordOperation::Claim,
                    item: Some(item.clone()),
                    idempotency_key: format!("dispatch:{}", item.idempotency_key),
                    queue_entry_id: Some(item.queue_entry_id.clone()),
                },
            )?;
            Ok(Some((item, replay.queue, replay.paused)))
        })
        .await
        .map_err(|error| TranscriptError::Task(error.to_string()))??;
        let Some((item, _queue, _paused)) = result else {
            return Ok(None);
        };
        Ok(Some((
            item.clone(),
            QueueStateEvent {
                session_id,
                queue_entry_id: item.queue_entry_id.clone(),
                mutation: QueueMutation::SentPrompt,
                idempotency_key: Some(item.idempotency_key.clone()),
                position: None,
                text: Some(item.text.clone()),
                completed: false,
            },
        )))
    }

    /// Mark a successfully dispatched entry complete. The claim remains in
    /// the log until this record is durable, so a failed prompt dispatch can
    /// never silently consume the user's text.
    pub async fn complete(
        &self,
        session_id: impl Into<String>,
        item: &QueueItem,
    ) -> Result<QueueStateEvent, TranscriptError> {
        self.finish_claim(session_id.into(), item, QueueRecordOperation::Complete)
            .await
    }

    /// Return a failed dispatch to the front of the FIFO for a later retry.
    pub async fn release(
        &self,
        session_id: impl Into<String>,
        item: &QueueItem,
    ) -> Result<QueueStateEvent, TranscriptError> {
        self.finish_claim(session_id.into(), item, QueueRecordOperation::Release)
            .await
    }

    async fn finish_claim(
        &self,
        session_id: String,
        item: &QueueItem,
        operation: QueueRecordOperation,
    ) -> Result<QueueStateEvent, TranscriptError> {
        let actor = self.actor(&session_id).await;
        let _guard = actor.lock().await;
        let path = self.path(&session_id)?;
        let root = Arc::clone(&self.root);
        let entry_id = item.queue_entry_id.clone();
        let item = item.clone();
        let event_operation = operation.clone();
        let event_item = item.clone();
        let _result = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(root.as_path())?;
            append_record(
                &path,
                &QueueRecord {
                    operation,
                    item: Some(item),
                    idempotency_key: format!("dispatch:{entry_id}"),
                    queue_entry_id: Some(entry_id),
                },
            )?;
            Ok::<_, TranscriptError>(replay_records(read_records(&path)?))
        })
        .await
        .map_err(|error| TranscriptError::Task(error.to_string()))??;
        Ok(QueueStateEvent {
            session_id,
            queue_entry_id: event_item.queue_entry_id.clone(),
            mutation: if event_operation == QueueRecordOperation::Complete {
                QueueMutation::Removed
            } else {
                QueueMutation::Inserted
            },
            idempotency_key: Some(event_item.idempotency_key.clone()),
            position: if event_operation == QueueRecordOperation::Release {
                Some(0)
            } else {
                None
            },
            text: Some(event_item.text.clone()),
            completed: event_operation == QueueRecordOperation::Complete,
        })
    }

    /// Convert unresolved dispatch claims left by a crashed daemon back into
    /// queued entries before the recovered session's dispatcher starts.
    pub async fn recover_inflight(
        &self,
        session_id: impl Into<String>,
    ) -> Result<(), TranscriptError> {
        let session_id = session_id.into();
        let actor = self.actor(&session_id).await;
        let _guard = actor.lock().await;
        let path = self.path(&session_id)?;
        let root = Arc::clone(&self.root);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(root.as_path())?;
            let state = replay_records(read_records(&path)?);
            for item in state.claimed {
                append_record(
                    &path,
                    &QueueRecord {
                        operation: QueueRecordOperation::Release,
                        item: Some(item.clone()),
                        idempotency_key: format!("recovery:{}", item.idempotency_key),
                        queue_entry_id: Some(item.queue_entry_id),
                    },
                )?;
            }
            Ok::<_, TranscriptError>(())
        })
        .await
        .map_err(|error| TranscriptError::Task(error.to_string()))??;
        Ok(())
    }
}

#[derive(Clone)]
pub struct QueueHub {
    streams: Arc<Mutex<HashMap<String, broadcast::Sender<Value>>>>,
    steer_streams: Arc<Mutex<HashMap<String, broadcast::Sender<Value>>>>,
}

impl Default for QueueHub {
    fn default() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            steer_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl QueueHub {
    pub async fn subscribe(&self, session_id: &str) -> broadcast::Receiver<Value> {
        let mut streams = self.streams.lock().await;
        streams
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(128).0)
            .subscribe()
    }

    pub async fn publish(&self, event: QueueStateEvent) {
        self.publish_with_origin(event, None).await;
    }

    /// Publish a queue lifecycle delta, optionally recording the originating
    /// websocket client.  Transports can use this marker to suppress the
    /// duplicate lifecycle frame for the request's origin while preserving
    /// fan-out to every other subscriber.
    pub async fn publish_with_origin(
        &self,
        event: QueueStateEvent,
        origin_client_id: Option<&str>,
    ) {
        let streams = self.streams.lock().await;
        if let Some(sender) = streams.get(&event.session_id) {
            let mut payload = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "acpx/session/queue",
                "params": {
                    "sessionId": event.session_id,
                    "queueEntryId": event.queue_entry_id,
                    "mutation": event.mutation,
                    "idempotencyKey": event.idempotency_key,
                    "position": event.position,
                    "text": event.text,
                    "completed": event.completed
                }
            });
            if let Some(origin) = origin_client_id {
                payload["params"]["originClientId"] = serde_json::Value::String(origin.to_owned());
            }
            let _ = sender.send(payload);
        }
    }

    pub async fn subscribe_steer(&self, session_id: &str) -> broadcast::Receiver<Value> {
        let mut streams = self.steer_streams.lock().await;
        streams
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(128).0)
            .subscribe()
    }

    pub async fn publish_steer(
        &self,
        event: acpx_proto::session_stream::SessionSteerEvent,
        origin_client_id: Option<&str>,
    ) {
        let streams = self.steer_streams.lock().await;
        if let Some(sender) = streams.get(&event.session_id) {
            let mut payload = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "acpx/session/steer",
                "params": event,
            });
            if let Some(origin) = origin_client_id {
                payload["_acpxOriginClientId"] = serde_json::Value::String(origin.to_owned());
            }
            let _ = sender.send(payload);
        }
    }
}

fn read_records(path: &std::path::Path) -> Result<Vec<QueueRecord>, TranscriptError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let unterminated_tail = !content.ends_with('\n');
    let lines: Vec<&str> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(error) if unterminated_tail && index + 1 == lines.len() => {
                // A process can be killed between append/write and the final
                // fsync, leaving only a truncated tail record. Earlier lines
                // are part of the durable history and must remain strict;
                // only the incomplete tail is safely discardable. Remove it
                // now so the next append does not turn the recoverable tail
                // into an interior corrupt record.
                tracing::warn!(path = %path.display(), %error, "ignoring truncated queue-log tail");
                let valid_len = content.rfind('\n').map_or(0, |offset| offset + 1);
                let file = std::fs::OpenOptions::new().write(true).open(path)?;
                file.set_len(valid_len as u64)?;
                file.sync_data()?;
            }
            Err(error) => return Err(TranscriptError::from(error)),
        }
    }
    Ok(records)
}

#[derive(Debug, Default)]
struct QueueReplayState {
    queue: Vec<QueueItem>,
    paused: bool,
    claimed: Vec<QueueItem>,
}

fn replay_records(records: Vec<QueueRecord>) -> QueueReplayState {
    let mut queue = Vec::new();
    let mut paused = false;
    let mut claimed: Vec<QueueItem> = Vec::new();
    for record in records {
        match record.operation {
            QueueRecordOperation::Enqueue => {
                if let Some(item) = record.item {
                    if !queue
                        .iter()
                        .any(|current: &QueueItem| current.idempotency_key == item.idempotency_key)
                    {
                        queue.push(item);
                    }
                }
            }
            QueueRecordOperation::SendNow => {
                if let Some(item) = record.item {
                    queue.retain(|current| {
                        current.idempotency_key != item.idempotency_key
                            && current.queue_entry_id != item.queue_entry_id
                    });
                    queue.insert(0, item);
                }
            }
            QueueRecordOperation::Cancel => {
                if let Some(entry_id) = record.queue_entry_id {
                    queue.retain(|item| item.queue_entry_id != entry_id);
                    claimed.retain(|item| item.queue_entry_id != entry_id);
                }
            }
            QueueRecordOperation::Pause => paused = true,
            QueueRecordOperation::Resume => paused = false,
            QueueRecordOperation::Claim => {
                if let Some(item) = record.item {
                    queue.retain(|current| current.queue_entry_id != item.queue_entry_id);
                    claimed.retain(|current| current.queue_entry_id != item.queue_entry_id);
                    claimed.push(item);
                }
            }
            QueueRecordOperation::Complete => {
                if let Some(entry_id) = record.queue_entry_id {
                    claimed.retain(|item| item.queue_entry_id != entry_id);
                }
            }
            QueueRecordOperation::Release => {
                if let Some(entry_id) = record.queue_entry_id {
                    if let Some(index) = claimed
                        .iter()
                        .position(|item| item.queue_entry_id == entry_id)
                    {
                        let item = claimed.remove(index);
                        queue.insert(0, item);
                    } else if let Some(item) = record.item {
                        queue.insert(0, item);
                    }
                }
            }
        }
    }
    for (position, item) in queue.iter_mut().enumerate() {
        item.position = position as u32;
    }
    QueueReplayState {
        queue,
        paused,
        claimed,
    }
}

fn append_record(path: &std::path::Path, record: &QueueRecord) -> Result<(), TranscriptError> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    drop(file);
    compact_if_needed(path)?;
    Ok(())
}

const COMPACTION_RECORD_LIMIT: usize = 128;

fn compact_if_needed(path: &std::path::Path) -> Result<(), TranscriptError> {
    let records = read_records(path)?;
    if records.len() <= COMPACTION_RECORD_LIMIT {
        return Ok(());
    }
    let record_count = records.len();
    let state = replay_records(records);
    let live_records = state.queue.len() + state.claimed.len();
    // Do not rewrite on every append when the queue itself legitimately has
    // more than the threshold of live entries. Compact only when tombstones
    // dominate the log; otherwise the compacted file would still exceed the
    // threshold and immediately trigger another full rewrite.
    if record_count <= live_records.saturating_add(32) {
        return Ok(());
    }
    let temporary = path.with_extension("queue.jsonl.compact");
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        for item in state.queue {
            write_record(
                &mut file,
                &QueueRecord {
                    operation: QueueRecordOperation::Enqueue,
                    item: Some(item.clone()),
                    idempotency_key: item.idempotency_key,
                    queue_entry_id: Some(item.queue_entry_id),
                },
            )?;
        }
        for item in state.claimed {
            write_record(
                &mut file,
                &QueueRecord {
                    operation: QueueRecordOperation::Claim,
                    item: Some(item.clone()),
                    idempotency_key: format!("dispatch:{}", item.idempotency_key),
                    queue_entry_id: Some(item.queue_entry_id),
                },
            )?;
        }
        if state.paused {
            write_record(
                &mut file,
                &QueueRecord {
                    operation: QueueRecordOperation::Pause,
                    item: None,
                    idempotency_key: "compact:pause".into(),
                    queue_entry_id: None,
                },
            )?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<_, TranscriptError>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn write_record(file: &mut std::fs::File, record: &QueueRecord) -> Result<(), TranscriptError> {
    use std::io::Write;
    serde_json::to_writer(&mut *file, record)?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[tokio::test]
    async fn queue_is_fifo_and_idempotent() {
        let store = QueueStore::new(tempdir().unwrap().path());
        let first = QueueMutationParams {
            session_id: "s1".into(),
            idempotency_key: "a".into(),
            operation: QueueOperation::Enqueue,
            queue_entry_id: None,
            text: Some("first".into()),
        };
        let second = QueueMutationParams {
            idempotency_key: "b".into(),
            text: Some("second".into()),
            ..first.clone()
        };
        let (one, _) = store.mutate("s1", first.clone()).await.unwrap();
        let (two, _) = store.mutate("s1", second).await.unwrap();
        let (duplicate, _) = store.mutate("s1", first).await.unwrap();
        assert_eq!(one.queue.len(), 1);
        assert_eq!(
            two.queue
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(duplicate.queue.len(), 2);
    }

    #[tokio::test]
    async fn queue_replays_after_store_restart() {
        let directory = tempdir().unwrap();
        let first = QueueStore::new(directory.path());
        first
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "a".into(),
                    operation: QueueOperation::Enqueue,
                    queue_entry_id: None,
                    text: Some("survives restart".into()),
                },
            )
            .await
            .unwrap();

        let restarted = QueueStore::new(directory.path());
        let (snapshot, _) = restarted
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "b".into(),
                    operation: QueueOperation::Pause,
                    queue_entry_id: None,
                    text: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(snapshot.queue[0].text, "survives restart");
        assert!(snapshot.paused);
    }

    #[tokio::test]
    async fn send_now_reorders_and_cancel_absorbs_a_queued_entry() {
        let store = QueueStore::new(tempdir().unwrap().path());
        for (key, text) in [("a", "first"), ("b", "second"), ("c", "urgent")] {
            store
                .mutate(
                    "s1",
                    QueueMutationParams {
                        session_id: "s1".into(),
                        idempotency_key: key.into(),
                        operation: if key == "c" {
                            QueueOperation::SendNow
                        } else {
                            QueueOperation::Enqueue
                        },
                        queue_entry_id: None,
                        text: Some(text.into()),
                    },
                )
                .await
                .unwrap();
        }
        let (snapshot, _) = store
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "cancel-b".into(),
                    operation: QueueOperation::Cancel,
                    queue_entry_id: Some("queue-b".into()),
                    text: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(snapshot.queue[0].text, "urgent");
        assert_eq!(snapshot.queue.len(), 2);
        assert!(!snapshot.queue.iter().any(|item| item.text == "second"));
    }

    #[tokio::test]
    async fn send_now_moves_an_existing_entry_to_the_front() {
        let store = QueueStore::new(tempdir().unwrap().path());
        for (key, text) in [("a", "first"), ("b", "urgent")] {
            store
                .mutate(
                    "s1",
                    QueueMutationParams {
                        session_id: "s1".into(),
                        idempotency_key: key.into(),
                        operation: QueueOperation::Enqueue,
                        queue_entry_id: None,
                        text: Some(text.into()),
                    },
                )
                .await
                .unwrap();
        }
        let urgent = store
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "send-now-b".into(),
                    operation: QueueOperation::SendNow,
                    queue_entry_id: Some("queue-b".into()),
                    text: None,
                },
            )
            .await
            .unwrap()
            .0;
        assert_eq!(urgent.queue[0].text, "urgent");
        assert_eq!(urgent.queue[1].text, "first");
    }

    #[tokio::test]
    async fn failed_dispatch_releases_claim_back_to_fifo() {
        let directory = tempdir().unwrap();
        let store = QueueStore::new(directory.path());
        store
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "a".into(),
                    operation: QueueOperation::Enqueue,
                    queue_entry_id: None,
                    text: Some("must retry".into()),
                },
            )
            .await
            .unwrap();
        let (item, _) = store.take_next("s1").await.unwrap().unwrap();
        assert!(store.snapshot("s1").await.unwrap().queue.is_empty());
        store.release("s1", &item).await.unwrap();
        let snapshot = store.snapshot("s1").await.unwrap();
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.queue[0].text, "must retry");
    }

    #[tokio::test]
    async fn unresolved_claim_is_recovered_after_restart() {
        let directory = tempdir().unwrap();
        let store = QueueStore::new(directory.path());
        store
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "a".into(),
                    operation: QueueOperation::Enqueue,
                    queue_entry_id: None,
                    text: Some("survives dispatcher crash".into()),
                },
            )
            .await
            .unwrap();
        let _ = store.take_next("s1").await.unwrap().unwrap();

        let restarted = QueueStore::new(directory.path());
        restarted.recover_inflight("s1").await.unwrap();
        let snapshot = restarted.snapshot("s1").await.unwrap();
        assert_eq!(snapshot.queue[0].text, "survives dispatcher crash");
    }

    #[tokio::test]
    async fn queue_log_compacts_without_changing_projection() {
        let directory = tempdir().unwrap();
        let store = QueueStore::new(directory.path());
        for index in 0..140 {
            store
                .mutate(
                    "s1",
                    QueueMutationParams {
                        session_id: "s1".into(),
                        idempotency_key: format!("key-{index}"),
                        operation: QueueOperation::Enqueue,
                        queue_entry_id: None,
                        text: Some(format!("message-{index}")),
                    },
                )
                .await
                .unwrap();
        }
        for index in 0..35 {
            store
                .mutate(
                    "s1",
                    QueueMutationParams {
                        session_id: "s1".into(),
                        idempotency_key: format!("cancel-{index}"),
                        operation: QueueOperation::Cancel,
                        queue_entry_id: Some(format!("queue-key-{index}")),
                        text: None,
                    },
                )
                .await
                .unwrap();
        }
        let line_count = std::fs::read_to_string(directory.path().join("s1.queue.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert!(line_count <= COMPACTION_RECORD_LIMIT);
        assert_eq!(store.snapshot("s1").await.unwrap().queue.len(), 105);
    }

    #[tokio::test]
    async fn truncated_tail_record_is_ignored_without_losing_prior_queue() {
        let directory = tempdir().unwrap();
        let store = QueueStore::new(directory.path());
        store
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "a".into(),
                    operation: QueueOperation::Enqueue,
                    queue_entry_id: None,
                    text: Some("durable prompt".into()),
                },
            )
            .await
            .unwrap();
        let path = directory.path().join("s1.queue.jsonl");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"operation\":\"Enqueue\"")
            .unwrap();

        let snapshot = store.snapshot("s1").await.unwrap();
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.queue[0].text, "durable prompt");

        store
            .mutate(
                "s1",
                QueueMutationParams {
                    session_id: "s1".into(),
                    idempotency_key: "b".into(),
                    operation: QueueOperation::Enqueue,
                    queue_entry_id: None,
                    text: Some("after recovery".into()),
                },
            )
            .await
            .unwrap();
        let snapshot = store.snapshot("s1").await.unwrap();
        assert_eq!(
            snapshot
                .queue
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["durable prompt", "after recovery"]
        );
    }

    #[tokio::test]
    async fn newline_terminated_corrupt_record_is_not_silently_dropped() {
        let directory = tempdir().unwrap();
        let store = QueueStore::new(directory.path());
        let path = directory.path().join("s1.queue.jsonl");
        std::fs::write(&path, b"{not-json}\n").unwrap();

        assert!(store.snapshot("s1").await.is_err());
    }

    #[tokio::test]
    async fn queue_hub_fans_out_each_state_change_to_all_clients() {
        let hub = QueueHub::default();
        let mut first = hub.subscribe("s1").await;
        let mut second = hub.subscribe("s1").await;
        hub.publish(QueueStateEvent {
            session_id: "s1".into(),
            queue_entry_id: "queue-a".into(),
            mutation: QueueMutation::Inserted,
            idempotency_key: Some("client-a".into()),
            position: Some(0),
            text: Some("queued".into()),
            completed: false,
        })
        .await;
        for receiver in [&mut first, &mut second] {
            let event = receiver.recv().await.unwrap();
            assert_eq!(event["method"], "acpx/session/queue");
            assert_eq!(event["params"]["idempotencyKey"], "client-a");
            assert_eq!(event["params"]["queueEntryId"], "queue-a");
        }
    }
}
