//! Per-process response demultiplexing: a single background task owns a
//! [`FramedReader`] for the backend process's lifetime and routes each
//! frame either to whichever caller registered interest in its `id`
//! (a pending-request table of `id -> oneshot` senders) or, if nothing
//! matches, onto an "unmatched" channel for the caller-supplied consumer
//! to interpret (bare notifications and agent-initiated requests both
//! flow there -- this crate stays protocol-agnostic about which is which,
//! same rationale as `BackendProcess::handshake_done`).
//!
//! This is new, unwired plumbing: nothing in `acpx-core::router` calls
//! [`spawn_reader_task`] yet. It exists so callers can register-then-await
//! a response instead of holding the outer per-process lock across the
//! entire write + blocking-read-loop of a turn -- see
//! `memory/acpx/tasks/zed_integration.yaml` task 7 and
//! `memory/acpx/gen/acpx-concurrency-config-execution.meta.json` phase 1.

use crate::framing::{FramedReader, FramingError};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

/// A frame the reader task could not match to a registered pending
/// request: either a bare notification, or an id-bearing frame nobody
/// (or no longer anybody) is waiting on -- including agent-initiated
/// requests (`id` + `method` both present), which the consumer is
/// responsible for answering via the process's independent
/// [`crate::framing::FramedWriter`] handle.
pub type UnmatchedFrame = Value;

/// Capacity of the unmatched-frame channel `spawn_reader_task` feeds and
/// its consumer drains.
///
/// **Why bounded, not unbounded.** A prior version used
/// `mpsc::unbounded_channel()` here: a backend emitting notifications (or
/// agent-initiated requests) faster than its consumer can process them
/// (`acpx_core::router::spawn_demux_consumer`, whose per-frame handling
/// can itself wait on a live client via `PERMISSION_RELAY_TIMEOUT`/
/// `DEFAULT_INTERACTION_TIMEOUT`) had no ceiling on how much unprocessed
/// JSON could pile up in this process's memory. Bounding it gives the
/// reader task natural backpressure instead: once full, [`spawn_reader_
/// task`]'s `send(value).await` simply waits for the consumer to drain
/// one, which only delays this one backend process's own frame delivery
/// (matched-response resolution still happens on every frame observed
/// before the queue filled, and the consumer's own operations are all
/// bounded by now -- see `write_backend_value_locked`/`*_TIMEOUT`
/// constants in `acpx-core::router` -- so this can't wait forever
/// either). Sized generously above any burst a well-behaved ACP session
/// should ever produce (a turn's `session/update` stream, or a handful of
/// concurrent permission requests) so it never engages in the common
/// case.
pub(crate) const UNMATCHED_FRAME_QUEUE_CAPACITY: usize = 1024;

/// Why a registered response never arrived.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DemuxRecvError {
    /// The reader task stopped (EOF, read error, or explicit shutdown)
    /// before a frame with the registered id arrived. Every other pending
    /// registration fails the same way at the same time -- see
    /// [`PendingRequests::fail_all`].
    #[error("backend process reader task ended before a response arrived")]
    ReaderClosed,
}

/// Table of in-flight callers keyed by request id, each holding a
/// [`oneshot::Receiver`] that the reader task resolves once a matching
/// response frame is observed.
#[derive(Debug, Default)]
pub struct PendingRequests {
    inner: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register interest in the response for `id`. Must be called before
    /// the matching request is written, so the reader task can never
    /// observe the response before the registration exists.
    pub async fn register(&self, id: &Value) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.insert(id_key(id), tx);
        rx
    }

    /// Drop a registration without waiting for it (e.g. the caller timed
    /// out or was cancelled). Safe to call even if the reader task already
    /// resolved or is about to resolve it -- the stale sender is simply
    /// discarded either way.
    pub async fn cancel(&self, id: &Value) {
        self.inner.lock().await.remove(&id_key(id));
    }

    /// Reader-task-only: resolve the waiter for `id` with `value`, if any.
    /// Returns `true` if a waiter was found and sent to (frame consumed),
    /// `false` if the frame should be treated as unmatched.
    async fn resolve(&self, id: &Value, value: Value) -> bool {
        let tx = self.inner.lock().await.remove(&id_key(id));
        match tx {
            Some(tx) => tx.send(value).is_ok(),
            None => false,
        }
    }

    /// Reader-task-only: drain and fail every pending waiter. Dropping
    /// each sender resolves its receiver with `Err(RecvError)`, which
    /// callers map to [`DemuxRecvError::ReaderClosed`].
    async fn fail_all(&self) {
        self.inner.lock().await.clear();
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Await the response registered under `id` via [`PendingRequests::register`].
pub async fn recv(rx: oneshot::Receiver<Value>) -> Result<Value, DemuxRecvError> {
    rx.await.map_err(|_| DemuxRecvError::ReaderClosed)
}

/// Spawns a background task that owns `reader` for the process's
/// lifetime: every frame with an `id` matching a live registration in
/// `pending` resolves that registration; everything else (bare
/// notifications, agent-initiated requests, and id-bearing frames with no
/// live registration) is forwarded on `unmatched_tx`. Exits on read error
/// or EOF, at which point every still-pending registration is failed via
/// [`PendingRequests::fail_all`] so no caller is left hanging.
///
/// `unmatched_tx` closing (no receiver left) is not fatal to the reader
/// loop -- frames are still drained off the stream so the child process
/// never blocks on a full stdout pipe, they're just dropped after the
/// failed send.
pub fn spawn_reader_task(
    mut reader: FramedReader,
    pending: Arc<PendingRequests>,
    unmatched_tx: mpsc::Sender<UnmatchedFrame>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match reader.read_value().await {
                Ok(value) => {
                    if let Some(id) = value.get("id").cloned() {
                        if pending.resolve(&id, value.clone()).await {
                            continue;
                        }
                    }
                    // Bounded send -- see `UNMATCHED_FRAME_QUEUE_CAPACITY`'s
                    // doc comment for why this is deliberate backpressure,
                    // not an oversight. A closed receiver (no consumer left) still
                    // fails immediately rather than blocking, same as the
                    // old unbounded `send`'s "not fatal" behavior.
                    let _ = unmatched_tx.send(value).await;
                }
                Err(FramingError::Eof) => break,
                Err(_read_err) => break,
            }
        }
        pending.fail_all().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    /// Builds a `FramedReader` over a real pipe so tests exercise the same
    /// newline-delimited framing path a real child stdout would.
    async fn reader_over_pipe() -> (FramedReader, tokio::process::ChildStdin, tokio::process::Child)
    {
        // `cat` echoes stdin to stdout unmodified -- a minimal stand-in for
        // a backend process's stdio pipe without depending on any acpx
        // agent binary being installed.
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn cat");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        (FramedReader::new(stdout), stdin, child)
    }

    async fn write_line(stdin: &mut tokio::process::ChildStdin, value: &Value) {
        let mut line = serde_json::to_vec(value).unwrap();
        line.push(b'\n');
        stdin.write_all(&line).await.unwrap();
        stdin.flush().await.unwrap();
    }

    #[tokio::test]
    async fn matched_response_resolves_the_registered_waiter() {
        let (reader, mut stdin, mut child) = reader_over_pipe().await;
        let pending = Arc::new(PendingRequests::new());
        let (unmatched_tx, mut unmatched_rx) = mpsc::channel(UNMATCHED_FRAME_QUEUE_CAPACITY);
        let _task = spawn_reader_task(reader, Arc::clone(&pending), unmatched_tx);

        let id = json!(1);
        let rx = pending.register(&id).await;
        write_line(&mut stdin, &json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}})).await;

        let resolved = recv(rx).await.expect("response");
        assert_eq!(resolved["result"]["ok"], json!(true));
        assert!(unmatched_rx.try_recv().is_err(), "matched frame must not also appear on the unmatched channel");

        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn unmatched_frames_route_to_the_unmatched_channel() {
        let (reader, mut stdin, mut child) = reader_over_pipe().await;
        let pending = Arc::new(PendingRequests::new());
        let (unmatched_tx, mut unmatched_rx) = mpsc::channel(UNMATCHED_FRAME_QUEUE_CAPACITY);
        let _task = spawn_reader_task(reader, pending, unmatched_tx);

        write_line(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"n": 1}}),
        )
        .await;

        let frame = unmatched_rx.recv().await.expect("unmatched frame");
        assert_eq!(frame["method"], json!("session/update"));

        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn id_bearing_frame_with_no_live_registration_is_unmatched() {
        let (reader, mut stdin, mut child) = reader_over_pipe().await;
        let pending = Arc::new(PendingRequests::new());
        let (unmatched_tx, mut unmatched_rx) = mpsc::channel(UNMATCHED_FRAME_QUEUE_CAPACITY);
        let _task = spawn_reader_task(reader, pending, unmatched_tx);

        // e.g. an agent-initiated request: has both id and method, but no
        // caller ever called `register` for this id.
        write_line(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "id": "agent-req-1", "method": "fs/read_text_file"}),
        )
        .await;

        let frame = unmatched_rx.recv().await.expect("unmatched frame");
        assert_eq!(frame["method"], json!("fs/read_text_file"));

        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn two_concurrent_registrations_each_resolve_independently() {
        let (reader, mut stdin, mut child) = reader_over_pipe().await;
        let pending = Arc::new(PendingRequests::new());
        let (unmatched_tx, _unmatched_rx) = mpsc::channel(UNMATCHED_FRAME_QUEUE_CAPACITY);
        let _task = spawn_reader_task(reader, Arc::clone(&pending), unmatched_tx);

        let rx_a = pending.register(&json!("a")).await;
        let rx_b = pending.register(&json!("b")).await;
        assert_eq!(pending.len().await, 2);

        // Responses arrive out of registration order -- the table must
        // route by id, not by arrival order.
        write_line(&mut stdin, &json!({"jsonrpc": "2.0", "id": "b", "result": "second"})).await;
        write_line(&mut stdin, &json!({"jsonrpc": "2.0", "id": "a", "result": "first"})).await;

        let a = recv(rx_a).await.expect("a resolves");
        let b = recv(rx_b).await.expect("b resolves");
        assert_eq!(a["result"], json!("first"));
        assert_eq!(b["result"], json!("second"));

        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn reader_task_exit_fails_every_pending_waiter() {
        let (reader, stdin, mut child) = reader_over_pipe().await;
        let pending = Arc::new(PendingRequests::new());
        let (unmatched_tx, _unmatched_rx) = mpsc::channel(UNMATCHED_FRAME_QUEUE_CAPACITY);
        let task = spawn_reader_task(reader, Arc::clone(&pending), unmatched_tx);

        let rx_a = pending.register(&json!("a")).await;
        let rx_b = pending.register(&json!("b")).await;

        // Closing stdin makes `cat` exit, which yields EOF on its stdout.
        drop(stdin);
        task.await.expect("reader task joins");

        assert_eq!(recv(rx_a).await, Err(DemuxRecvError::ReaderClosed));
        assert_eq!(recv(rx_b).await, Err(DemuxRecvError::ReaderClosed));
        assert_eq!(pending.len().await, 0);

        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn cancel_drops_a_registration_without_failing_others() {
        let (reader, mut stdin, mut child) = reader_over_pipe().await;
        let pending = Arc::new(PendingRequests::new());
        let (unmatched_tx, _unmatched_rx) = mpsc::channel(UNMATCHED_FRAME_QUEUE_CAPACITY);
        let _task = spawn_reader_task(reader, Arc::clone(&pending), unmatched_tx);

        let _rx_a = pending.register(&json!("a")).await;
        let rx_b = pending.register(&json!("b")).await;
        pending.cancel(&json!("a")).await;
        assert_eq!(pending.len().await, 1);

        write_line(&mut stdin, &json!({"jsonrpc": "2.0", "id": "b", "result": "ok"})).await;
        assert_eq!(recv(rx_b).await.unwrap()["result"], json!("ok"));

        let _ = child.start_kill();
    }
}
