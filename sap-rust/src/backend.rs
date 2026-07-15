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
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub index: usize,
    pub kind: String, // "video" | "audio"
    /// Audio muted -- bit 2 (value 2) of the real MLT `<track hide=".."/>`
    /// bitmask (see `multitrackmodel.cpp::setTrackMute`). Defaulted so
    /// existing `Track { index, kind }` struct literals / wire payloads
    /// from before this field existed keep compiling and deserializing.
    #[serde(default)]
    pub muted: bool,
    /// Video hidden -- bit 1 (value 1) of the same `hide` bitmask (see
    /// `multitrackmodel.cpp::setTrackHidden`). Independent of `kind ==
    /// "audio"`, which always implies video-hidden regardless of this flag.
    #[serde(default)]
    pub hidden: bool,
    /// UI edit-guard only, matches real Shotcut's `shotcut:lock` track
    /// property -- has no effect on rendered/exported output.
    #[serde(default)]
    pub locked: bool,
    /// QPainter composition-mode index as a string, matching the
    /// `qtblend` transition's `compositing` property in real Shotcut's
    /// per-track blend mode combo (`trackpropertieswidget.cpp`).
    /// `"0"` = Source Over / Normal, the default.
    #[serde(default = "default_blend_mode")]
    pub blend_mode: String,
}

pub(crate) fn default_blend_mode() -> String {
    "0".to_string()
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

/// Result of `playlist.get` -- full metadata for one playlist bin entry,
/// including probe data where available. `probe` is `None` for entries
/// this backend cannot probe (e.g. `MockBackend`, or a non-file-backed
/// source like a generator/blank spacer) rather than the call failing --
/// only an out-of-range `index` is an error here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntryDetail {
    pub index: usize,
    pub name: String,
    pub source: Value,
    pub duration_frames: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<FileProbe>,
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

/// One entry from `filter.list`, per 01's `filter.*` namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterListEntry {
    pub index: usize,
    pub mlt_service: String,
    pub properties: Value,
}

/// One keyframe from `filter.listKeyframes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyframeInfo {
    pub position: i64,
    pub value: Value,
    /// `"linear"` | `"smooth"` | `"discrete"`
    pub interpolation: String,
}

/// Result of `edit.splitClip`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitClipResult {
    pub left_clip_id: String,
    pub right_clip_id: String,
    pub left_index: usize,
    pub right_index: usize,
}

/// Result of `subtitles.addTrack`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrackInfo {
    pub track_index: usize,
}

/// A timeline marker, per 01's `markers.*` namespace (`MarkersModel::Marker`:
/// text / start / end / color). Wire shape uses `frame` for the start
/// position and optional `endFrame` for range markers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub index: usize,
    pub frame: i64,
    pub text: String,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_frame: Option<i64>,
}

/// A `jobs.get` snapshot, per 01's `jobs.*` namespace -- deliberately a
/// small subset (this crate only ever runs export jobs today, not the full
/// heterogeneous `JobQueue` from the real Shotcut GUI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub job_id: String,
    /// "running" | "done" | "error" | "stopped"
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

    /// `playlist.insert` -- insert a source into the Playlist bin at
    /// `index` (shifting existing entries at/after `index` up by one),
    /// per `PlaylistModel::insert()`. `name` follows the same optional
    /// convention as `playlist_append`.
    fn playlist_insert(
        &mut self,
        project_id: &str,
        index: usize,
        source: Value,
        name: Option<String>,
    ) -> BackendResult<PlaylistEntry>;

    /// `playlist.remove` -- remove the entry at `index` (shifting
    /// subsequent entries down by one), per `PlaylistModel::remove()`.
    fn playlist_remove(&mut self, project_id: &str, index: usize) -> BackendResult<()>;

    /// `playlist.move` -- move the entry at `from_index` to `to_index`,
    /// per `PlaylistModel::move()`.
    fn playlist_move(
        &mut self,
        project_id: &str,
        from_index: usize,
        to_index: usize,
    ) -> BackendResult<()>;

    /// `playlist.get` -- full metadata for one playlist bin entry,
    /// including probe data where available (see `PlaylistEntryDetail`).
    fn playlist_get(&mut self, project_id: &str, index: usize) -> BackendResult<PlaylistEntryDetail>;

    // Note: `playlist.addToTimeline` has no dedicated trait method -- per
    // 01-jsonrpc-spec.md it's a pure convenience wrapper equivalent to
    // `edit.appendClip({source: {playlistIndex: index}})`, so server.rs
    // dispatches it straight to the existing `edit_append_clip` rather than
    // this trait growing a near-duplicate method.

    /// `file.import` -- import a local file into the project's playlist bin.
    fn file_import(&mut self, project_id: &str, path: &str) -> BackendResult<PlaylistEntry>;

   /// `edit.trimClipIn` / `edit.trimClipOut`.
   fn edit_trim_clip_in(
       &mut self,
       project_id: &str,
       track_index: usize,
       clip_index: usize,
       new_frame: i64,
        ripple: bool,
   ) -> BackendResult<()>;
   fn edit_trim_clip_out(
       &mut self,
       project_id: &str,
       track_index: usize,
       clip_index: usize,
       new_frame: i64,
        ripple: bool,
   ) -> BackendResult<()>;

    /// `edit.splitClip` -- split a clip at a source frame strictly between
    /// `in_frame` and `out_frame`. Left keeps the original `clip_id` with
    /// `out_frame = position - 1`; right gets a new `clip_id` with
    /// `in_frame = position` (Shotcut-compatible inclusive-frame split).
    fn edit_split_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        position: i64,
    ) -> BackendResult<SplitClipResult>;

    /// `edit.reorderTrack` -- move the track at `from_index` to `to_index`,
    /// shifting the tracks in between (same remove+insert+reindex
    /// semantics as `playlist_move`). Implementors must also remap any
    /// track_index-keyed storage (clips, and MltBackend's transitions) so
    /// each track's clips/crossfades follow it to its new position.
    /// Returns the full, reindexed track list.
    fn edit_reorder_track(
        &mut self,
        project_id: &str,
        from_index: usize,
        to_index: usize,
    ) -> BackendResult<Vec<Track>>;

    /// `edit.setTrackProperties` -- partial update of mute/hidden/locked/
    /// blendMode on one track; `None` fields are left unchanged. Returns
    /// the updated `Track`.
    fn edit_set_track_properties(
        &mut self,
        project_id: &str,
        track_index: usize,
        muted: Option<bool>,
        hidden: Option<bool>,
        locked: Option<bool>,
        blend_mode: Option<String>,
    ) -> BackendResult<Track>;

    /// `edit.setTrackHeight` -- real Shotcut stores this as ONE
    /// project-wide timeline row height (`shotcut:trackHeight` on the
    /// tractor), not per track.
    fn edit_set_track_height(&mut self, project_id: &str, height: i64) -> BackendResult<()>;

    /// `edit.removeClip` -- remove the clip at `clip_index` on
    /// `track_index`, reindexing the remaining clips on that track.
    fn edit_remove_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
    ) -> BackendResult<()>;

    /// `edit.moveClip` -- move/reposition a clip within the same track or
    /// across tracks. `to_clip_index == <dest track's clip count>` means
    /// "append at end". Returns the moved `Clip` (with its updated index).
    fn edit_move_clip(
        &mut self,
        project_id: &str,
        from_track_index: usize,
        from_clip_index: usize,
        to_track_index: usize,
        to_clip_index: usize,
    ) -> BackendResult<Clip>;

    /// `edit.insertClip` -- insert `source` on `track_index` BEFORE the
    /// clip currently at `clip_index` (`clip_index == that track's
    /// current clip count` means "insert at the end", equivalent to
    /// `edit_append_clip`), rippling every downstream clip on that track
    /// forward by the inserted clip's duration. Distinct from
    /// `edit_append_clip` + `edit_move_clip`: this is one undo step, one
    /// real `Timeline::InsertCommand` (real backend) rather than an
    /// append followed by a non-rippling reposition. `clip_index` (not an
    /// absolute frame) for the same reason `edit_move_clip` uses
    /// `to_clip_index` rather than the spec's `toPosition` -- this trait
    /// models a track as an ordered clip list, not raw frame offsets; see
    /// `edit_move_clip`'s doc comment.
    fn edit_insert_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        source: Value,
    ) -> BackendResult<Clip>;

    /// `edit.overwriteClip` -- place `source` on `track_index` starting at
    /// clip-slot `clip_index`, REPLACING whatever clip currently occupies
    /// that slot (non-rippling "drop and replace"), rather than shifting
    /// downstream clips like `edit_insert_clip` does. `clip_index ==
    /// that track's current clip count` means "no clip to replace",
    /// which behaves like `edit_append_clip`. One real
    /// `Timeline::OverwriteCommand` (real backend), one undo step.
    /// `clip_index` (not an absolute frame) for the same reason
    /// `edit_insert_clip`/`edit_move_clip` use a clip-slot index -- this
    /// trait models a track as an ordered clip list, not raw frame
    /// offsets.
    fn edit_overwrite_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        source: Value,
    ) -> BackendResult<Clip>;

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

    /// `filter.list` -- enumerate filters attached to a clip.
    fn filter_list(
        &mut self,
        project_id: &str,
        clip_id: &str,
    ) -> BackendResult<Vec<FilterListEntry>>;

    /// `filter.remove` -- detach a filter and reindex the chain.
    fn filter_remove(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
    ) -> BackendResult<()>;

    /// `filter.reorder` -- move a filter within the clip's filter chain.
    fn filter_reorder(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        new_index: usize,
    ) -> BackendResult<()>;

    /// `filter.listKeyframes` -- return the full curve for one property.
    fn filter_list_keyframes(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
    ) -> BackendResult<Vec<KeyframeInfo>>;

    /// `filter.removeKeyframe` -- remove one keyframe from a property curve.
    fn filter_remove_keyframe(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        position: i64,
    ) -> BackendResult<()>;


    /// Inclusive clip length in frames (`out - in + 1`), used by
    /// `audio.setFadeInOut` to place the fade-out level envelope relative
    /// to the clip end.
    fn clip_length_frames(&mut self, project_id: &str, clip_id: &str) -> BackendResult<i64>;

   /// `generator.createTitle` -- constructs a title-card producer (color
   /// background + `dynamictext`/`qtext` filter, per 01's `generator.*`
   /// namespace) and adds it to the Playlist bin, ready for
   /// `edit.appendClip({source:{playlistIndex}})` like any other source.
   fn generator_create_title(&mut self, project_id: &str, params: Value) -> BackendResult<PlaylistEntry>;

    /// `generator.createColor` -- constructs a plain `color:` producer
    /// (`{hexColor}`, `#AARRGGBB`) and adds it to the Playlist bin, per
    /// 01's `generator.*` namespace (`ColorProducerWidget::newProducer`).
    /// Commonly used with a fully-transparent `#00000000` to build a
    /// timeline spacer clip: append it, then `edit.trimClipOut` to the
    /// desired length.
    fn generator_create_color(&mut self, project_id: &str, params: Value) -> BackendResult<PlaylistEntry>;

   fn subtitles_add_track(&mut self, project_id: &str) -> BackendResult<SubtitleTrackInfo>;
    fn subtitles_append_item(
        &mut self,
        project_id: &str,
        track_index: usize,
        start_frame: i64,
        end_frame: i64,
        text: &str,
    ) -> BackendResult<()>;

    /// `subtitles.removeItems` -- remove cues by 0-based index (append order).
    fn subtitles_remove_items(
        &mut self,
        project_id: &str,
        track_index: usize,
        item_indices: &[usize],
    ) -> BackendResult<()>;

    /// `subtitles.importSrt` -- import an SRT file into an existing track
    /// (default track 0, creating it if needed) or a new track when
    /// `new_track` is true. Returns the target `trackIndex`.
    fn subtitles_import_srt(
        &mut self,
        project_id: &str,
        path: &str,
        new_track: bool,
    ) -> BackendResult<SubtitleTrackInfo>;

    /// `subtitles.exportSrt` -- write a track's SRT to `path` (relative paths
    /// resolve against the project root). Returns the resolved path.
    fn subtitles_export_srt(
        &mut self,
        project_id: &str,
        path: &str,
        track_index: usize,
    ) -> BackendResult<String>;

    /// `subtitles.burnIn` -- attach (or, if already attached, leave in
    /// place) a real burn-in filter on the timeline output rendering
    /// `track_index`'s cues into every exported/rendered frame. Unlike the
    /// other `subtitles.*` calls, this mutates the *output* rather than
    /// the subtitle track data itself; `subtitles.appendItem`/`removeItems`
    /// alone never make cues visible in rendered frames.
    fn subtitles_burn_in(&mut self, project_id: &str, track_index: usize) -> BackendResult<()>;

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

    /// `jobs.stop` -- cancel a running job (`AbstractJob::stop` equivalent).
    /// Sets status to `"stopped"` when the job was running.
    fn jobs_stop(&mut self, job_id: &str) -> BackendResult<()>;

    /// `playback.getFrame` -- one-off frame render for agent-side visual
    /// verification. Returns base64-encoded image bytes in `format`.
    fn playback_get_frame(
        &mut self,
        project_id: &str,
        frame: i64,
        format: &str,
    ) -> BackendResult<String>;

    // --- markers.* / recent.* (additive; append-only on the trait) ---

    /// `markers.append` -- place a cue/range marker on the timeline.
    fn markers_append(
        &mut self,
        project_id: &str,
        frame: i64,
        text: Option<String>,
        color: Option<String>,
    ) -> BackendResult<Marker>;

    fn markers_remove(&mut self, project_id: &str, marker_index: usize) -> BackendResult<()>;

    fn markers_update(
        &mut self,
        project_id: &str,
        marker_index: usize,
        frame: Option<i64>,
        text: Option<String>,
        color: Option<String>,
    ) -> BackendResult<Marker>;

    /// `markers.move` -- set the marker's frame range. Stores `start`/`end`
    /// when the model supports range markers; always sets `frame = start`.
    fn markers_move(
        &mut self,
        project_id: &str,
        marker_index: usize,
        start: i64,
        end: i64,
    ) -> BackendResult<Marker>;

    fn markers_set_color(
        &mut self,
        project_id: &str,
        marker_index: usize,
        color: &str,
    ) -> BackendResult<Marker>;

    fn markers_clear(&mut self, project_id: &str) -> BackendResult<()>;

    fn markers_list(&mut self, project_id: &str) -> BackendResult<Vec<Marker>>;

    fn markers_get(&mut self, project_id: &str, marker_index: usize) -> BackendResult<Marker>;

    /// `markers.next` -- next marker frame strictly after `from_frame`, or
    /// `None` if none.
    fn markers_next(&mut self, project_id: &str, from_frame: i64) -> BackendResult<Option<i64>>;

    /// `markers.prev` -- previous marker frame strictly before `from_frame`,
    /// or `None` if none.
    fn markers_prev(&mut self, project_id: &str, from_frame: i64) -> BackendResult<Option<i64>>;

    /// `recent.add` -- project-scoped recent path list (dedupe, newest first).
    fn recent_add(&mut self, project_id: &str, path: &str) -> BackendResult<()>;

    /// `recent.remove` -- remove `path` from the recent list; returns the
    /// removed path on success.
    fn recent_remove(&mut self, project_id: &str, path: &str) -> BackendResult<String>;

    fn recent_list(&mut self, project_id: &str) -> BackendResult<Vec<String>>;
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
    /// Per-track cue list (0-based track index → cues in append order).
    subtitle_items: HashMap<usize, Vec<MockSubtitleItem>>,
    next_clip_id: u64,
    next_job_id: u64,
    jobs: HashMap<String, JobStatus>,
    markers: Vec<Marker>,
    /// Newest-first, deduped on add.
    recent: Vec<String>,
    /// Project-wide timeline row height, `shotcut:trackHeight` on export.
    /// `0` means "unset" (build_mlt_xml omits the attribute).
    track_height: i64,
}

#[derive(Debug, Clone)]
struct MockSubtitleItem {
    start_frame: i64,
    end_frame: i64,
    text: String,
}

#[derive(Default, Clone)]
struct MockFilter {
    mlt_service: String,
    properties: HashMap<String, Value>,
    /// property -> (position -> (value, interpolation))
    keyframes: HashMap<String, HashMap<i64, (Value, String)>>,
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
        // Bug fix: previously never wired up -- 10-testing-plan.md's Phase 3
        // `recent.*` row expects `recent.list` to contain a project after
        // `project.select`. Recent-list state lives per-`ProjectData` (see
        // the `recent` field below), so the natural equivalent here is
        // recording the project's own id into its own recent bin on select.
        let _ = self.recent_add(project_id, project_id);
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
        let track = Track {
            index: data.tracks.len(),
            kind: kind.to_string(),
            muted: false,
            hidden: false,
            locked: false,
            blend_mode: default_blend_mode(),
        };
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

    fn edit_reorder_track(&mut self, project_id: &str, from_index: usize, to_index: usize) -> BackendResult<Vec<Track>> {
        let data = self.project_mut(project_id);
        let len = data.tracks.len();
        if from_index >= len {
            return Err(BackendError::NotFound(format!("track {from_index}")));
        }
        if to_index >= len {
            return Err(BackendError::InvalidParams(format!("toIndex {to_index} out of range (len {len})")));
        }
        // Snapshot old-index -> clips before mutating `tracks`, so the
        // remap below can rebuild the HashMap keyed by each track's *new*
        // index while still knowing what used to live at each *old* index.
        let old_clips: HashMap<usize, Vec<Clip>> = data.clips.drain().collect();

        let track = data.tracks.remove(from_index);
        data.tracks.insert(to_index, track);
        for (i, t) in data.tracks.iter_mut().enumerate() {
            t.index = i;
        }

        // Reproduce the same remove+insert permutation on a Vec of old
        // indices to learn each new index's original index, then rebuild
        // `clips` keyed by new index.
        let mut order: Vec<usize> = (0..len).collect();
        let moved = order.remove(from_index);
        order.insert(to_index, moved);
        let mut new_clips = HashMap::new();
        for (new_index, old_index) in order.into_iter().enumerate() {
            if let Some(clips) = old_clips.get(&old_index) {
                new_clips.insert(new_index, clips.clone());
            }
        }
        data.clips = new_clips;
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(data.tracks.clone())
    }

    fn edit_set_track_properties(
        &mut self,
        project_id: &str,
        track_index: usize,
        muted: Option<bool>,
        hidden: Option<bool>,
        locked: Option<bool>,
        blend_mode: Option<String>,
    ) -> BackendResult<Track> {
        let data = self.project_mut(project_id);
        let track = data
            .tracks
            .get_mut(track_index)
            .ok_or_else(|| BackendError::NotFound(format!("track {track_index}")))?;
        if let Some(v) = muted {
            track.muted = v;
        }
        if let Some(v) = hidden {
            track.hidden = v;
        }
        if let Some(v) = locked {
            track.locked = v;
        }
        if let Some(v) = blend_mode {
            track.blend_mode = v;
        }
        let result = track.clone();
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(result)
    }

    fn edit_set_track_height(&mut self, project_id: &str, height: i64) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        data.track_height = height;
        data.dirty = true;
        Ok(())
    }

    fn edit_remove_clip(&mut self, project_id: &str, track_index: usize, clip_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        let clips = data
            .clips
            .get_mut(&track_index)
            .ok_or_else(|| BackendError::NotFound(format!("clip {track_index}/{clip_index}")))?;
        if clip_index >= clips.len() {
            return Err(BackendError::NotFound(format!("clip {track_index}/{clip_index}")));
        }
        clips.remove(clip_index);
        for (i, c) in clips.iter_mut().enumerate() {
            c.index = i;
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn edit_move_clip(
        &mut self,
        project_id: &str,
        from_track_index: usize,
        from_clip_index: usize,
        to_track_index: usize,
        to_clip_index: usize,
    ) -> BackendResult<Clip> {
        let data = self.project_mut(project_id);
        if from_track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {from_track_index}")));
        }
        if to_track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {to_track_index}")));
        }
        let mut clip = {
            let source_clips = data
                .clips
                .get_mut(&from_track_index)
                .ok_or_else(|| BackendError::NotFound(format!("clip {from_track_index}/{from_clip_index}")))?;
            if from_clip_index >= source_clips.len() {
                return Err(BackendError::NotFound(format!("clip {from_track_index}/{from_clip_index}")));
            }
            source_clips.remove(from_clip_index)
        };
        if let Some(source_clips) = data.clips.get_mut(&from_track_index) {
            for (i, c) in source_clips.iter_mut().enumerate() {
                c.index = i;
            }
        }
        let dest_clips = data.clips.entry(to_track_index).or_default();
        if to_clip_index > dest_clips.len() {
            return Err(BackendError::InvalidParams(format!(
                "toClipIndex {to_clip_index} out of range (len {})",
                dest_clips.len()
            )));
        }
        dest_clips.insert(to_clip_index.min(dest_clips.len()), clip.clone());
        for (i, c) in dest_clips.iter_mut().enumerate() {
            c.index = i;
        }
        clip.index = to_clip_index;
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(clip)
    }

    fn edit_insert_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        source: Value,
    ) -> BackendResult<Clip> {
        let data = self.project_mut(project_id);
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        data.next_clip_id += 1;
        let clip_id = format!("clip-{}", data.next_clip_id);
        let clips = data.clips.entry(track_index).or_default();
        if clip_index > clips.len() {
            return Err(BackendError::InvalidParams(format!(
                "clipIndex {clip_index} out of range (len {})",
                clips.len()
            )));
        }
        let clip = Clip { clip_id, index: clip_index, source, in_frame: 0, out_frame: 0 };
        clips.insert(clip_index, clip.clone());
        for (i, c) in clips.iter_mut().enumerate() {
            c.index = i;
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(clip)
    }

    fn edit_overwrite_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        source: Value,
    ) -> BackendResult<Clip> {
        let data = self.project_mut(project_id);
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        data.next_clip_id += 1;
        let clip_id = format!("clip-{}", data.next_clip_id);
        let clips = data.clips.entry(track_index).or_default();
        if clip_index > clips.len() {
            return Err(BackendError::InvalidParams(format!(
                "clipIndex {clip_index} out of range (len {})",
                clips.len()
            )));
        }
        let clip = Clip { clip_id, index: clip_index, source, in_frame: 0, out_frame: 0 };
        if clip_index == clips.len() {
            // No clip occupies this slot yet -- behaves like append.
            clips.push(clip.clone());
        } else {
            // Non-rippling: replace the occupant in place, downstream
            // indices unaffected (unlike edit_insert_clip's splice).
            clips[clip_index] = clip.clone();
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(clip)
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

    fn playlist_insert(
        &mut self,
        project_id: &str,
        index: usize,
        source: Value,
        name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        let data = self.project_mut(project_id);
        if index > data.playlist.len() {
            return Err(BackendError::InvalidParams(format!(
                "playlist.insert index {index} out of range (len {})",
                data.playlist.len()
            )));
        }
        let entry = PlaylistEntry {
            index,
            name: name.unwrap_or_else(|| format!("clip{index}")),
            source,
            duration_frames: 0,
        };
        data.playlist.insert(index, entry);
        for (i, e) in data.playlist.iter_mut().enumerate() {
            e.index = i;
        }
        data.dirty = true;
        Ok(data.playlist[index].clone())
    }

    fn playlist_remove(&mut self, project_id: &str, index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if index >= data.playlist.len() {
            return Err(BackendError::NotFound(format!("playlist index {index}")));
        }
        data.playlist.remove(index);
        for (i, e) in data.playlist.iter_mut().enumerate() {
            e.index = i;
        }
        data.dirty = true;
        Ok(())
    }

    fn playlist_move(&mut self, project_id: &str, from_index: usize, to_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let len = data.playlist.len();
        if from_index >= len {
            return Err(BackendError::NotFound(format!("playlist index {from_index}")));
        }
        if to_index >= len {
            return Err(BackendError::InvalidParams(format!("toIndex {to_index} out of range (len {len})")));
        }
        let entry = data.playlist.remove(from_index);
        data.playlist.insert(to_index, entry);
        for (i, e) in data.playlist.iter_mut().enumerate() {
            e.index = i;
        }
        data.dirty = true;
        Ok(())
    }

    fn playlist_get(&mut self, project_id: &str, index: usize) -> BackendResult<PlaylistEntryDetail> {
        let data = self.project_mut(project_id);
        let entry = data
            .playlist
            .get(index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("playlist index {index}")))?;
        // MockBackend has no real probe capability (see `file_probe` above),
        // so `probe` is honestly `None` here rather than fabricated data.
        Ok(PlaylistEntryDetail {
            index: entry.index,
            name: entry.name,
            source: entry.source,
            duration_frames: entry.duration_frames,
            probe: None,
        })
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
        _ripple: bool,
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
        _ripple: bool,
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

    fn edit_split_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        position: i64,
    ) -> BackendResult<SplitClipResult> {
        let data = self.project_mut(project_id);
        let (left_clip_id, source, out_frame) = {
            let clips = data
                .clips
                .get(&track_index)
                .ok_or_else(|| BackendError::NotFound(format!("track {track_index}")))?;
            if clip_index >= clips.len() {
                return Err(BackendError::NotFound(format!("clip {track_index}/{clip_index}")));
            }
            let left = &clips[clip_index];
            // Both halves must be non-empty: left [in, position-1], right [position, out].
            // Equivalent to position strictly after in_frame and not past out_frame.
            if position <= left.in_frame || position > left.out_frame {
                return Err(BackendError::InvalidParams(format!(
                    "position {position} must be strictly between inFrame {} and outFrame {} (inclusive of outFrame)",
                    left.in_frame, left.out_frame
                )));
            }
            (left.clip_id.clone(), left.source.clone(), left.out_frame)
        };

        data.next_clip_id += 1;
        let right_clip_id = format!("clip-{}", data.next_clip_id);

        // Clone filters from left onto right (Shotcut copies producer + filters).
        if let Some(filters) = data.filters.get(&left_clip_id).cloned() {
            data.filters.insert(right_clip_id.clone(), filters);
        }

        let clips = data.clips.get_mut(&track_index).expect("track clips");
        clips[clip_index].out_frame = position - 1;
        clips.insert(
            clip_index + 1,
            Clip {
                clip_id: right_clip_id.clone(),
                index: clip_index + 1,
                source,
                in_frame: position,
                out_frame,
            },
        );
        for (i, c) in clips.iter_mut().enumerate() {
            c.index = i;
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(SplitClipResult {
            left_clip_id,
            right_clip_id,
            left_index: clip_index,
            right_index: clip_index + 1,
        })
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
        // clip_id must address a real timeline clip so agents cannot attach
        // filters to ghosts; scan all tracks.
        let known = data
            .clips
            .values()
            .flat_map(|c| c.iter())
            .any(|c| c.clip_id == clip_id);
        if !known {
            return Err(BackendError::NotFound(format!("clip {clip_id}")));
        }
        let filters = data.filters.entry(clip_id.to_string()).or_default();
        let mut filter = MockFilter {
            mlt_service: mlt_service.to_string(),
            ..MockFilter::default()
        };
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
                filter
                    .keyframes
                    .entry(property.to_string())
                    .or_default()
                    .insert(position, (value, "linear".to_string()));
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
        clip_id: &str,
        filter_index: usize,
        property: &str,
        position: i64,
        value: Value,
        interpolation: &str,
    ) -> BackendResult<()> {
        let interp = normalize_interpolation(interpolation);
        let data = self.project_mut(project_id);
        let filter = data
            .filters
            .get_mut(clip_id)
            .and_then(|filters| filters.get_mut(filter_index))
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        filter
            .keyframes
            .entry(property.to_string())
            .or_default()
            .insert(position, (value, interp));
        data.dirty = true;
        Ok(())
    }

    fn filter_list(
        &mut self,
        project_id: &str,
        clip_id: &str,
    ) -> BackendResult<Vec<FilterListEntry>> {
        let data = self.project_mut(project_id);
        let known = data
            .clips
            .values()
            .flat_map(|c| c.iter())
            .any(|c| c.clip_id == clip_id);
        if !known {
            return Err(BackendError::NotFound(format!("clip {clip_id}")));
        }
        let filters = data.filters.get(clip_id).map(|f| f.as_slice()).unwrap_or(&[]);
        Ok(filters
            .iter()
            .enumerate()
            .map(|(index, f)| FilterListEntry {
                index,
                mlt_service: f.mlt_service.clone(),
                properties: Value::Object(f.properties.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            })
            .collect())
    }

    fn filter_remove(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let filters = data
            .filters
            .get_mut(clip_id)
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        if filter_index >= filters.len() {
            return Err(BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")));
        }
        filters.remove(filter_index);
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn filter_reorder(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        new_index: usize,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let filters = data
            .filters
            .get_mut(clip_id)
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        if filter_index >= filters.len() {
            return Err(BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")));
        }
        if new_index >= filters.len() {
            return Err(BackendError::InvalidParams(format!(
                "newIndex {new_index} out of range (len={})",
                filters.len()
            )));
        }
        if filter_index != new_index {
            let item = filters.remove(filter_index);
            filters.insert(new_index, item);
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn filter_list_keyframes(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
    ) -> BackendResult<Vec<KeyframeInfo>> {
        let data = self.project_mut(project_id);
        let filter = data
            .filters
            .get(clip_id)
            .and_then(|filters| filters.get(filter_index))
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        let mut list = filter
            .keyframes
            .get(property)
            .map(|map| {
                map.iter()
                    .map(|(position, (value, interpolation))| KeyframeInfo {
                        position: *position,
                        value: value.clone(),
                        interpolation: interpolation.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        list.sort_by_key(|k| k.position);
        Ok(list)
    }

    fn filter_remove_keyframe(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        position: i64,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        let filter = data
            .filters
            .get_mut(clip_id)
            .and_then(|filters| filters.get_mut(filter_index))
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        let removed = filter
            .keyframes
            .get_mut(property)
            .and_then(|map| map.remove(&position))
            .is_some();
        if !removed {
            return Err(BackendError::NotFound(format!(
                "keyframe at {position} on property {property} of filter {filter_index}"
            )));
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }


    fn clip_length_frames(&mut self, project_id: &str, clip_id: &str) -> BackendResult<i64> {
        let data = self.project_mut(project_id);
        for clips in data.clips.values() {
            if let Some(clip) = clips.iter().find(|c| c.clip_id == clip_id) {
                return Ok((clip.out_frame - clip.in_frame + 1).max(0));
            }
        }
        Err(BackendError::NotFound(format!("clip {clip_id}")))
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

    fn generator_create_color(&mut self, project_id: &str, params: Value) -> BackendResult<PlaylistEntry> {
        let data = self.project_mut(project_id);
        let entry = PlaylistEntry {
            index: data.playlist.len(),
            name: "color".to_string(),
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
        data.subtitle_items.entry(track_index).or_default();
        data.dirty = true;
        Ok(SubtitleTrackInfo { track_index })
    }

    fn subtitles_append_item(
        &mut self,
        project_id: &str,
        track_index: usize,
        start_frame: i64,
        end_frame: i64,
        text: &str,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if track_index >= data.subtitle_tracks {
            return Err(BackendError::NotFound(format!("subtitle track {track_index}")));
        }
        data.subtitle_items
            .entry(track_index)
            .or_default()
            .push(MockSubtitleItem {
                start_frame,
                end_frame,
                text: text.to_string(),
            });
        data.dirty = true;
        Ok(())
    }

    fn subtitles_remove_items(
        &mut self,
        project_id: &str,
        track_index: usize,
        item_indices: &[usize],
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if track_index >= data.subtitle_tracks {
            return Err(BackendError::NotFound(format!("subtitle track {track_index}")));
        }
        let items = data.subtitle_items.entry(track_index).or_default();
        let mut remove: Vec<usize> = item_indices.to_vec();
        remove.sort_unstable();
        remove.dedup();
        for &idx in remove.iter().rev() {
            if idx >= items.len() {
                return Err(BackendError::InvalidParams(format!(
                    "subtitle item index {idx} out of range (len {})",
                    items.len()
                )));
            }
            items.remove(idx);
        }
        data.dirty = true;
        Ok(())
    }

    fn subtitles_import_srt(
        &mut self,
        project_id: &str,
        path: &str,
        new_track: bool,
    ) -> BackendResult<SubtitleTrackInfo> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            BackendError::InvalidParams(format!("subtitles.importSrt path {path} is not readable: {e}"))
        })?;
        let cues = parse_mock_srt_cues(&content);
        let track_index = if new_track || self.project_mut(project_id).subtitle_tracks == 0 {
            self.subtitles_add_track(project_id)?.track_index
        } else {
            0
        };
        let data = self.project_mut(project_id);
        data.subtitle_items.insert(track_index, cues);
        data.dirty = true;
        Ok(SubtitleTrackInfo { track_index })
    }

    fn subtitles_export_srt(
        &mut self,
        project_id: &str,
        path: &str,
        track_index: usize,
    ) -> BackendResult<String> {
        let data = self.project_mut(project_id);
        if track_index >= data.subtitle_tracks {
            return Err(BackendError::NotFound(format!("subtitle track {track_index}")));
        }
        let items = data.subtitle_items.get(&track_index).cloned().unwrap_or_default();
        let srt = format_mock_srt(&items);
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    BackendError::InvalidParams(format!("failed to create export parent: {e}"))
                })?;
            }
        }
        std::fs::write(path, srt).map_err(|e| {
            BackendError::InvalidParams(format!("failed to write SRT to {path}: {e}"))
        })?;
        data.dirty = true;
        Ok(path.to_string())
    }

    fn subtitles_burn_in(&mut self, project_id: &str, track_index: usize) -> BackendResult<()> {
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
            JobStatus {
                job_id: job_id.clone(),
                status: "running".into(),
                percent: 0.0,
                result_path: None,
                error: None,
            },
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

    fn jobs_stop(&mut self, job_id: &str) -> BackendResult<()> {
        for data in self.projects.values_mut() {
            if let Some(job) = data.jobs.get_mut(job_id) {
                job.status = "stopped".into();
                job.error = Some("stopped by client".into());
                return Ok(());
            }
        }
        Err(BackendError::NotFound(format!("job {job_id}")))
    }

    fn playback_get_frame(
        &mut self,
        _project_id: &str,
        _frame: i64,
        _format: &str,
    ) -> BackendResult<String> {
        Err(BackendError::NotFound("playback.getFrame not implemented in MockBackend".into()))
    }

    fn markers_append(
        &mut self,
        project_id: &str,
        frame: i64,
        text: Option<String>,
        color: Option<String>,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id);
        let marker = Marker {
            index: data.markers.len(),
            frame,
            text: text.unwrap_or_default(),
            color: color.unwrap_or_else(|| "#000000".to_string()),
            end_frame: None,
        };
        data.markers.push(marker.clone());
        data.dirty = true;
        Ok(marker)
    }

    fn markers_remove(&mut self, project_id: &str, marker_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        if marker_index >= data.markers.len() {
            return Err(BackendError::NotFound(format!("marker {marker_index}")));
        }
        data.markers.remove(marker_index);
        reindex_markers(&mut data.markers);
        data.dirty = true;
        Ok(())
    }

    fn markers_update(
        &mut self,
        project_id: &str,
        marker_index: usize,
        frame: Option<i64>,
        text: Option<String>,
        color: Option<String>,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id);
        {
            let marker = data
                .markers
                .get_mut(marker_index)
                .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))?;
            if let Some(frame) = frame {
                marker.frame = frame;
            }
            if let Some(text) = text {
                marker.text = text;
            }
            if let Some(color) = color {
                marker.color = color;
            }
        }
        data.dirty = true;
        Ok(data.markers[marker_index].clone())
    }

    fn markers_move(
        &mut self,
        project_id: &str,
        marker_index: usize,
        start: i64,
        end: i64,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id);
        {
            let marker = data
                .markers
                .get_mut(marker_index)
                .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))?;
            marker.frame = start;
            marker.end_frame = if end != start { Some(end) } else { None };
        }
        data.dirty = true;
        Ok(data.markers[marker_index].clone())
    }

    fn markers_set_color(
        &mut self,
        project_id: &str,
        marker_index: usize,
        color: &str,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id);
        {
            let marker = data
                .markers
                .get_mut(marker_index)
                .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))?;
            marker.color = color.to_string();
        }
        data.dirty = true;
        Ok(data.markers[marker_index].clone())
    }

    fn markers_clear(&mut self, project_id: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        data.markers.clear();
        data.dirty = true;
        Ok(())
    }

    fn markers_list(&mut self, project_id: &str) -> BackendResult<Vec<Marker>> {
        Ok(self.project_mut(project_id).markers.clone())
    }

    fn markers_get(&mut self, project_id: &str, marker_index: usize) -> BackendResult<Marker> {
        self.project_mut(project_id)
            .markers
            .get(marker_index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))
    }

    fn markers_next(&mut self, project_id: &str, from_frame: i64) -> BackendResult<Option<i64>> {
        let mut frames: Vec<i64> = self
            .project_mut(project_id)
            .markers
            .iter()
            .map(|m| m.frame)
            .filter(|f| *f > from_frame)
            .collect();
        frames.sort_unstable();
        Ok(frames.into_iter().next())
    }

    fn markers_prev(&mut self, project_id: &str, from_frame: i64) -> BackendResult<Option<i64>> {
        let mut frames: Vec<i64> = self
            .project_mut(project_id)
            .markers
            .iter()
            .map(|m| m.frame)
            .filter(|f| *f < from_frame)
            .collect();
        frames.sort_unstable();
        Ok(frames.into_iter().next_back())
    }

    fn recent_add(&mut self, project_id: &str, path: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id);
        data.recent.retain(|p| p != path);
        data.recent.insert(0, path.to_string());
        Ok(())
    }

    fn recent_remove(&mut self, project_id: &str, path: &str) -> BackendResult<String> {
        let data = self.project_mut(project_id);
        let before = data.recent.len();
        data.recent.retain(|p| p != path);
        if data.recent.len() == before {
            return Err(BackendError::NotFound(format!("recent path {path}")));
        }
        Ok(path.to_string())
    }

    fn recent_list(&mut self, project_id: &str) -> BackendResult<Vec<String>> {
        Ok(self.project_mut(project_id).recent.clone())
    }
}

fn reindex_markers(markers: &mut [Marker]) {
    for (i, m) in markers.iter_mut().enumerate() {
        m.index = i;
    }
}

/// Minimal SRT parser for MockBackend import (cue number + timing + text blocks).
fn parse_mock_srt_cues(content: &str) -> Vec<MockSubtitleItem> {
    let mut items = Vec::new();
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let timing = if line.contains("-->") {
            line.to_string()
        } else {
            match lines.next() {
                Some(next) => next.trim_end_matches('\r').to_string(),
                None => break,
            }
        };
        let Some((start_ts, end_ts)) = timing.split_once("-->") else {
            continue;
        };
        let start_frame = mock_srt_timestamp_to_frames(start_ts.trim());
        let end_frame = mock_srt_timestamp_to_frames(end_ts.trim());
        let mut text_lines = Vec::new();
        while let Some(peek) = lines.peek() {
            let t = peek.trim_end_matches('\r');
            if t.trim().is_empty() {
                lines.next();
                break;
            }
            text_lines.push(t.to_string());
            lines.next();
        }
        items.push(MockSubtitleItem {
            start_frame,
            end_frame,
            text: text_lines.join("\n"),
        });
    }
    items
}

fn format_mock_srt(items: &[MockSubtitleItem]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            mock_frames_to_srt_timestamp(item.start_frame),
            mock_frames_to_srt_timestamp(item.end_frame),
            item.text
        ));
    }
    out
}

fn mock_frames_to_srt_timestamp(frame: i64) -> String {
    // Mock uses the same 30fps convention as MltBackend for round-trips in unit tests.
    let fps = 30i64;
    let total_ms = (frame.max(0) as f64 / fps as f64 * 1000.0).round() as i64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

fn mock_srt_timestamp_to_frames(ts: &str) -> i64 {
    // HH:MM:SS,mmm or HH:MM:SS.mmm
    let ts = ts.replace('.', ",");
    let parts: Vec<&str> = ts.split(&[',', ':'][..]).collect();
    if parts.len() < 4 {
        return 0;
    }
    let h: i64 = parts[0].parse().unwrap_or(0);
    let m: i64 = parts[1].parse().unwrap_or(0);
    let s: i64 = parts[2].parse().unwrap_or(0);
    let ms: i64 = parts[3].parse().unwrap_or(0);
    let total_ms = ((h * 3600 + m * 60 + s) * 1000) + ms;
    ((total_ms as f64 / 1000.0) * 30.0).round() as i64
}

/// Map wire/API interpolation names onto the three SAP values.
fn normalize_interpolation(interpolation: &str) -> String {
    match interpolation {
        "smooth" => "smooth".to_string(),
        "discrete" | "hold" => "discrete".to_string(),
        _ => "linear".to_string(),
    }
}

/// Parse one MLT animated-property keyframe token (`"10=value"`, `"10~=value"`,
/// `"10|=value"`) into position / value / interpolation. Used by MltBackend's
/// `filter.listKeyframes` and unit-tested here so the mapping stays shared.
pub fn parse_mlt_keyframe_entry(entry: &str) -> Option<KeyframeInfo> {
    let eq = entry.find('=')?;
    let lhs = &entry[..eq];
    let rhs = &entry[eq + 1..];
    let (position_str, interpolation) = if let Some(pos) = lhs.strip_suffix('~') {
        (pos, "smooth")
    } else if let Some(pos) = lhs.strip_suffix('|') {
        (pos, "discrete")
    } else {
        (lhs, "linear")
    };
    let position: i64 = position_str.parse().ok()?;
    let value = if let Ok(n) = rhs.parse::<i64>() {
        json!(n)
    } else if let Ok(n) = rhs.parse::<f64>() {
        json!(n)
    } else {
        json!(rhs)
    };
    Some(KeyframeInfo {
        position,
        value,
        interpolation: interpolation.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_reorder_track_remaps_clips_to_follow_their_track() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        b.edit_add_track("p", "video").unwrap();
        b.edit_add_track("p", "video").unwrap();
        let clip0 = b.edit_append_clip("p", 0, json!({"path": "/tmp/track0.mp4"})).unwrap();
        let clip1 = b.edit_append_clip("p", 1, json!({"path": "/tmp/track1.mp4"})).unwrap();
        let clip2 = b.edit_append_clip("p", 2, json!({"path": "/tmp/track2.mp4"})).unwrap();

        // Move track 0 to index 2: new order is [track1, track2, track0].
        let tracks = b.edit_reorder_track("p", 0, 2).unwrap();
        assert_eq!(tracks.iter().map(|t| t.index).collect::<Vec<_>>(), vec![0, 1, 2]);

        assert_eq!(b.edit_list_clips("p", 0).unwrap()[0].clip_id, clip1.clip_id);
        assert_eq!(b.edit_list_clips("p", 1).unwrap()[0].clip_id, clip2.clip_id);
        assert_eq!(b.edit_list_clips("p", 2).unwrap()[0].clip_id, clip0.clip_id);
    }

    #[test]
    fn edit_set_track_properties_is_a_partial_update() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();

        let t = b.edit_set_track_properties("p", 0, Some(true), None, None, None).unwrap();
        assert!(t.muted);
        assert!(!t.hidden);
        assert!(!t.locked);
        assert_eq!(t.blend_mode, "0");

        let t = b.edit_set_track_properties("p", 0, None, None, None, Some("13".into())).unwrap();
        // muted from the previous call must survive this unrelated update.
        assert!(t.muted);
        assert_eq!(t.blend_mode, "13");

        assert!(b.edit_set_track_properties("p", 5, Some(true), None, None, None).is_err());
    }

    #[test]
    fn edit_set_track_height_round_trips_via_state() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        assert!(b.edit_set_track_height("p", 120).is_ok());
    }

    #[test]
    fn edit_remove_clip_reindexes_remaining_clips() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        b.edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"})).unwrap();
        let keep = b.edit_append_clip("p", 0, json!({"path": "/tmp/b.mp4"})).unwrap();
        b.edit_append_clip("p", 0, json!({"path": "/tmp/c.mp4"})).unwrap();

        b.edit_remove_clip("p", 0, 0).unwrap();
        let clips = b.edit_list_clips("p", 0).unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].clip_id, keep.clip_id);
        assert_eq!(clips[0].index, 0);
        assert_eq!(clips[1].index, 1);
        assert!(b.edit_remove_clip("p", 0, 99).is_err());
    }

    #[test]
    fn edit_move_clip_same_track_and_cross_track() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        b.edit_add_track("p", "video").unwrap();
        let a = b.edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"})).unwrap();
        let bee = b.edit_append_clip("p", 0, json!({"path": "/tmp/b.mp4"})).unwrap();

        // Same-track reorder: move clip 0 (a) to the end of track 0.
        let moved = b.edit_move_clip("p", 0, 0, 0, 1).unwrap();
        assert_eq!(moved.clip_id, a.clip_id);
        let clips = b.edit_list_clips("p", 0).unwrap();
        assert_eq!(clips[0].clip_id, bee.clip_id);
        assert_eq!(clips[1].clip_id, a.clip_id);

        // Cross-track move: move `a` (now at track 0 index 1) onto track 1.
        let moved = b.edit_move_clip("p", 0, 1, 1, 0).unwrap();
        assert_eq!(moved.clip_id, a.clip_id);
        assert_eq!(b.edit_list_clips("p", 0).unwrap().len(), 1);
        assert_eq!(b.edit_list_clips("p", 1).unwrap()[0].clip_id, a.clip_id);

        assert!(b.edit_move_clip("p", 0, 99, 1, 0).is_err());
        assert!(b.edit_move_clip("p", 9, 0, 1, 0).is_err());
    }

    #[test]
    fn edit_insert_clip_splices_mid_track_and_ripples_downstream_indices() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        let a = b.edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"})).unwrap();
        let c = b.edit_append_clip("p", 0, json!({"path": "/tmp/c.mp4"})).unwrap();

        // Splice `b` between `a` and `c` in one call, distinct from
        // append+move: the caller never places `b` at the end first.
        let inserted = b.edit_insert_clip("p", 0, 1, json!({"path": "/tmp/b.mp4"})).unwrap();
        assert_eq!(inserted.index, 1);

        let clips = b.edit_list_clips("p", 0).unwrap();
        assert_eq!(clips.len(), 3);
        assert_eq!(clips[0].clip_id, a.clip_id);
        assert_eq!(clips[1].clip_id, inserted.clip_id);
        assert_eq!(clips[2].clip_id, c.clip_id);
        assert_eq!(clips[2].index, 2); // c rippled from index 1 to 2.

        // clipIndex == current clip count is append-equivalent.
        let appended_via_insert = b.edit_insert_clip("p", 0, 3, json!({"path": "/tmp/d.mp4"})).unwrap();
        assert_eq!(appended_via_insert.index, 3);

        assert!(b.edit_insert_clip("p", 0, 99, json!({"path": "/tmp/e.mp4"})).is_err());
        assert!(b.edit_insert_clip("p", 9, 0, json!({"path": "/tmp/e.mp4"})).is_err());
    }

    #[test]
    fn edit_overwrite_clip_replaces_in_place_without_rippling_downstream() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        let a = b.edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"})).unwrap();
        let bee = b.edit_append_clip("p", 0, json!({"path": "/tmp/b.mp4"})).unwrap();
        let c = b.edit_append_clip("p", 0, json!({"path": "/tmp/c.mp4"})).unwrap();

        // Overwrite slot 1 (b) with a new clip -- unlike insertClip, this
        // must NOT shift c's index.
        let overwritten = b.edit_overwrite_clip("p", 0, 1, json!({"path": "/tmp/x.mp4"})).unwrap();
        assert_eq!(overwritten.index, 1);
        assert_ne!(overwritten.clip_id, bee.clip_id);

        let clips = b.edit_list_clips("p", 0).unwrap();
        assert_eq!(clips.len(), 3); // count unchanged -- replace, not splice.
        assert_eq!(clips[0].clip_id, a.clip_id);
        assert_eq!(clips[1].clip_id, overwritten.clip_id);
        assert_eq!(clips[2].clip_id, c.clip_id);
        assert_eq!(clips[2].index, 2); // c did NOT ripple, unlike insertClip.

        // clipIndex == current clip count is append-equivalent.
        let appended_via_overwrite =
            b.edit_overwrite_clip("p", 0, 3, json!({"path": "/tmp/d.mp4"})).unwrap();
        assert_eq!(appended_via_overwrite.index, 3);
        assert_eq!(b.edit_list_clips("p", 0).unwrap().len(), 4);

        assert!(b.edit_overwrite_clip("p", 0, 99, json!({"path": "/tmp/e.mp4"})).is_err());
        assert!(b.edit_overwrite_clip("p", 9, 0, json!({"path": "/tmp/e.mp4"})).is_err());
    }

    #[test]
    fn edit_split_clip_splits_and_reindexes() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        let clip = b
            .edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"}))
            .unwrap();
        b.edit_trim_clip_in("p", 0, 0, 10, false).unwrap();
        b.edit_trim_clip_out("p", 0, 0, 100, false).unwrap();

        let result = b.edit_split_clip("p", 0, 0, 50).unwrap();
        assert_eq!(result.left_clip_id, clip.clip_id);
        assert_eq!(result.left_index, 0);
        assert_eq!(result.right_index, 1);
        assert_ne!(result.right_clip_id, result.left_clip_id);

        let clips = b.edit_list_clips("p", 0).unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].clip_id, result.left_clip_id);
        assert_eq!(clips[0].in_frame, 10);
        assert_eq!(clips[0].out_frame, 49);
        assert_eq!(clips[0].index, 0);
        assert_eq!(clips[1].clip_id, result.right_clip_id);
        assert_eq!(clips[1].in_frame, 50);
        assert_eq!(clips[1].out_frame, 100);
        assert_eq!(clips[1].index, 1);
        assert_eq!(clips[1].source, json!({"path": "/tmp/a.mp4"}));
    }

    #[test]
    fn edit_split_clip_rejects_boundary_position() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        b.edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"})).unwrap();
        b.edit_trim_clip_in("p", 0, 0, 10, false).unwrap();
        b.edit_trim_clip_out("p", 0, 0, 100, false).unwrap();
        assert!(b.edit_split_clip("p", 0, 0, 10).is_err());
        assert!(b.edit_split_clip("p", 0, 0, 101).is_err());
    }

    #[test]
    fn filter_lifecycle_list_remove_reorder_keyframes() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.edit_add_track("p", "video").unwrap();
        let clip = b
            .edit_append_clip("p", 0, json!({"path": "/tmp/a.mp4"}))
            .unwrap();

        let f0 = b
            .filter_add("p", &clip.clip_id, "qtcrop", json!({"rect": "0 0 100 100"}))
            .unwrap();
        let f1 = b
            .filter_add("p", &clip.clip_id, "brightness", json!({"level": 0.5}))
            .unwrap();
        assert_eq!(f0.filter_index, 0);
        assert_eq!(f1.filter_index, 1);

        let listed = b.filter_list("p", &clip.clip_id).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].mlt_service, "qtcrop");
        assert_eq!(listed[1].mlt_service, "brightness");
        assert_eq!(listed[1].properties["level"], json!(0.5));

        b.filter_reorder("p", &clip.clip_id, 0, 1).unwrap();
        let listed = b.filter_list("p", &clip.clip_id).unwrap();
        assert_eq!(listed[0].mlt_service, "brightness");
        assert_eq!(listed[1].mlt_service, "qtcrop");

        // brightness is now index 0
        b.filter_add_keyframe("p", &clip.clip_id, 0, "level", 10, json!(0.2), "linear")
            .unwrap();
        b.filter_add_keyframe("p", &clip.clip_id, 0, "level", 20, json!(0.8), "smooth")
            .unwrap();
        b.filter_add_keyframe("p", &clip.clip_id, 0, "level", 30, json!(1.0), "discrete")
            .unwrap();

        let kfs = b.filter_list_keyframes("p", &clip.clip_id, 0, "level").unwrap();
        assert_eq!(kfs.len(), 3);
        assert_eq!(kfs[0].position, 10);
        assert_eq!(kfs[0].interpolation, "linear");
        assert_eq!(kfs[1].position, 20);
        assert_eq!(kfs[1].interpolation, "smooth");
        assert_eq!(kfs[2].position, 30);
        assert_eq!(kfs[2].interpolation, "discrete");

        b.filter_remove_keyframe("p", &clip.clip_id, 0, "level", 20).unwrap();
        let kfs = b.filter_list_keyframes("p", &clip.clip_id, 0, "level").unwrap();
        assert_eq!(kfs.len(), 2);
        assert_eq!(kfs[0].position, 10);
        assert_eq!(kfs[1].position, 30);

        b.filter_remove("p", &clip.clip_id, 0).unwrap();
        let listed = b.filter_list("p", &clip.clip_id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].mlt_service, "qtcrop");
        assert_eq!(listed[0].index, 0);
    }

    #[test]
    fn markers_append_list_remove_next_prev() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();

        let m0 = b
            .markers_append("p", 100, Some("first".into()), Some("#ff0000".into()))
            .unwrap();
        assert_eq!(m0.index, 0);
        assert_eq!(m0.frame, 100);
        assert_eq!(m0.text, "first");
        assert_eq!(m0.color, "#ff0000");

        let m1 = b.markers_append("p", 50, None, None).unwrap();
        assert_eq!(m1.index, 1);
        assert_eq!(m1.frame, 50);
        assert_eq!(m1.color, "#000000");

        let m2 = b.markers_append("p", 200, Some("third".into()), None).unwrap();
        assert_eq!(m2.index, 2);

        let listed = b.markers_list("p").unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].frame, 100);
        assert_eq!(listed[1].frame, 50);
        assert_eq!(listed[2].frame, 200);

        assert_eq!(b.markers_next("p", 50).unwrap(), Some(100));
        assert_eq!(b.markers_next("p", 100).unwrap(), Some(200));
        assert_eq!(b.markers_next("p", 200).unwrap(), None);
        assert_eq!(b.markers_prev("p", 200).unwrap(), Some(100));
        assert_eq!(b.markers_prev("p", 100).unwrap(), Some(50));
        assert_eq!(b.markers_prev("p", 50).unwrap(), None);

        b.markers_remove("p", 0).unwrap();
        let listed = b.markers_list("p").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].index, 0);
        assert_eq!(listed[0].frame, 50);
        assert_eq!(listed[1].index, 1);
        assert_eq!(listed[1].frame, 200);

        let got = b.markers_get("p", 1).unwrap();
        assert_eq!(got.frame, 200);
        assert!(b.markers_get("p", 9).is_err());
    }

    #[test]
    fn markers_update_move_set_color_clear() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.markers_append("p", 10, Some("a".into()), Some("#111111".into()))
            .unwrap();

        let updated = b
            .markers_update("p", 0, Some(20), Some("b".into()), None)
            .unwrap();
        assert_eq!(updated.frame, 20);
        assert_eq!(updated.text, "b");
        assert_eq!(updated.color, "#111111");

        let moved = b.markers_move("p", 0, 30, 40).unwrap();
        assert_eq!(moved.frame, 30);
        assert_eq!(moved.end_frame, Some(40));

        let colored = b.markers_set_color("p", 0, "#abcdef").unwrap();
        assert_eq!(colored.color, "#abcdef");

        b.markers_clear("p").unwrap();
        assert!(b.markers_list("p").unwrap().is_empty());
    }

    #[test]
    fn mock_subtitles_remove_and_jobs_stop() {
        let mut b = MockBackend::new();
        b.project_select("p").unwrap();
        b.subtitles_add_track("p").unwrap();
        b.subtitles_append_item("p", 0, 0, 30, "a").unwrap();
        b.subtitles_append_item("p", 0, 30, 60, "b").unwrap();
        b.subtitles_append_item("p", 0, 60, 90, "c").unwrap();
        b.subtitles_remove_items("p", 0, &[1]).unwrap();
        let items = &b.projects.get("p").unwrap().subtitle_items[&0];
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "a");
        assert_eq!(items[1].text, "c");

        let job_id = b.file_export("p", "out.mp4", "h264", "mp4").unwrap();
        assert_eq!(b.jobs_get(&job_id).unwrap().status, "running");
        b.jobs_stop(&job_id).unwrap();
        let stopped = b.jobs_get(&job_id).unwrap();
        assert_eq!(stopped.status, "stopped");
        assert_eq!(stopped.error.as_deref(), Some("stopped by client"));
    }

    #[test]
    fn recent_add_list_remove_dedupes_newest_first() {
        let mut b = MockBackend::new();
        // Deliberately does *not* call project_select first (unlike other
        // tests here) -- project_select itself now also pushes onto the
        // recent list (see project_select_adds_the_project_to_its_own_recent_list
        // below), which would otherwise pollute these exact-list assertions.
        // recent_add/list/remove all lazily create the project entry on
        // their own, same as every other per-project method here.

        b.recent_add("p", "/a.mp4").unwrap();
        b.recent_add("p", "/b.mp4").unwrap();
        b.recent_add("p", "/a.mp4").unwrap(); // move to front
        assert_eq!(b.recent_list("p").unwrap(), vec!["/a.mp4".to_string(), "/b.mp4".to_string()]);

        let removed = b.recent_remove("p", "/b.mp4").unwrap();
        assert_eq!(removed, "/b.mp4");
        assert_eq!(b.recent_list("p").unwrap(), vec!["/a.mp4".to_string()]);
        assert!(b.recent_remove("p", "/missing.mp4").is_err());
    }

    /// Proof for testing-plan.md Phase 3's `recent.*` row: "After a
    /// project.select, confirm the project appears in recent.list" -- this
    /// was previously never wired up (project_select didn't touch `recent`
    /// at all); see the bug fix in `project_select` above.
    #[test]
    fn project_select_adds_the_project_to_its_own_recent_list() {
        let mut b = MockBackend::new();
        assert!(b.recent_list("proj").unwrap().is_empty());

        b.project_select("proj").unwrap();
        assert_eq!(b.recent_list("proj").unwrap(), vec!["proj".to_string()]);

        // Re-selecting the same project must dedupe (move-to-front), not
        // grow the list -- same dedupe contract `recent_add` already has.
        b.project_select("proj").unwrap();
        assert_eq!(b.recent_list("proj").unwrap(), vec!["proj".to_string()]);
    }

    #[test]
    fn playlist_insert_remove_move_get_round_trip() {
        let mut b = MockBackend::new();
        b.playlist_append("p", json!({"path": "/a.mp4"}), Some("a".into())).unwrap();
        b.playlist_append("p", json!({"path": "/c.mp4"}), Some("c".into())).unwrap();

        // Insert "b" between "a" and "c".
        let inserted = b.playlist_insert("p", 1, json!({"path": "/b.mp4"}), Some("b".into())).unwrap();
        assert_eq!(inserted.index, 1);
        assert_eq!(inserted.name, "b");
        let names: Vec<String> = b.playlist_list("p").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);

        // playlist.get returns full entry metadata (MockBackend: probe is
        // honestly None, no fabricated data).
        let got = b.playlist_get("p", 1).unwrap();
        assert_eq!(got.name, "b");
        assert_eq!(got.source, json!({"path": "/b.mp4"}));
        assert!(got.probe.is_none());
        assert!(b.playlist_get("p", 99).is_err());

        // Move "c" (index 2) to the front.
        b.playlist_move("p", 2, 0).unwrap();
        let names: Vec<String> = b.playlist_list("p").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["c", "a", "b"]);

        // Remove "a" (now index 1); reindexing must be reflected.
        b.playlist_remove("p", 1).unwrap();
        let remaining = b.playlist_list("p").unwrap();
        let names: Vec<String> = remaining.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["c", "b"]);
        assert_eq!(remaining[0].index, 0);
        assert_eq!(remaining[1].index, 1);

        assert!(b.playlist_remove("p", 99).is_err());
        assert!(b.playlist_move("p", 0, 99).is_err());
        assert!(b.playlist_insert("p", 99, json!({"path": "/z.mp4"}), None).is_err());
    }

    #[test]
    fn parse_mlt_keyframe_entry_tags() {
        let linear = parse_mlt_keyframe_entry("10=0.5").unwrap();
        assert_eq!(linear.position, 10);
        assert_eq!(linear.interpolation, "linear");
        assert_eq!(linear.value, json!(0.5));

        let smooth = parse_mlt_keyframe_entry("20~=1").unwrap();
        assert_eq!(smooth.position, 20);
        assert_eq!(smooth.interpolation, "smooth");
        assert_eq!(smooth.value, json!(1));

        let discrete = parse_mlt_keyframe_entry("30|=hold-me").unwrap();
        assert_eq!(discrete.position, 30);
        assert_eq!(discrete.interpolation, "discrete");
        assert_eq!(discrete.value, json!("hold-me"));
    }
}
