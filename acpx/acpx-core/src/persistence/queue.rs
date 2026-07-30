//! Durable per-session FIFO prompt queue.

use acpx_proto::session_stream::{
    QueueItem, QueueMutationParams, QueueMutationResult, QueueOperation, QueueSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use super::transcripts::{TranscriptError, TranscriptStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueRecord {
    operation: QueueOperation,
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
    pub queue: Vec<QueueItem>,
    pub paused: bool,
    pub idempotency_key: String,
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
            let mut queue = Vec::new();
            let mut paused = false;
            for record in records {
                match record.operation {
                    QueueOperation::Enqueue | QueueOperation::SendNow => {
                        if let Some(item) = record.item {
                            if !queue.iter().any(|current: &QueueItem| {
                                current.idempotency_key == item.idempotency_key
                            }) {
                                if matches!(record.operation, QueueOperation::SendNow) {
                                    queue.insert(0, item);
                                } else {
                                    queue.push(item);
                                }
                            }
                        }
                    }
                    QueueOperation::Cancel => {
                        if let Some(entry_id) = record.queue_entry_id {
                            queue.retain(|item| item.queue_entry_id != entry_id);
                        }
                    }
                    QueueOperation::Pause => paused = true,
                    QueueOperation::Resume => paused = false,
                }
            }

            let duplicate = if matches!(
                task_params.operation,
                QueueOperation::Enqueue | QueueOperation::SendNow
            ) {
                queue.iter().find(|item| {
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
            if duplicate.is_none() {
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
                operation: task_params.operation,
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
            queue_entry_id,
            queue: queue.clone(),
            paused,
        };
        let event = QueueStateEvent {
            session_id,
            queue,
            paused,
            idempotency_key: params.idempotency_key,
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
            queue: result.0,
            paused: result.1,
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
            let (mut queue, paused) = replay_records(records);
            if paused || queue.is_empty() {
                return Ok::<_, TranscriptError>(None);
            }
            let item = queue.remove(0);
            for (position, item) in queue.iter_mut().enumerate() {
                item.position = position as u32;
            }
            append_record(
                &path,
                &QueueRecord {
                    operation: QueueOperation::Cancel,
                    item: None,
                    idempotency_key: format!("dispatch:{}", item.idempotency_key),
                    queue_entry_id: Some(item.queue_entry_id.clone()),
                },
            )?;
            Ok(Some((item, queue, paused)))
        })
        .await
        .map_err(|error| TranscriptError::Task(error.to_string()))??;
        let Some((item, queue, paused)) = result else {
            return Ok(None);
        };
        Ok(Some((
            item,
            QueueStateEvent {
                session_id,
                queue,
                paused,
                idempotency_key: String::new(),
            },
        )))
    }
}

#[derive(Clone)]
pub struct QueueHub {
    streams: Arc<Mutex<HashMap<String, broadcast::Sender<Value>>>>,
}

impl Default for QueueHub {
    fn default() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
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
        let streams = self.streams.lock().await;
        if let Some(sender) = streams.get(&event.session_id) {
            let _ = sender.send(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "acpx/session/queue",
                "params": {
                    "sessionId": event.session_id,
                    "queue": event.queue,
                    "paused": event.paused,
                    "idempotencyKey": event.idempotency_key
                }
            }));
        }
    }
}

fn read_records(path: &std::path::Path) -> Result<Vec<QueueRecord>, TranscriptError> {
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

fn replay_records(records: Vec<QueueRecord>) -> (Vec<QueueItem>, bool) {
    let mut queue = Vec::new();
    let mut paused = false;
    for record in records {
        match record.operation {
            QueueOperation::Enqueue | QueueOperation::SendNow => {
                if let Some(item) = record.item {
                    if !queue
                        .iter()
                        .any(|current: &QueueItem| current.idempotency_key == item.idempotency_key)
                    {
                        if matches!(record.operation, QueueOperation::SendNow) {
                            queue.insert(0, item);
                        } else {
                            queue.push(item);
                        }
                    }
                }
            }
            QueueOperation::Cancel => {
                if let Some(entry_id) = record.queue_entry_id {
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
    (queue, paused)
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn queue_hub_fans_out_each_state_change_to_all_clients() {
        let hub = QueueHub::default();
        let mut first = hub.subscribe("s1").await;
        let mut second = hub.subscribe("s1").await;
        hub.publish(QueueStateEvent {
            session_id: "s1".into(),
            queue: Vec::new(),
            paused: false,
            idempotency_key: "client-a".into(),
        })
        .await;
        for receiver in [&mut first, &mut second] {
            let event = receiver.recv().await.unwrap();
            assert_eq!(event["method"], "acpx/session/queue");
            assert_eq!(event["params"]["idempotencyKey"], "client-a");
        }
    }
}
