//! The `Backend` trait is the seam between SAP's wire protocol (real, tested)
//! and the actual editing primitives (mocked here — see `ffi.rs` and README.md
//! for what a real Qt/C-ABI-backed implementation will look like per
//! memory/head/gen/rust-fork/02-rust-embedding.md).
//!
//! `MockBackend` implements this trait with plain in-memory state so the rest
//! of the crate (dispatcher, multi-client fan-out, session binding) can be
//! built and tested today without Qt/MLT available.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type BackendResult<T> = Result<T, BackendError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub index: usize,
    pub kind: String, // "video" | "audio"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    /// Stable identifier for this clip, used to address it in filter.* /
    /// transitions.* calls per 01-jsonrpc-spec.md's `clipId` convention.
    /// Not present in the original v1 trait (index-pair addressing only) --
    /// added here since filter.add/addKeyframe need a single opaque handle,
    /// not a (trackIndex, clipIndex) pair that shifts under insert/remove.
    pub clip_id: String,
    pub index: usize,
    pub source: Value,
    /// In/out points within the source, in frames. Defaults to the full
    /// source length until edit.trimClipIn/Out narrows them.
    #[serde(default)]
    pub in_frame: i64,
    #[serde(default)]
    pub out_frame: i64,
}

/// A `playlist.*` bin entry -- distinct from a timeline `Clip`, per
/// 01-jsonrpc-spec.md's "Playlist dock: the source bin" namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    pub index: usize,
    pub name: String,
    pub source: Value,
    pub duration_frames: i64,
}

/// Result of `transitions.addCrossfade`, per 01's `transitions.*` namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionInfo {
    pub track_index: usize,
    pub transition_index: usize,
    pub between_clips: (usize, usize),
    pub duration_frames: i64,
}

/// Result of `filter.add`, per 01's `filter.*` namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterInfo {
    pub filter_index: usize,
    pub mlt_service: String,
}

/// Result of `subtitles.addTrack`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrackInfo {
    pub track_index: usize,
}

/// A `jobs.get` snapshot, per 01's `jobs.*` namespace -- deliberately a
/// small subset (this crate only ever runs export jobs today, not the full
/// heterogeneous `JobQueue` from the real Shotcut GUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub job_id: String,
    /// "running" | "done" | "error"
    pub status: String,
    pub percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Metadata returned by `file.probe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileProbe {
    pub path: String,
    pub duration_seconds: f64,
    pub duration_frames: i64,
    pub codec: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectState {
    pub project_id: String,
    pub dirty: bool,
    pub undo_depth: usize,
    pub redo_depth: usize,
}

/// A meaningful subset of 01-jsonrpc-spec.md's method surface — enough to
/// prove out session binding, mutation + notification fan-out, and undo
/// bookkeeping end to end. Namespaces not covered here (`filter.*`,
/// `playlist.*`, `subtitles.*`, ...) follow the exact same trait-method
/// pattern and are a mechanical, not architectural, extension.
pub trait Backend: Send {
    fn project_select(&mut self, project_id: &str) -> BackendResult<ProjectState>;
    fn project_exit(&mut self) -> BackendResult<()>;
    fn project_get_state(&mut self, project_id: &str) -> BackendResult<ProjectState>;
    fn project_save(&mut self, project_id: &str) -> BackendResult<()>;
    fn project_undo(&mut self, project_id: &str) -> BackendResult<()>;
    fn project_redo(&mut self, project_id: &str) -> BackendResult<()>;

    fn edit_add_track(&mut self, project_id: &str, kind: &str) -> BackendResult<Track>;
    fn edit_remove_track(&mut self, project_id: &str, track_index: usize) -> BackendResult<()>;
    fn edit_list_tracks(&mut self, project_id: &str) -> BackendResult<Vec<Track>>;
    fn edit_append_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        source: Value,
    ) -> BackendResult<Clip>;
    fn edit_list_clips(&mut self, project_id: &str, track_index: usize) -> BackendResult<Vec<Clip>>;

    fn playback_seek(&mut self, project_id: &str, frame: i64) -> BackendResult<()>;

    fn notes_get_text(&mut self, project_id: &str) -> BackendResult<String>;
    fn notes_set_text(&mut self, project_id: &str, text: &str) -> BackendResult<()>;

    // --- Additive extension, doc 11 Phase A surface ---
    // Every method below is new relative to the original trait; existing
    // methods above are unchanged so MockBackend/FfiBackend callers and
    // server.rs's existing dispatch continue to compile unmodified except
    // for the new match arms added there.

    /// `playlist.append` -- add a source to the project's Playlist bin
    /// (distinct from placing it on the timeline). `source` is the same
    /// tagged-union shape as `edit.appendClip`'s (`{path}` today).
    fn playlist_append(
        &mut self,
        project_id: &str,
        source: Value,
        name: Option<String>,
    ) -> BackendResult<PlaylistEntry>;

    fn playlist_list(&mut self, project_id: &str) -> BackendResult<Vec<PlaylistEntry>>;

    /// `file.import` -- import a local file into the project's playlist bin.
    fn file_import(&mut self, project_id: &str, path: &str) -> BackendResult<PlaylistEntry>;

    /// `edit.trimClipIn` / `edit.trimClipOut`.
    fn edit_trim_clip_in(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        new_frame: i64,
    ) -> BackendResult<()>;
    fn edit_trim_clip_out(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        new_frame: i64,
    ) -> BackendResult<()>;

    /// `transitions.addCrossfade`.
    fn transitions_add_crossfade(
        &mut self,
        project_id: &str,
        track_index: usize,
        between_clips: (usize, usize),
        duration_frames: i64,
    ) -> BackendResult<TransitionInfo>;

    /// `filter.add` -- attach an MLT filter (by `mlt_service` name) to the
    /// clip addressed by `clip_id`, with an initial static property map.
    fn filter_add(
        &mut self,
        project_id: &str,
        clip_id: &str,
        mlt_service: &str,
        properties: Value,
    ) -> BackendResult<FilterInfo>;

    /// `filter.setProperty` -- replace a static property, or write one
    /// positioned value to the filter's property curve.
    fn filter_set_property(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        value: Value,
        position: Option<i64>,
    ) -> BackendResult<()>;

    /// `filter.addKeyframe` -- add one keyframe to an already-attached
    /// filter's property curve.
    fn filter_add_keyframe(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        position: i64,
        value: Value,
        interpolation: &str,
    ) -> BackendResult<()>;

    /// `generator.createTitle` -- constructs a title-card producer (color
    /// background + `dynamictext`/`qtext` filter, per 01's `generator.*`
    /// namespace) and adds it to the Playlist bin, ready for
    /// `edit.appendClip({source:{playlistIndex}})` like any other source.
    fn generator_create_title(&mut self, project_id: &str, params: Value) -> BackendResult<PlaylistEntry>;

    fn subtitles_add_track(&mut self, project_id: &str) -> BackendResult<SubtitleTrackInfo>;
    fn subtitles_append_item(
        &mut self,
        project_id: &str,
        track_index: usize,
        start_frame: i64,
        end_frame: i64,
        text: &str,
    ) -> BackendResult<()>;

    /// `file.export` -- returns a `jobId` immediately per 01's async-job
    /// convention; progress/completion is polled via `jobs_get`.
    fn file_export(
        &mut self,
        project_id: &str,
        output_path: &str,
        codec: &str,
        container: &str,
    ) -> BackendResult<String>;

    fn file_probe(&mut self, path: &str) -> BackendResult<FileProbe>;

    fn jobs_get(&mut self, job_id: &str) -> BackendResult<JobStatus>;
    fn jobs_list(&mut self, project_id: &str) -> BackendResult<Vec<JobStatus>>;

    /// `playback.getFrame` -- one-off frame render for agent-side visual
    /// verification. Returns base64-encoded image bytes in `format`.
    fn playback_get_frame(
        &mut self,
        project_id: &str,
        frame: i64,
        format: &str,
    ) -> BackendResult<String>;
}

#[derive(Default)]
struct ProjectData {
    dirty: bool,
    undo_depth: usize,
    redo_depth: usize,
    tracks: Vec<Track>,
    clips: HashMap<usize, Vec<Clip>>, // track_index -> clips
    notes: String,
    playlist: Vec<PlaylistEntry>,
    filters: HashMap<String, Vec<MockFilter>>,
    subtitle_tracks: usize,
    next_clip_id: u64,
    next_job_id: u64,
    jobs: HashMap<String, JobStatus>,
}

#[derive(Default)]
struct MockFilter {
    properties: HashMap<String, Value>,
    keyframes: HashMap<String, HashMap<i64, Value>>,
}

/// Stands in for the real backend until sap_ffi.h/.cpp (per 02-rust-embedding.md)
/// exist and are linked in via the `real_ffi` feature. Every mutating call here
/// bumps `undo_depth` and marks the project dirty, mirroring the real
/// "every mutating primitive pushes a QUndoCommand" invariant from the docs —
/// close enough to exercise the protocol layer honestly, not a claim that undo
/// semantics are actually implemented.
#[derive(Default)]
pub struct MockBackend {
    projects: HashMap<String, ProjectData>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn project_mut(&mut self, project_id: &str) -> &mut ProjectData {
        self.projects.entry(project_id.to_string()).or_default()
    }

    fn state_of(&self, project_id: &str) -> ProjectState {
        let data = self.projects.get(project_id);
        ProjectState {
            project_id: project_id.to_string(),
            dirty: data.map(|d| d.dirty).unwrap_or(false),
            undo_depth: data.map(|d| d.undo_depth).unwrap_or(0),
            redo_depth: data.map(|d| d.redo_depth).unwrap_or(0),
        }
    }
}

impl Backend for MockBackend {
    fn project_select(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        self.project_mut(project_id);
        Ok(self.state_of(project_id))
    }

    fn project_exit(&mut self) -> BackendResult<()> {
        Ok(())
    }

    fn project_get_state(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        Ok(self.state_of(project_id))
    }

    fn project_save(&mut self, project_id: &str) -> BackendResult<()> {
        self.project_mut(project_id).dirty = false;
        Ok(())
    }

    fn project_undo(&mut self, project_id: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if data.undo_depth == 0 {
            return Err(BackendError::NotFound("nothing to undo".into()));
        }
        data.undo_depth -= 1;
        data.redo_depth += 1;
        Ok(())
    }

    fn project_redo(&mut self, project_id: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if data.redo_depth == 0 {
            return Err(BackendError::NotFound("nothing to redo".into()));
        }
        data.redo_depth -= 1;
        data.undo_depth += 1;
        Ok(())
    }

    fn edit_add_track(&mut self, project_id: &str, kind: &str) -> BackendResult<Track> {
        if kind != "video" && kind != "audio" {
            return Err(BackendError::InvalidParams(format!("bad track kind: {kind}")));
        }
        let data = self.project_mut(project_id);
        let track = Track { index: data.tracks.len(), kind: kind.to_string() };
        data.tracks.push(track.clone());
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(track)
    }

    fn edit_remove_track(&mut self, project_id: &str, track_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        data.tracks.remove(track_index);
        data.clips.remove(&track_index);
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn edit_list_tracks(&mut self, project_id: &str) -> BackendResult<Vec<Track>> {
        Ok(self.project_mut(project_id).tracks.clone())
    }

    fn edit_append_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        source: Value,
    ) -> BackendResult<Clip> {
        let data = self.project_mut(project_id);
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        data.next_clip_id += 1;
        let clip_id = format!("clip-{}", data.next_clip_id);
        let clips = data.clips.entry(track_index).or_default();
        let clip = Clip { clip_id, index: clips.len(), source, in_frame: 0, out_frame: 0 };
        clips.push(clip.clone());
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(clip)
    }

    fn edit_list_clips(&mut self, project_id: &str, track_index: usize) -> BackendResult<Vec<Clip>> {
        Ok(self.project_mut(project_id).clips.get(&track_index).cloned().unwrap_or_default())
    }

    fn playback_seek(&mut self, _project_id: &str, _frame: i64) -> BackendResult<()> {
        // Playback is explicitly "not undo-tracked" per 01-jsonrpc-spec.md.
        Ok(())
    }

    fn notes_get_text(&mut self, project_id: &str) -> BackendResult<String> {
        Ok(self.project_mut(project_id).notes.clone())
    }

    fn notes_set_text(&mut self, project_id: &str, text: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        data.notes = text.to_string();
        data.dirty = true;
        Ok(())
    }

    // --- Additive extension: minimal but real in-memory bookkeeping so the
    // mock stays usable for protocol-level tests; MltBackend (mlt_backend.rs)
    // is the implementor that actually renders real video via `melt`.

    fn playlist_append(
        &mut self,
        project_id: &str,
        source: Value,
        name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        let data = self.project_mut(project_id);
        let entry = PlaylistEntry {
            index: data.playlist.len(),
            name: name.unwrap_or_else(|| format!("clip{}", data.playlist.len())),
            source,
            duration_frames: 0,
        };
        data.playlist.push(entry.clone());
        data.dirty = true;
        Ok(entry)
    }

    fn playlist_list(&mut self, project_id: &str) -> BackendResult<Vec<PlaylistEntry>> {
        Ok(self.project_mut(project_id).playlist.clone())
    }

    fn file_import(&mut self, project_id: &str, path: &str) -> BackendResult<PlaylistEntry> {
        self.playlist_append(project_id, Value::from(json!({"path": path})), None)
    }

    fn edit_trim_clip_in(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        new_frame: i64,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let clip = data
            .clips
            .get_mut(&track_index)
            .and_then(|c| c.get_mut(clip_index))
            .ok_or_else(|| BackendError::NotFound(format!("clip {track_index}/{clip_index}")))?;
        clip.in_frame = new_frame;
        data.dirty = true;
        Ok(())
    }

    fn edit_trim_clip_out(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        new_frame: i64,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let clip = data
            .clips
            .get_mut(&track_index)
            .and_then(|c| c.get_mut(clip_index))
            .ok_or_else(|| BackendError::NotFound(format!("clip {track_index}/{clip_index}")))?;
        clip.out_frame = new_frame;
        data.dirty = true;
        Ok(())
    }

    fn transitions_add_crossfade(
        &mut self,
        project_id: &str,
        track_index: usize,
        between_clips: (usize, usize),
        duration_frames: i64,
    ) -> BackendResult<TransitionInfo> {
        let data = self.project_mut(project_id);
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        data.dirty = true;
        Ok(TransitionInfo { track_index, transition_index: 0, between_clips, duration_frames })
    }

    fn filter_add(
        &mut self,
        project_id: &str,
        clip_id: &str,
        mlt_service: &str,
        properties: Value,
    ) -> BackendResult<FilterInfo> {
        let data = self.project_mut(project_id);
        let filters = data.filters.entry(clip_id.to_string()).or_default();
        let mut filter = MockFilter::default();
        if let Value::Object(properties) = properties {
            filter.properties = properties.into_iter().collect();
        }
        let filter_index = filters.len();
        filters.push(filter);
        data.dirty = true;
        Ok(FilterInfo { filter_index, mlt_service: mlt_service.to_string() })
    }

    fn filter_set_property(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        value: Value,
        position: Option<i64>,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let filter = data
            .filters
            .get_mut(clip_id)
            .and_then(|filters| filters.get_mut(filter_index))
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        match position {
            Some(position) => {
                filter.keyframes.entry(property.to_string()).or_default().insert(position, value);
            }
            None => {
                filter.properties.insert(property.to_string(), value);
            }
        }
        data.dirty = true;
        Ok(())
    }

    fn filter_add_keyframe(
        &mut self,
        project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
        _property: &str,
        _position: i64,
        _value: Value,
        _interpolation: &str,
    ) -> BackendResult<()> {
        self.project_mut(project_id).dirty = true;
        Ok(())
    }

    fn generator_create_title(&mut self, project_id: &str, params: Value) -> BackendResult<PlaylistEntry> {
        let data = self.project_mut(project_id);
        let entry = PlaylistEntry {
            index: data.playlist.len(),
            name: "title".to_string(),
            source: params,
            duration_frames: 0,
        };
        data.playlist.push(entry.clone());
        data.dirty = true;
        Ok(entry)
    }

    fn subtitles_add_track(&mut self, project_id: &str) -> BackendResult<SubtitleTrackInfo> {
        let data = self.project_mut(project_id);
        let track_index = data.subtitle_tracks;
        data.subtitle_tracks += 1;
        data.dirty = true;
        Ok(SubtitleTrackInfo { track_index })
    }

    fn subtitles_append_item(
        &mut self,
        project_id: &str,
        track_index: usize,
        _start_frame: i64,
        _end_frame: i64,
        _text: &str,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if track_index >= data.subtitle_tracks {
            return Err(BackendError::NotFound(format!("subtitle track {track_index}")));
        }
        data.dirty = true;
        Ok(())
    }

    fn file_export(
        &mut self,
        project_id: &str,
        _output_path: &str,
        _codec: &str,
        _container: &str,
    ) -> BackendResult<String> {
        let data = self.project_mut(project_id);
        data.next_job_id += 1;
        let job_id = format!("mock-job-{}", data.next_job_id);
        data.jobs.insert(
            job_id.clone(),
            JobStatus { job_id: job_id.clone(), status: "done".into(), percent: 100.0, result_path: None, error: None },
        );
        Ok(job_id)
    }

    fn file_probe(&mut self, _path: &str) -> BackendResult<FileProbe> {
        Err(BackendError::Unsupported("file.probe is not available in MockBackend".into()))
    }

    fn jobs_get(&mut self, job_id: &str) -> BackendResult<JobStatus> {
        for data in self.projects.values() {
            if let Some(job) = data.jobs.get(job_id) {
                return Ok(job.clone());
            }
        }
        Err(BackendError::NotFound(format!("job {job_id}")))
    }

    fn jobs_list(&mut self, project_id: &str) -> BackendResult<Vec<JobStatus>> {
        let data = self.project_mut(project_id);
        let mut jobs = data.jobs.values().cloned().collect::<Vec<_>>();
        jobs.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        Ok(jobs)
    }

    fn playback_get_frame(
        &mut self,
        _project_id: &str,
        _frame: i64,
        _format: &str,
    ) -> BackendResult<String> {
        Err(BackendError::NotFound("playback.getFrame not implemented in MockBackend".into()))
    }
}
