//! Multi-client JSON-RPC server, per `05-multi-client-concurrency.md` and the
//! session-binding model in `01-jsonrpc-spec.md`.
//!
//! Shape, matching the docs' architecture diagram exactly:
//!
//! - `tokio::net::UnixListener::accept()` loop spawns one task-pair
//!   (reader + writer) per connection — this is the "many connections" half.
//! - Every connection funnels its parsed, session-validated requests into a
//!   **single** shared dispatcher task (via an unbounded mpsc channel) that
//!   owns the `Backend` trait object and calls it exactly once per request,
//!   strictly FIFO across all connections. This is the "one dispatcher owns
//!   the backend" half — the in-process stand-in for `05`'s
//!   `BlockingQueuedConnection`-onto-the-Qt-main-thread serialization: a
//!   `MockBackend` has no thread-affinity requirement of its own, but routing
//!   every mutation through one task here means a real Qt-backed `Backend`
//!   impl can be dropped in later without changing this file at all.
//! - Mutating calls that succeed carry a notification (`edit.changed`,
//!   `notes.changed`, `project.dirty`) that gets published on a per-project
//!   `tokio::sync::broadcast` channel, so every connection currently bound to
//!   that project (not just the requester) receives it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::backend::{Backend, BackendError};
use crate::framing::{self, FramingError};
use crate::protocol::{error_codes, RpcError, RpcNotification, RpcRequest, RpcResponse};

/// Configuration handed to [`serve`]: where to listen, and the token
/// `sap.hello` must present before a connection is allowed to do anything
/// else, per `01-jsonrpc-spec.md`'s session-binding model.
pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub token: String,
    /// Optional audio convenience methods are not callable until the daemon
    /// explicitly enables this capability for the child process.
    pub audio_enabled: bool,
}

/// Per-connection session state. Two gates, enforced in order: `sap.hello`
/// must succeed before anything else is accepted, then `project.select`
/// must succeed before any project-scoped method is accepted.
#[derive(Default)]
struct Session {
    authenticated: bool,
    project_id: Option<String>,
}

/// Outcome of running one op against the backend: the RPC result to send
/// back to the requester, plus an optional notification to fan out to every
/// connection bound to the same project (only published when `result` is
/// `Ok`, see [`handle_request`]).
struct BackendCallResult {
    result: Result<Value, RpcError>,
    notify: Option<RpcNotification>,
}

fn ok_result(v: Value) -> BackendCallResult {
    BackendCallResult { result: Ok(v), notify: None }
}

fn err_result(e: BackendError) -> BackendCallResult {
    BackendCallResult { result: Err(backend_err_to_rpc(e)), notify: None }
}

fn backend_err_to_rpc(e: BackendError) -> RpcError {
    match e {
        BackendError::InvalidParams(msg) => {
            RpcError { code: error_codes::INVALID_PARAMS, message: msg, data: None }
        }
        BackendError::NotFound(msg) => {
            RpcError { code: error_codes::NOT_FOUND, message: msg, data: None }
        }
        BackendError::Unsupported(msg) => {
            RpcError { code: error_codes::INTERNAL_ERROR, message: msg, data: None }
        }
    }
}

fn rpc_error(code: i64) -> RpcError {
    RpcError { code, message: error_codes::message(code).to_string(), data: None }
}

fn invalid_params(e: &serde_json::Error) -> RpcError {
    RpcError {
        code: error_codes::INVALID_PARAMS,
        message: format!("invalid params: {e}"),
        data: None,
    }
}

fn method_not_found(method: &str) -> RpcError {
    RpcError {
        code: error_codes::METHOD_NOT_FOUND,
        message: format!("method not found: {method}"),
        data: None,
    }
}

fn internal_error(msg: &str) -> RpcError {
    RpcError { code: error_codes::INTERNAL_ERROR, message: msg.to_string(), data: None }
}

/// A single unit of work for the dispatcher: a closure that calls one
/// `Backend` method (params already parsed and validated) and packages the
/// result, boxed so the dispatcher's match-on-method-name logic
/// (`build_op`) and its single point of `&mut dyn Backend` access
/// (`run_dispatcher`) can live in different places without either one
/// knowing about the other's internals.
type BackendOp = Box<dyn FnOnce(&mut dyn Backend) -> BackendCallResult + Send>;

struct DispatchMsg {
    op: BackendOp,
    respond_to: oneshot::Sender<BackendCallResult>,
}

type DispatchSender = mpsc::UnboundedSender<DispatchMsg>;

/// Per-project notification fan-out channels. Deliberately a plain
/// `Mutex<HashMap<..>>` shared between the dispatcher and every connection
/// task, *separate* from the single-writer `Backend` access above: managing
/// which channel exists for which project is not a Backend mutation, so it
/// doesn't need to be serialized through the dispatcher — only creating a
/// `broadcast::Sender`/subscribing to it, both cheap, non-blocking, and
/// `Send + Sync`-safe under a short-lived lock.
type ProjectChannels = Arc<Mutex<HashMap<String, broadcast::Sender<RpcNotification>>>>;

fn channel_for_project(channels: &ProjectChannels, project_id: &str) -> broadcast::Sender<RpcNotification> {
    let mut map = channels.lock().expect("project channel map poisoned");
    map.entry(project_id.to_string())
        .or_insert_with(|| broadcast::channel(256).0)
        .clone()
}

/// Sends one op to the shared dispatcher and awaits its result. This is the
/// only way any connection task ever touches the backend.
async fn dispatch(tx: &DispatchSender, op: BackendOp) -> BackendCallResult {
    let (respond_to, rx) = oneshot::channel();
    if tx.send(DispatchMsg { op, respond_to }).is_err() {
        return BackendCallResult {
            result: Err(internal_error("dispatcher unavailable")),
            notify: None,
        };
    }
    match rx.await {
        Ok(outcome) => outcome,
        Err(_) => BackendCallResult {
            result: Err(internal_error("dispatcher dropped the response")),
            notify: None,
        },
    }
}

/// The single task that owns the `Backend`. Every connection's requests
/// funnel through `rx` in strict arrival order (FIFO across all
/// connections, per `05-multi-client-concurrency.md`'s "explicit FIFO queue"
/// requirement) and are applied one at a time.
async fn run_dispatcher<B: Backend>(mut backend: B, mut rx: mpsc::UnboundedReceiver<DispatchMsg>) {
    while let Some(msg) = rx.recv().await {
        let outcome = (msg.op)(&mut backend);
        let _ = msg.respond_to.send(outcome);
    }
}

/// Routes a project-scoped method (session already authenticated and bound)
/// to a boxed `Backend` call, parsing and validating `params` up front so
/// the dispatcher never sees malformed input. Mutating methods attach the
/// notification that must fan out to the project's other connections on
/// success, per the doc's "comprehensive fan-out requirement".
fn build_op(method: &str, params: Value, project_id: String) -> Result<BackendOp, RpcError> {
    match method {
        "project.getState" => Ok(Box::new(move |b| match b.project_get_state(&project_id) {
            Ok(s) => ok_result(serde_json::to_value(&s).expect("ProjectState serializes")),
            Err(e) => err_result(e),
        })),

        "project.save" => Ok(Box::new(move |b| match b.project_save(&project_id) {
            Ok(()) => BackendCallResult {
                result: Ok(json!({})),
                notify: Some(RpcNotification::new("project.dirty", json!({"reason": "save"}))),
            },
            Err(e) => err_result(e),
        })),

        "project.undo" => Ok(Box::new(move |b| match b.project_undo(&project_id) {
            Ok(()) => BackendCallResult {
                result: Ok(json!({})),
                notify: Some(RpcNotification::new("project.dirty", json!({"reason": "undo"}))),
            },
            Err(e) => err_result(e),
        })),

        "project.redo" => Ok(Box::new(move |b| match b.project_redo(&project_id) {
            Ok(()) => BackendCallResult {
                result: Ok(json!({})),
                notify: Some(RpcNotification::new("project.dirty", json!({"reason": "redo"}))),
            },
            Err(e) => err_result(e),
        })),

        "edit.addTrack" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                kind: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.edit_add_track(&project_id, &p.kind) {
                Ok(track) => BackendCallResult {
                    result: Ok(serde_json::to_value(&track).expect("Track serializes")),
                    notify: Some(RpcNotification::new(
                        "edit.changed",
                        json!({"reason": "addTrack", "trackIndex": track.index}),
                    )),
                },
                Err(e) => err_result(e),
            }))
        }

        "edit.removeTrack" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.edit_remove_track(&project_id, p.track_index) {
                Ok(()) => BackendCallResult {
                    result: Ok(json!({})),
                    notify: Some(RpcNotification::new(
                        "edit.changed",
                        json!({"reason": "removeTrack", "trackIndex": p.track_index}),
                    )),
                },
                Err(e) => err_result(e),
            }))
        }

        "edit.listTracks" => Ok(Box::new(move |b| match b.edit_list_tracks(&project_id) {
            Ok(tracks) => ok_result(serde_json::to_value(&tracks).expect("tracks serialize")),
            Err(e) => err_result(e),
        })),

        "edit.appendClip" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
                source: Value,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            let track_index = p.track_index;
            Ok(Box::new(move |b| match b.edit_append_clip(&project_id, track_index, p.source) {
                Ok(clip) => BackendCallResult {
                    result: Ok(serde_json::to_value(&clip).expect("Clip serializes")),
                    notify: Some(RpcNotification::new(
                        "edit.changed",
                        json!({"reason": "appendClip", "trackIndex": track_index, "clipIndex": clip.index}),
                    )),
                },
                Err(e) => err_result(e),
            }))
        }

        "edit.listClips" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.edit_list_clips(&project_id, p.track_index) {
                Ok(clips) => ok_result(serde_json::to_value(&clips).expect("clips serialize")),
                Err(e) => err_result(e),
            }))
        }

        "playback.seek" => {
            // Not undo-tracked (per 01-jsonrpc-spec.md's playback.* note) and
            // deliberately not in the task's mutating/notification list —
            // no broadcast on success.
            #[derive(Deserialize)]
            struct P {
                frame: i64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.playback_seek(&project_id, p.frame) {
                Ok(()) => ok_result(json!({})),
                Err(e) => err_result(e),
            }))
        }

        "notes.getText" => Ok(Box::new(move |b| match b.notes_get_text(&project_id) {
            Ok(text) => ok_result(json!({"text": text})),
            Err(e) => err_result(e),
        })),

        "notes.setText" => {
            #[derive(Deserialize)]
            struct P {
                text: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.notes_set_text(&project_id, &p.text) {
                Ok(()) => BackendCallResult {
                    result: Ok(json!({})),
                    notify: Some(RpcNotification::new("notes.changed", json!({"reason": "setText"}))),
                },
                Err(e) => err_result(e),
            }))
        }

        _ => Err(method_not_found(method)),
    }
}

/// Additive dispatch for doc 11 Phase A's method surface (playlist.*,
/// trim/transitions/filter/generator/subtitles/file.export/jobs/playback.getFrame).
/// Kept as a separate function from `build_op` (rather than growing that
/// match arm-by-arm) purely to keep the diff reviewable; `handle_request`
/// tries this first, falling back to `build_op` for the original surface.
fn build_op_ext(
    method: &str,
    params: Value,
    project_id: String,
    audio_enabled: bool,
) -> Result<BackendOp, RpcError> {
    match method {
        "file.probe" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.file_probe(&p.path) {
                Ok(probe) => ok_result(serde_json::to_value(&probe).expect("FileProbe serializes")),
                Err(e) => err_result(e),
            }))
        }

        "audio.setGain" => {
            if !audio_enabled {
                return Err(method_not_found(method));
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                clip_id: String,
                db: f64,
                #[serde(default)]
                position: Option<i64>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            if !p.db.is_finite() {
                return Err(RpcError {
                    code: error_codes::INVALID_PARAMS,
                    message: "audio.setGain db must be finite".to_string(),
                    data: None,
                });
            }
            Ok(Box::new(move |b| {
                let initial = if p.position.is_none() {
                    json!({"level": p.db})
                } else {
                    json!({})
                };
                match b.filter_add(&project_id, &p.clip_id, "volume", initial) {
                    Ok(info) => {
                        if let Some(position) = p.position {
                            if let Err(e) = b.filter_set_property(
                                &project_id,
                                &p.clip_id,
                                info.filter_index,
                                "level",
                                json!(p.db),
                                Some(position),
                            ) {
                                return err_result(e);
                            }
                        }
                        BackendCallResult {
                            result: Ok(serde_json::to_value(&info).expect("FilterInfo serializes")),
                            notify: Some(RpcNotification::new(
                                "filter.changed",
                                json!({"clipId": p.clip_id, "filterIndex": info.filter_index, "reason": "audio.setGain"}),
                            )),
                        }
                    }
                    Err(e) => err_result(e),
                }
            }))
        }

        "playlist.append" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                source: Value,
                #[serde(default)]
                name: Option<String>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.playlist_append(&project_id, p.source, p.name) {
                Ok(entry) => BackendCallResult {
                    result: Ok(serde_json::to_value(&entry).expect("PlaylistEntry serializes")),
                    notify: Some(RpcNotification::new(
                        "playlist.changed",
                        json!({"reason": "append", "index": entry.index}),
                    )),
                },
                Err(e) => err_result(e),
            }))
        }

        "playlist.list" => Ok(Box::new(move |b| match b.playlist_list(&project_id) {
            Ok(entries) => ok_result(serde_json::to_value(&entries).expect("entries serialize")),
            Err(e) => err_result(e),
        })),

        "file.import" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.file_import(&project_id, &p.path) {
                Ok(entry) => BackendCallResult {
                    result: Ok(serde_json::to_value(&entry).expect("PlaylistEntry serializes")),
                    notify: Some(RpcNotification::new(
                        "playlist.changed",
                        json!({"reason": "import", "index": entry.index}),
                    )),
                },
                Err(e) => err_result(e),
            }))
        }

        "edit.trimClipIn" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
                clip_index: usize,
                new_frame: i64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.edit_trim_clip_in(&project_id, p.track_index, p.clip_index, p.new_frame) {
                    Ok(()) => BackendCallResult {
                        result: Ok(json!({})),
                        notify: Some(RpcNotification::new(
                            "edit.changed",
                            json!({"reason": "trimClipIn", "trackIndex": p.track_index, "clipIndex": p.clip_index}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "edit.trimClipOut" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
                clip_index: usize,
                new_frame: i64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.edit_trim_clip_out(&project_id, p.track_index, p.clip_index, p.new_frame) {
                    Ok(()) => BackendCallResult {
                        result: Ok(json!({})),
                        notify: Some(RpcNotification::new(
                            "edit.changed",
                            json!({"reason": "trimClipOut", "trackIndex": p.track_index, "clipIndex": p.clip_index}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "transitions.addCrossfade" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
                between_clips: (usize, usize),
                duration_frames: i64,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.transitions_add_crossfade(&project_id, p.track_index, p.between_clips, p.duration_frames) {
                    Ok(info) => BackendCallResult {
                        result: Ok(serde_json::to_value(&info).expect("TransitionInfo serializes")),
                        notify: Some(RpcNotification::new(
                            "transitions.changed",
                            json!({"trackIndex": p.track_index}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "filter.add" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                clip_id: String,
                mlt_service: String,
                #[serde(default)]
                properties: Value,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.filter_add(&project_id, &p.clip_id, &p.mlt_service, p.properties) {
                Ok(info) => BackendCallResult {
                    result: Ok(serde_json::to_value(&info).expect("FilterInfo serializes")),
                    notify: Some(RpcNotification::new(
                        "filter.changed",
                        json!({"clipId": p.clip_id, "filterIndex": info.filter_index}),
                    )),
                },
                Err(e) => err_result(e),
            }))
        }

        "filter.setProperty" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                clip_id: String,
                filter_index: usize,
                property: String,
                value: Value,
                #[serde(default)]
                position: Option<i64>,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.filter_set_property(
                    &project_id,
                    &p.clip_id,
                    p.filter_index,
                    &p.property,
                    p.value,
                    p.position,
                ) {
                    Ok(()) => BackendCallResult {
                        result: Ok(json!({})),
                        notify: Some(RpcNotification::new(
                            "filter.changed",
                            json!({"clipId": p.clip_id, "filterIndex": p.filter_index}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "filter.addKeyframe" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                clip_id: String,
                filter_index: usize,
                property: String,
                position: i64,
                value: Value,
                #[serde(default = "default_interpolation")]
                interpolation: String,
            }
            fn default_interpolation() -> String {
                "linear".to_string()
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.filter_add_keyframe(
                    &project_id,
                    &p.clip_id,
                    p.filter_index,
                    &p.property,
                    p.position,
                    p.value,
                    &p.interpolation,
                ) {
                    Ok(()) => BackendCallResult {
                        result: Ok(json!({})),
                        notify: Some(RpcNotification::new(
                            "filter.changed",
                            json!({"clipId": p.clip_id, "filterIndex": p.filter_index}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "generator.createTitle" => Ok(Box::new(move |b| match b.generator_create_title(&project_id, params) {
            Ok(entry) => ok_result(serde_json::to_value(&entry).expect("PlaylistEntry serializes")),
            Err(e) => err_result(e),
        })),

        "subtitles.addTrack" => Ok(Box::new(move |b| match b.subtitles_add_track(&project_id) {
            Ok(info) => BackendCallResult {
                result: Ok(serde_json::to_value(&info).expect("SubtitleTrackInfo serializes")),
                notify: Some(RpcNotification::new("subtitles.changed", json!({"reason": "addTrack"}))),
            },
            Err(e) => err_result(e),
        })),

        "subtitles.appendItem" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                track_index: usize,
                start_frame: i64,
                end_frame: i64,
                text: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.subtitles_append_item(&project_id, p.track_index, p.start_frame, p.end_frame, &p.text) {
                    Ok(()) => BackendCallResult {
                        result: Ok(json!({})),
                        notify: Some(RpcNotification::new(
                            "subtitles.changed",
                            json!({"reason": "appendItem", "trackIndex": p.track_index}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "file.export" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                output_path: String,
                #[serde(default = "default_codec")]
                codec: String,
                #[serde(default = "default_container")]
                container: String,
            }
            fn default_codec() -> String {
                "h264".to_string()
            }
            fn default_container() -> String {
                "mp4".to_string()
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| {
                match b.file_export(&project_id, &p.output_path, &p.codec, &p.container) {
                    Ok(job_id) => BackendCallResult {
                        result: Ok(json!({"jobId": job_id})),
                        notify: Some(RpcNotification::new(
                            "jobs.changed",
                            json!({"jobId": job_id, "status": "running"}),
                        )),
                    },
                    Err(e) => err_result(e),
                }
            }))
        }

        "jobs.get" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                job_id: String,
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.jobs_get(&p.job_id) {
                Ok(status) => ok_result(serde_json::to_value(&status).expect("JobStatus serializes")),
                Err(e) => err_result(e),
            }))
        }

        "jobs.list" => Ok(Box::new(move |b| match b.jobs_list(&project_id) {
            Ok(statuses) => ok_result(serde_json::to_value(&statuses).expect("job statuses serialize")),
            Err(e) => err_result(e),
        })),

        "playback.getFrame" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct P {
                frame: i64,
                #[serde(default = "default_format")]
                format: String,
            }
            fn default_format() -> String {
                "jpeg".to_string()
            }
            let p: P = serde_json::from_value(params).map_err(|e| invalid_params(&e))?;
            Ok(Box::new(move |b| match b.playback_get_frame(&project_id, p.frame, &p.format) {
                Ok(data_b64) => ok_result(json!({"format": p.format, "data": data_b64})),
                Err(e) => err_result(e),
            }))
        }

        _ => Err(method_not_found(method)),
    }
}

/// Handles one parsed request against the connection's session state.
/// Returns `None` for id-less (fire-and-forget) requests, matching JSON-RPC
/// 2.0 notification semantics — SAP clients are expected to always send an
/// `id`, but nothing here assumes it.
async fn handle_request(
    req: RpcRequest,
    session: &mut Session,
    token: &str,
    dispatch_tx: &DispatchSender,
    channels: &ProjectChannels,
    notif_rx: &mut Option<broadcast::Receiver<RpcNotification>>,
    audio_enabled: bool,
) -> Option<RpcResponse> {
    let id = req.id.clone();
    let respond = |result: Result<Value, RpcError>| -> Option<RpcResponse> {
        id.map(|id| match result {
            Ok(v) => RpcResponse::ok(id, v),
            Err(e) => RpcResponse::err(id, e),
        })
    };

    // Gate 1: sap.hello must be the very first thing accepted on a
    // connection, per 01-jsonrpc-spec.md's session-binding model.
    if req.method == "sap.hello" {
        #[derive(Deserialize)]
        struct P {
            token: String,
        }
        return match serde_json::from_value::<P>(req.params) {
            Ok(p) if p.token == token => {
                session.authenticated = true;
                respond(Ok(json!({"ok": true})))
            }
            Ok(_) => respond(Err(rpc_error(error_codes::BAD_TOKEN))),
            Err(e) => respond(Err(invalid_params(&e))),
        };
    }

    if !session.authenticated {
        return respond(Err(rpc_error(error_codes::UNAUTHENTICATED)));
    }

    // project.exit is session-level (unbind), not itself project-scoped, so
    // it doesn't require an existing binding — calling it while unbound is a
    // harmless no-op, matching "exit" being idempotent.
    if req.method == "project.exit" {
        return match session.project_id.take() {
            Some(_project_id) => {
                let outcome = dispatch(
                    dispatch_tx,
                    Box::new(move |b| match b.project_exit() {
                        Ok(()) => ok_result(json!({})),
                        Err(e) => err_result(e),
                    }),
                )
                .await;
                *notif_rx = None;
                respond(outcome.result)
            }
            None => respond(Ok(json!({}))),
        };
    }

    // `file.probe` is file-scoped metadata inspection, not a project
    // mutation, so authentication is sufficient; it must not require a
    // project binding.
    if req.method == "file.probe" {
        let op = match build_op_ext(&req.method, req.params, String::new(), audio_enabled) {
            Ok(op) => op,
            Err(e) => return respond(Err(e)),
        };
        let outcome = dispatch(dispatch_tx, op).await;
        return respond(outcome.result);
    }

    // Gate 2: project.select must succeed before anything project-scoped.
    if req.method == "project.select" {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct P {
            project_id: String,
        }
        let p: P = match serde_json::from_value(req.params) {
            Ok(p) => p,
            Err(e) => return respond(Err(invalid_params(&e))),
        };
        let project_id = p.project_id;
        let dispatch_project_id = project_id.clone();
        let outcome = dispatch(
            dispatch_tx,
            Box::new(move |b| match b.project_select(&dispatch_project_id) {
                Ok(state) => ok_result(serde_json::to_value(&state).expect("ProjectState serializes")),
                Err(e) => err_result(e),
            }),
        )
        .await;
        if outcome.result.is_ok() {
            let sender = channel_for_project(channels, &project_id);
            *notif_rx = Some(sender.subscribe());
            session.project_id = Some(project_id);
        }
        return respond(outcome.result);
    }

    // Every remaining method (edit.*/playback.*/notes.*/project.save|undo|
    // redo|getState) requires an active project binding.
    let project_id = match session.project_id.clone() {
        Some(p) => p,
        None => return respond(Err(rpc_error(error_codes::NO_PROJECT_BOUND))),
    };

    let op = match build_op(&req.method, req.params.clone(), project_id.clone()) {
        Ok(op) => op,
        Err(_) => match build_op_ext(&req.method, req.params, project_id.clone(), audio_enabled) {
            Ok(op) => op,
            Err(e) => return respond(Err(e)),
        },
    };

    let outcome = dispatch(dispatch_tx, op).await;
    if outcome.result.is_ok() {
        if let Some(notification) = &outcome.notify {
            let sender = channel_for_project(channels, &project_id);
            // Ignore "no receivers" errors: a project with only one bound
            // connection (the requester, who already gets the RPC result)
            // has nothing to fan out to yet, which is not a failure.
            let _ = sender.send(notification.clone());
        }
    }
    respond(outcome.result)
}

/// Pulls the next fanned-out notification for this connection, if it's
/// bound to a project. `tokio::select!` needs an always-pollable,
/// cancel-safe future to race against incoming requests regardless of
/// whether a project is currently bound, hence this small wrapper.
async fn recv_notification(rx: &mut Option<broadcast::Receiver<RpcNotification>>) -> Option<RpcNotification> {
    match rx {
        Some(r) => match r.recv().await {
            Ok(n) => Some(n),
            Err(broadcast::error::RecvError::Lagged(_)) => None,
            Err(broadcast::error::RecvError::Closed) => {
                *rx = None;
                None
            }
        },
        None => std::future::pending().await,
    }
}

/// Dedicated writer task: owns the write half and serializes all outbound
/// frames (responses interleaved with notifications) onto the wire for this
/// one connection.
async fn writer_loop(mut write_half: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Value>) {
    while let Some(value) = rx.recv().await {
        if framing::write_message(&mut write_half, &value).await.is_err() {
            break;
        }
    }
}

/// Dedicated reader task: owns the read half and turns the framed byte
/// stream into parsed `Value`s (or a terminal `FramingError`), so the main
/// connection loop only ever needs to `select!` on cancel-safe channel
/// receives — `framing::read_message` itself is not cancel-safe (a
/// `select!` cancellation mid-read would silently drop already-consumed
/// bytes), so it must not be raced directly against another future.
async fn reader_loop(read_half: OwnedReadHalf, tx: mpsc::UnboundedSender<Result<Value, FramingError>>) {
    let mut reader = BufReader::new(read_half);
    loop {
        let msg = framing::read_message(&mut reader).await;
        let is_terminal = msg.is_err();
        if tx.send(msg).is_err() || is_terminal {
            break;
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    dispatch_tx: DispatchSender,
    channels: ProjectChannels,
    token: String,
    audio_enabled: bool,
) {
    let (read_half, write_half) = stream.into_split();

    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Result<Value, FramingError>>();
    tokio::spawn(reader_loop(read_half, in_tx));

    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_handle = tokio::spawn(writer_loop(write_half, out_rx));

    let mut session = Session::default();
    let mut notif_rx: Option<broadcast::Receiver<RpcNotification>> = None;

    loop {
        tokio::select! {
            maybe_msg = in_rx.recv() => {
                match maybe_msg {
                    None | Some(Err(_)) => break,
                    Some(Ok(value)) => {
                        let req: RpcRequest = match serde_json::from_value(value) {
                            Ok(r) => r,
                            Err(_) => {
                                let resp = RpcResponse::err(Value::Null, rpc_error(error_codes::INVALID_REQUEST));
                                let _ = out_tx.send(serde_json::to_value(&resp).expect("response serializes"));
                                continue;
                            }
                        };
                        let response = handle_request(
                            req,
                            &mut session,
                            &token,
                            &dispatch_tx,
                            &channels,
                            &mut notif_rx,
                            audio_enabled,
                        )
                        .await;
                        if let Some(resp) = response {
                            let _ = out_tx.send(serde_json::to_value(&resp).expect("response serializes"));
                        }
                    }
                }
            }
            maybe_notif = recv_notification(&mut notif_rx) => {
                if let Some(notification) = maybe_notif {
                    let _ = out_tx.send(serde_json::to_value(&notification).expect("notification serializes"));
                }
            }
        }
    }

    drop(out_tx);
    let _ = writer_handle.await;
}

/// Binds `config.socket_path`, starts the shared dispatcher task that owns
/// `backend`, then accepts connections forever, spawning one task pair per
/// connection. Never returns except on a fatal listener error.
pub async fn serve<B: Backend + 'static>(config: ServerConfig, backend: B) -> std::io::Result<()> {
    // A stale socket file from a previous run would otherwise make bind()
    // fail with AddrInUse.
    let _ = std::fs::remove_file(&config.socket_path);
    if let Some(parent) = config.socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&config.socket_path)?;

    let (dispatch_tx, dispatch_rx) = mpsc::unbounded_channel::<DispatchMsg>();
    tokio::spawn(run_dispatcher(backend, dispatch_rx));

    let channels: ProjectChannels = Arc::new(Mutex::new(HashMap::new()));
    let token = config.token;
    let audio_enabled = config.audio_enabled;

    loop {
        let (stream, _addr) = listener.accept().await?;
        let dispatch_tx = dispatch_tx.clone();
        let channels = channels.clone();
        let token = token.clone();
        tokio::spawn(handle_connection(stream, dispatch_tx, channels, token, audio_enabled));
    }
}
