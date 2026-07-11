//! `MltBackend`: a third, independent `Backend` implementor that needs
//! neither a running Shotcut process nor the `real_ffi` feature -- it
//! maintains its own in-memory project model (tracks/clips/playlist bin/
//! filters/subtitles) and can serialize that model into real MLT XML
//! (producers, per-track playlists, a combining tractor, per doc
//! 09-project-folder-layout.md's `<projectRoot>/project.mlt` convention),
//! then shell out to the real `melt` CLI to actually render video.
//!
//! **What's real**: the generated MLT XML is valid input to `melt` (see
//! `tests/mlt_export_integration.rs`, which renders it and inspects the
//! output with `ffprobe`); `file.export` spawns a real `melt` subprocess in
//! the background and reports real completion via `jobs_get`;
//! `playback_get_frame` shells out to `melt` to render one real frame and
//! returns its real bytes; clip in/out points come from real `ffprobe`
//! probing of the source file, not guesses.
//!
//! **What's simulated**: there is no live Qt/QUndoStack anywhere in this
//! file -- `project_undo`/`project_redo` are plain depth counters (the same
//! honesty caveat `MockBackend` already documents), and
//! `transitions.addCrossfade`'s nested-tractor XML is a simplified version
//! of Shotcut's exact `MultitrackModel::addTransition` splitting logic
//! (real MLT `luma` + `mix` transitions, not the literal `movit.luma_mix`/
//! `"mix:-2"` service-string details cited from the real source in
//! `01-jsonrpc-spec.md`, which don't correspond to standalone registered
//! MLT service names outside that exact call site).
//!
//! **Multi-track video compositing** (added for `11-e2e-scenario-tests.md`'s
//! Phase A overlay-track requirement): every pair of consecutive *video*
//! tracks gets a real `qtblend` `<transition>` planted between them in the
//! top-level `<tractor>`, bottom track as `a_track`, the next-higher video
//! track as `b_track` -- this is the exact real primitive
//! `MultitrackModel::getVideoBlendTransition`/`addVideoTrack` in
//! `shotcut/src/models/multitrackmodel.cpp` uses (confirmed by reading that
//! source), and empirically verified against the installed `melt 7.36.1` by
//! rendering a two-track probe XML and pixel-diffing decoded frames before/
//! during/after the top track's visible window. Audio across tracks relies
//! on MLT tractor's own default implicit summing (no explicit `mix`
//! transitions are planted here) -- untested for exact levels, but a real,
//! not fabricated, MLT behavior.
//!
//! Mid-timeline positioning on an overlay track (no `position`/offset
//! parameter exists on `edit_append_clip`, by design -- the trait wasn't
//! extended for this) is done the same way real MLT playlists represent
//! gaps: a transparent `color:#00000000` spacer clip, addressable through
//! the *existing* `source: Value` tagged union as `{"blank": <frames>}`
//! (handled only inside this file's `resolve_source_direct` -- no `Backend`
//! trait or wire-protocol change). `edit.appendClip({trackIndex, source:
//! {"blank": N}})` then a real content clip reproduces exactly what a
//! real Shotcut timeline gap looks like in MLT XML (`<blank length="N"/>`).
//!
//! **Keyframed `transition.rect` caveat (empirically discovered, not in any
//! doc)**: MLT's legacy `rect`/`mlt_geometry`-typed properties (used by the
//! `affine` filter's `transition.rect`) tween *back toward the first
//! keyframe's value* past the last explicit keyframe if no keyframe pins
//! the end -- verified by rendering a 2-keyframe slide-in and watching the
//! overlay slide back out again with no third keyframe. A held end value
//! needs an explicit keyframe at (or past) the last frame you want it to
//! hold for, same as real Shotcut's own keyframe panel always writes.
//! Numeric (non-rect) animated properties like `brightness`'s `level` do
//! *not* have this quirk (confirmed via the same probe) -- they clamp-hold
//! past the last keyframe as usually assumed.
//!
//! **Subtitles**: real Shotcut's own mechanism (`subtitle_feed` filter +
//! `subtitle.N.feed`/`subtitle.N.lang` consumer properties, see
//! `shotcut/src/models/subtitlesmodel.cpp` and `encodedock.cpp`) was tested
//! directly against `melt` with a real SRT path and produced only an empty
//! placeholder `mov_text` stream (0 real packets) -- that mechanism depends
//! on a live Shotcut `Subtitles` QObject injecting per-frame cue-text frame
//! properties during rendering, which doesn't exist when driving `melt` as
//! a bare CLI subprocess. The mechanism that *does* work standalone,
//! confirmed by decoding frames and finding real burned-in white/black-
//! outline text pixels inside vs. outside the cue window: ffmpeg's own
//! `avfilter.subtitles` MLT service (`av.filename=<path.srt>`), attached as
//! a `<filter>` on the top-level `<tractor>` (post-composite, so it burns
//! in over whatever the tracks below produced). That's what
//! `build_mlt_xml` attaches per subtitle track below -- real pixel burn-in,
//! not a Shotcut-GUI-only overlay, and not a silently-unused sidecar file.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::backend::{
    parse_mlt_keyframe_entry, Backend, BackendError, BackendResult, Clip, FileProbe, FilterInfo,
    FilterListEntry, JobStatus, KeyframeInfo, Marker, PlaylistEntry, PlaylistEntryDetail, ProjectState,
    SplitClipResult, SubtitleTrackInfo, Track, TransitionInfo,
};

/// Project frame rate assumed throughout this backend's MLT profile and
/// frame-count math. A real implementation would read this per-source; this
/// one fixes it project-wide for simplicity (documented, not hidden).
const DEFAULT_FPS: i64 = 30;
/// Default duration for a generated title clip (no natural source length),
/// matching real Shotcut's ballpark default title length.
const DEFAULT_TITLE_DURATION_FRAMES: i64 = 150; // 5s @ 30fps

// --------------------------------------------------------------------
// In-memory project model
// --------------------------------------------------------------------

#[derive(Clone)]
enum ProducerSpec {
    File { path: String },
    Title { mode: String, text: String, bg: String, fg: String },
    /// A transparent spacer, for positioning a real clip mid-timeline on a
    /// track that would otherwise be empty up to that point -- see the
    /// module doc comment's "Mid-timeline positioning" note. Serializes to
    /// a fully-transparent `color:` producer, same technique already used
    /// for a title's background.
    Blank { frames: i64 },
}

#[derive(Clone)]
struct MltFilter {
    mlt_service: String,
    properties: HashMap<String, String>,
    /// property name -> sorted `(position, "pos[tag]=value")` entries,
    /// joined with `;` at serialization time into MLT's animated-property
    /// syntax.
    keyframes: HashMap<String, Vec<(i64, String)>>,
}

#[derive(Clone)]
struct MltClip {
    clip_id: String,
    /// The raw `source` the caller passed to `edit.appendClip`, kept
    /// verbatim so `edit_list_clips` can round-trip it faithfully.
    source: Value,
    producer: ProducerSpec,
    in_frame: i64,
    out_frame: i64,
    filters: Vec<MltFilter>,
}

#[derive(Clone)]
struct CrossfadeRecord {
    between_clips: (usize, usize),
    duration_frames: i64,
}

struct MltProjectData {
    /// `<projectsRoot>/<projectId>/`, per 09-project-folder-layout.md.
    root: PathBuf,
    dirty: bool,
    undo_depth: usize,
    redo_depth: usize,
    tracks: Vec<Track>,
    clips: HashMap<usize, Vec<MltClip>>, // track_index -> ordered clips
    playlist_bin: Vec<PlaylistEntry>,
    bin_producers: HashMap<usize, ProducerSpec>, // playlist bin index -> resolved producer
    notes: String,
    subtitle_tracks: Vec<PathBuf>, // per subtitle track index: its .srt sidecar path
    transitions: HashMap<usize, Vec<CrossfadeRecord>>, // track_index -> crossfades
    next_clip_seq: u64,
    markers: Vec<Marker>,
    /// Newest-first, deduped on add. Persisted under `.snapshot/recent.json`.
    recent: Vec<String>,
}

impl MltProjectData {
    fn new(root: PathBuf) -> Self {
        let mut data = Self {
            root,
            dirty: false,
            undo_depth: 0,
            redo_depth: 0,
            tracks: Vec::new(),
            clips: HashMap::new(),
            playlist_bin: Vec::new(),
            bin_producers: HashMap::new(),
            notes: String::new(),
            subtitle_tracks: Vec::new(),
            transitions: HashMap::new(),
            next_clip_seq: 0,
            markers: Vec::new(),
            recent: Vec::new(),
        };
        data.load_markers_from_disk();
        data.load_recent_from_disk();
        data
    }

    fn snapshot_dir(&self) -> PathBuf {
        self.root.join(".snapshot")
    }

    fn load_markers_from_disk(&mut self) {
        let path = self.snapshot_dir().join("markers.json");
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(mut markers) = serde_json::from_str::<Vec<Marker>>(&raw) {
                for (i, m) in markers.iter_mut().enumerate() {
                    m.index = i;
                }
                self.markers = markers;
            }
        }
    }

    fn load_recent_from_disk(&mut self) {
        let path = self.snapshot_dir().join("recent.json");
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(recent) = serde_json::from_str::<Vec<String>>(&raw) {
                self.recent = recent;
            }
        }
    }

    fn persist_markers(&self) -> BackendResult<()> {
        let dir = self.snapshot_dir();
        fs::create_dir_all(&dir).map_err(|e| {
            BackendError::InvalidParams(format!("failed to create .snapshot dir: {e}"))
        })?;
        let path = dir.join("markers.json");
        let raw = serde_json::to_string_pretty(&self.markers).map_err(|e| {
            BackendError::InvalidParams(format!("failed to serialize markers: {e}"))
        })?;
        fs::write(&path, raw).map_err(|e| {
            BackendError::InvalidParams(format!("failed to write {}: {e}", path.display()))
        })?;
        Ok(())
    }

    fn persist_recent(&self) -> BackendResult<()> {
        let dir = self.snapshot_dir();
        fs::create_dir_all(&dir).map_err(|e| {
            BackendError::InvalidParams(format!("failed to create .snapshot dir: {e}"))
        })?;
        let path = dir.join("recent.json");
        let raw = serde_json::to_string_pretty(&self.recent).map_err(|e| {
            BackendError::InvalidParams(format!("failed to serialize recent: {e}"))
        })?;
        fs::write(&path, raw).map_err(|e| {
            BackendError::InvalidParams(format!("failed to write {}: {e}", path.display()))
        })?;
        Ok(())
    }
}

fn find_clip_mut<'a>(data: &'a mut MltProjectData, clip_id: &str) -> Option<&'a mut MltClip> {
    data.clips.values_mut().flat_map(|v| v.iter_mut()).find(|c| c.clip_id == clip_id)
}

fn find_clip<'a>(data: &'a MltProjectData, clip_id: &str) -> Option<&'a MltClip> {
    data.clips.values().flat_map(|v| v.iter()).find(|c| c.clip_id == clip_id)
}

// --------------------------------------------------------------------
// MltBackend
// --------------------------------------------------------------------

/// Third `Backend` implementor, independent of both `MockBackend` (pure
/// in-memory, no real media) and `FfiBackend` (needs a live Shotcut/Qt
/// process). Always available -- only needs `melt`/`ffprobe` on `PATH` (or
/// `MELT_BIN`/`FFPROBE_BIN` env overrides) at call time, not at build time.
pub struct MltBackend {
    projects_root: PathBuf,
    fixed_root: bool,
    projects: HashMap<String, MltProjectData>,
    /// Export jobs, keyed by jobId. Not project-scoped in storage (matching
    /// `Backend::jobs_get`'s signature, which takes no `project_id`) --
    /// shared behind a mutex so the background `melt`-waiting thread
    /// spawned by `file_export` can update status without going back
    /// through the single dispatcher thread.
    jobs: Arc<Mutex<HashMap<String, JobStatus>>>,
    /// Maps each export job to its project so `jobs.list` is project-scoped
    /// even when this backend hosts multiple standalone projects.
    job_projects: HashMap<String, String>,
    /// Live `melt` child processes for `jobs.stop`. Shared with the
    /// background waiter so either side can take ownership of the Child.
    job_children: HashMap<String, Arc<Mutex<Option<Child>>>>,
}

impl MltBackend {
    /// `projects_root` is the daemon-level projects directory per
    /// 09-project-folder-layout.md (e.g. `~/Snapshot/Projects/`); each
    /// `project_id` gets its own `<projects_root>/<project_id>/` folder,
    /// created lazily on first `project_select`.
    pub fn new(projects_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: projects_root.into(),
            fixed_root: false,
            projects: HashMap::new(),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            job_projects: HashMap::new(),
            job_children: HashMap::new(),
        }
    }

    /// Creates a backend for a daemon process already bound to one project's
    /// exact root. All project artifacts are written directly in this
    /// directory, matching doc 09's per-project layout.
    pub fn new_fixed_root(project_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: project_root.into(),
            fixed_root: true,
            projects: HashMap::new(),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            job_projects: HashMap::new(),
            job_children: HashMap::new(),
        }
    }

    fn project_mut(&mut self, project_id: &str) -> BackendResult<&mut MltProjectData> {
        if !self.projects.contains_key(project_id) {
            let root = if self.fixed_root {
                self.projects_root.clone()
            } else {
                self.projects_root.join(project_id)
            };
            fs::create_dir_all(&root).map_err(|e| {
                BackendError::InvalidParams(format!("failed to create project dir {}: {e}", root.display()))
            })?;
            self.projects.insert(project_id.to_string(), MltProjectData::new(root));
        }
        Ok(self.projects.get_mut(project_id).expect("just inserted"))
    }

    fn project_ref(&self, project_id: &str) -> BackendResult<&MltProjectData> {
        self.projects.get(project_id).ok_or_else(|| BackendError::NotFound(format!("project {project_id} not selected")))
    }
}

impl Backend for MltBackend {
    fn project_select(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        self.project_mut(project_id)?;
        // Bug fix: previously never wired up -- see the same fix + comment
        // in backend.rs's MockBackend::project_select for the rationale
        // (recent-list state is per-ProjectData here, so recording the
        // project's own id on select is this backend's equivalent of
        // 10-testing-plan.md's Phase 3 recent.* requirement).
        self.recent_add(project_id, project_id)?;
        self.project_get_state(project_id)
    }

    fn project_exit(&mut self) -> BackendResult<()> {
        Ok(())
    }

    fn project_get_state(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        let data = self.project_mut(project_id)?;
        Ok(ProjectState {
            project_id: project_id.to_string(),
            dirty: data.dirty,
            undo_depth: data.undo_depth,
            redo_depth: data.redo_depth,
        })
    }

    fn project_save(&mut self, project_id: &str) -> BackendResult<()> {
        let xml = build_mlt_xml(self.project_ref(project_id)?)?;
        let data = self.project_mut(project_id)?;
        fs::write(data.root.join("project.mlt"), xml)
            .map_err(|e| BackendError::InvalidParams(format!("failed to save project.mlt: {e}")))?;
        data.dirty = false;
        Ok(())
    }

    fn project_undo(&mut self, project_id: &str) -> BackendResult<()> {
        // Depth-only, same honesty caveat as MockBackend: no real rewind of
        // the in-memory model happens here.
        let data = self.project_mut(project_id)?;
        if data.undo_depth == 0 {
            return Err(BackendError::NotFound("nothing to undo".into()));
        }
        data.undo_depth -= 1;
        data.redo_depth += 1;
        Ok(())
    }

    fn project_redo(&mut self, project_id: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
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
        let data = self.project_mut(project_id)?;
        let track = Track { index: data.tracks.len(), kind: kind.to_string() };
        data.tracks.push(track.clone());
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(track)
    }

    fn edit_remove_track(&mut self, project_id: &str, track_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        data.tracks.remove(track_index);
        data.clips.remove(&track_index);
        data.transitions.remove(&track_index);
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn edit_list_tracks(&mut self, project_id: &str) -> BackendResult<Vec<Track>> {
        Ok(self.project_mut(project_id)?.tracks.clone())
    }

    fn edit_append_clip(&mut self, project_id: &str, track_index: usize, source: Value) -> BackendResult<Clip> {
        let data = self.project_mut(project_id)?;
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        let producer = resolve_source(data, &source)?;
        let (in_frame, out_frame) = default_in_out(&producer)?;
        data.next_clip_seq += 1;
        let clip_id = format!("clip-{}", data.next_clip_seq);
        let clips = data.clips.entry(track_index).or_default();
        let index = clips.len();
        clips.push(MltClip { clip_id: clip_id.clone(), source: source.clone(), producer, in_frame, out_frame, filters: Vec::new() });
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(Clip { clip_id, index, source, in_frame, out_frame })
    }

    fn edit_list_clips(&mut self, project_id: &str, track_index: usize) -> BackendResult<Vec<Clip>> {
        let data = self.project_mut(project_id)?;
        Ok(data
            .clips
            .get(&track_index)
            .map(|clips| {
                clips
                    .iter()
                    .enumerate()
                    .map(|(index, c)| Clip {
                        clip_id: c.clip_id.clone(),
                        index,
                        source: c.source.clone(),
                        in_frame: c.in_frame,
                        out_frame: c.out_frame,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn playback_seek(&mut self, _project_id: &str, _frame: i64) -> BackendResult<()> {
        Ok(())
    }

    fn notes_get_text(&mut self, project_id: &str) -> BackendResult<String> {
        Ok(self.project_mut(project_id)?.notes.clone())
    }

    fn notes_set_text(&mut self, project_id: &str, text: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        data.notes = text.to_string();
        data.dirty = true;
        Ok(())
    }

    fn playlist_append(&mut self, project_id: &str, source: Value, name: Option<String>) -> BackendResult<PlaylistEntry> {
        let producer = resolve_source_direct(&source)?;
        let (in_f, out_f) = default_in_out(&producer)?;
        let duration_frames = out_f - in_f + 1;
        let data = self.project_mut(project_id)?;
        let index = data.playlist_bin.len();
        let entry = PlaylistEntry { index, name: name.unwrap_or_else(|| format!("clip{index}")), source, duration_frames };
        data.playlist_bin.push(entry.clone());
        data.bin_producers.insert(index, producer);
        data.dirty = true;
        Ok(entry)
    }

    fn playlist_list(&mut self, project_id: &str) -> BackendResult<Vec<PlaylistEntry>> {
        Ok(self.project_mut(project_id)?.playlist_bin.clone())
    }

    fn playlist_insert(
        &mut self,
        project_id: &str,
        index: usize,
        source: Value,
        name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        let producer = resolve_source_direct(&source)?;
        let (in_f, out_f) = default_in_out(&producer)?;
        let duration_frames = out_f - in_f + 1;
        let data = self.project_mut(project_id)?;
        if index > data.playlist_bin.len() {
            return Err(BackendError::InvalidParams(format!(
                "playlist.insert index {index} out of range (len {})",
                data.playlist_bin.len()
            )));
        }
        let mut producers = bin_producers_as_vec(data);
        producers.insert(index, producer);
        let entry = PlaylistEntry { index, name: name.unwrap_or_else(|| format!("clip{index}")), source, duration_frames };
        data.playlist_bin.insert(index, entry);
        for (i, e) in data.playlist_bin.iter_mut().enumerate() {
            e.index = i;
        }
        set_bin_producers_from_vec(data, producers);
        data.dirty = true;
        Ok(data.playlist_bin[index].clone())
    }

    fn playlist_remove(&mut self, project_id: &str, index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        if index >= data.playlist_bin.len() {
            return Err(BackendError::NotFound(format!("playlist index {index}")));
        }
        let mut producers = bin_producers_as_vec(data);
        producers.remove(index);
        data.playlist_bin.remove(index);
        for (i, e) in data.playlist_bin.iter_mut().enumerate() {
            e.index = i;
        }
        set_bin_producers_from_vec(data, producers);
        data.dirty = true;
        Ok(())
    }

    fn playlist_move(&mut self, project_id: &str, from_index: usize, to_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        let len = data.playlist_bin.len();
        if from_index >= len {
            return Err(BackendError::NotFound(format!("playlist index {from_index}")));
        }
        if to_index >= len {
            return Err(BackendError::InvalidParams(format!("toIndex {to_index} out of range (len {len})")));
        }
        let mut producers = bin_producers_as_vec(data);
        let p = producers.remove(from_index);
        producers.insert(to_index, p);
        let entry = data.playlist_bin.remove(from_index);
        data.playlist_bin.insert(to_index, entry);
        for (i, e) in data.playlist_bin.iter_mut().enumerate() {
            e.index = i;
        }
        set_bin_producers_from_vec(data, producers);
        data.dirty = true;
        Ok(())
    }

    fn playlist_get(&mut self, project_id: &str, index: usize) -> BackendResult<PlaylistEntryDetail> {
        let data = self.project_mut(project_id)?;
        let entry = data
            .playlist_bin
            .get(index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("playlist index {index}")))?;
        // Reuse the same real `ffprobe`-backed helper `file.probe` uses --
        // only meaningful for file-backed sources (a generator/title or
        // blank-spacer entry has no real file to probe, so `probe` is
        // honestly `None` there, not an error).
        let probe = entry
            .source
            .get("path")
            .and_then(Value::as_str)
            .and_then(|p| probe_media(p).ok());
        Ok(PlaylistEntryDetail {
            index: entry.index,
            name: entry.name,
            source: entry.source,
            duration_frames: entry.duration_frames,
            probe,
        })
    }

    fn file_import(&mut self, project_id: &str, path: &str) -> BackendResult<PlaylistEntry> {
        let project_root = self.project_ref(project_id)?.root.clone();
        let canonical_root = fs::canonicalize(&project_root).map_err(|e| {
            BackendError::InvalidParams(format!(
                "failed to resolve project root {}: {e}",
                project_root.display()
            ))
        })?;
        let requested_path = Path::new(path);
        let candidate = if requested_path.is_absolute() {
            requested_path.to_path_buf()
        } else {
            project_root.join(requested_path)
        };
        let canonical_path = fs::canonicalize(&candidate).map_err(|e| {
            BackendError::InvalidParams(format!("file.import path {} is not readable: {e}", candidate.display()))
        })?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(BackendError::InvalidParams(format!(
                "file.import path {} is outside project root {}",
                canonical_path.display(),
                canonical_root.display()
            )));
        }

        let producer = resolve_source_direct(&json!({"path": canonical_path.to_string_lossy()}))?;
        let (in_frame, out_frame) = default_in_out(&producer)?;
        let data = self.project_mut(project_id)?;
        let index = data.playlist_bin.len();
        let name = canonical_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("imported-file")
            .to_string();
        let source = json!({"path": canonical_path.to_string_lossy()});
        let entry = PlaylistEntry {
            index,
            name,
            source,
            duration_frames: out_frame - in_frame + 1,
        };
        data.playlist_bin.push(entry.clone());
        data.bin_producers.insert(index, producer);
        data.dirty = true;
        Ok(entry)
    }

    fn edit_trim_clip_in(&mut self, project_id: &str, track_index: usize, clip_index: usize, new_frame: i64) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        let clip = data
            .clips
            .get_mut(&track_index)
            .and_then(|c| c.get_mut(clip_index))
            .ok_or_else(|| BackendError::NotFound(format!("clip {track_index}/{clip_index}")))?;
        if new_frame < 0 || new_frame >= clip.out_frame {
            return Err(BackendError::InvalidParams(format!(
                "newFrame {new_frame} out of range for clip (out={})",
                clip.out_frame
            )));
        }
        clip.in_frame = new_frame;
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn edit_trim_clip_out(&mut self, project_id: &str, track_index: usize, clip_index: usize, new_frame: i64) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        let clip = data
            .clips
            .get_mut(&track_index)
            .and_then(|c| c.get_mut(clip_index))
            .ok_or_else(|| BackendError::NotFound(format!("clip {track_index}/{clip_index}")))?;
        if new_frame <= clip.in_frame {
            return Err(BackendError::InvalidParams(format!(
                "newFrame {new_frame} must be greater than inFrame {}",
                clip.in_frame
            )));
        }
        clip.out_frame = new_frame;
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(())
    }

    fn edit_split_clip(
        &mut self,
        project_id: &str,
        track_index: usize,
        clip_index: usize,
        position: i64,
    ) -> BackendResult<SplitClipResult> {
        let data = self.project_mut(project_id)?;
        let (left_clip_id, source, producer, out_frame, filters) = {
            let clips = data
                .clips
                .get(&track_index)
                .ok_or_else(|| BackendError::NotFound(format!("track {track_index}")))?;
            if clip_index >= clips.len() {
                return Err(BackendError::NotFound(format!("clip {track_index}/{clip_index}")));
            }
            let left = &clips[clip_index];
            if position <= left.in_frame || position > left.out_frame {
                return Err(BackendError::InvalidParams(format!(
                    "position {position} must be strictly between inFrame {} and outFrame {} (inclusive of outFrame)",
                    left.in_frame, left.out_frame
                )));
            }
            (
                left.clip_id.clone(),
                left.source.clone(),
                left.producer.clone(),
                left.out_frame,
                left.filters.clone(),
            )
        };

        data.next_clip_seq += 1;
        let right_clip_id = format!("clip-{}", data.next_clip_seq);

        let clips = data.clips.get_mut(&track_index).expect("track clips");
        clips[clip_index].out_frame = position - 1;
        clips.insert(
            clip_index + 1,
            MltClip {
                clip_id: right_clip_id.clone(),
                source,
                producer,
                in_frame: position,
                out_frame,
                filters,
            },
        );
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
        let data = self.project_mut(project_id)?;
        if track_index >= data.tracks.len() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        let clip_count = data.clips.get(&track_index).map(|c| c.len()).unwrap_or(0);
        if between_clips.0 >= clip_count || between_clips.1 >= clip_count {
            return Err(BackendError::NotFound(format!("clip index out of range on track {track_index}")));
        }
        if between_clips.1 != between_clips.0 + 1 {
            return Err(BackendError::InvalidParams("transitions.addCrossfade requires adjacent clip indices".into()));
        }
        if duration_frames <= 0 {
            return Err(BackendError::InvalidParams("durationFrames must be positive".into()));
        }
        let list = data.transitions.entry(track_index).or_default();
        let transition_index = list.len();
        list.push(CrossfadeRecord { between_clips, duration_frames });
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
        Ok(TransitionInfo { track_index, transition_index, between_clips, duration_frames })
    }

    fn filter_add(&mut self, project_id: &str, clip_id: &str, mlt_service: &str, properties: Value) -> BackendResult<FilterInfo> {
        let mut props = HashMap::new();
        if let Value::Object(map) = properties {
            for (k, v) in map {
                props.insert(k, json_value_to_mlt_prop(&v));
            }
        }
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        let filter_index = clip.filters.len();
        clip.filters.push(MltFilter { mlt_service: mlt_service.to_string(), properties: props, keyframes: HashMap::new() });
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
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
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        let filter = clip
            .filters
            .get_mut(filter_index)
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        match position {
            Some(position) => {
                let entry = format!("{position}={}", json_value_to_mlt_prop(&value));
                let list = filter.keyframes.entry(property.to_string()).or_default();
                list.retain(|(p, _)| *p != position);
                list.push((position, entry));
                list.sort_by_key(|(p, _)| *p);
            }
            None => {
                filter.properties.insert(property.to_string(), json_value_to_mlt_prop(&value));
                filter.keyframes.remove(property);
            }
        }
        data.dirty = true;
        data.undo_depth += 1;
        data.redo_depth = 0;
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
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        let filter = clip
            .filters
            .get_mut(filter_index)
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        // MLT's animated-property tag: "" = linear (default), "~" = smooth
        // (catmull-rom), "|" = discrete/hold.
        let tag = match interpolation {
            "smooth" => "~",
            "discrete" | "hold" => "|",
            _ => "",
        };
        let entry = format!("{position}{tag}={}", json_value_to_mlt_prop(&value));
        let list = filter.keyframes.entry(property.to_string()).or_default();
        list.retain(|(p, _)| *p != position);
        list.push((position, entry));
        list.sort_by_key(|(p, _)| *p);
        data.dirty = true;
        Ok(())
    }

    fn filter_list(
        &mut self,
        project_id: &str,
        clip_id: &str,
    ) -> BackendResult<Vec<FilterListEntry>> {
        let data = self.project_mut(project_id)?;
        let clip = find_clip(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        Ok(clip
            .filters
            .iter()
            .enumerate()
            .map(|(index, f)| {
                let mut map = serde_json::Map::new();
                for (k, v) in &f.properties {
                    if f.keyframes.contains_key(k) {
                        continue;
                    }
                    map.insert(k.clone(), mlt_prop_to_json(v));
                }
                FilterListEntry {
                    index,
                    mlt_service: f.mlt_service.clone(),
                    properties: Value::Object(map),
                }
            })
            .collect())
    }

    fn filter_remove(
        &mut self,
        project_id: &str,
        clip_id: &str,
        filter_index: usize,
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        if filter_index >= clip.filters.len() {
            return Err(BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")));
        }
        clip.filters.remove(filter_index);
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
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        if filter_index >= clip.filters.len() {
            return Err(BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")));
        }
        if new_index >= clip.filters.len() {
            return Err(BackendError::InvalidParams(format!(
                "newIndex {new_index} out of range (len={})",
                clip.filters.len()
            )));
        }
        if filter_index != new_index {
            let item = clip.filters.remove(filter_index);
            clip.filters.insert(new_index, item);
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
        let data = self.project_mut(project_id)?;
        let clip = find_clip(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        let filter = clip
            .filters
            .get(filter_index)
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        let mut list = filter
            .keyframes
            .get(property)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(_, entry)| parse_mlt_keyframe_entry(entry))
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
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id).ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        let filter = clip
            .filters
            .get_mut(filter_index)
            .ok_or_else(|| BackendError::NotFound(format!("filter {filter_index} on clip {clip_id}")))?;
        let list = filter
            .keyframes
            .get_mut(property)
            .ok_or_else(|| {
                BackendError::NotFound(format!(
                    "keyframe at {position} on property {property} of filter {filter_index}"
                ))
            })?;
        let before = list.len();
        list.retain(|(p, _)| *p != position);
        if list.len() == before {
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
        let data = self.project_mut(project_id)?;
        let clip = find_clip_mut(data, clip_id)
            .ok_or_else(|| BackendError::NotFound(format!("clip {clip_id}")))?;
        Ok((clip.out_frame - clip.in_frame + 1).max(0))
    }

    fn generator_create_title(&mut self, project_id: &str, params: Value) -> BackendResult<PlaylistEntry> {
        let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("simple").to_string();
        let text = params
            .get("text")
            .or_else(|| params.get("html"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| BackendError::InvalidParams("generator.createTitle requires text (or html)".into()))?
            .to_string();
        let fg = params.get("fgColour").and_then(|v| v.as_str()).unwrap_or("#ffffffff").to_string();
        let bg = params.get("bgColour").and_then(|v| v.as_str()).unwrap_or("#00000000").to_string();
        let producer = ProducerSpec::Title { mode: mode.clone(), text: text.clone(), bg, fg };
        let (in_f, out_f) = default_in_out(&producer)?;
        let duration_frames = out_f - in_f + 1;
        let data = self.project_mut(project_id)?;
        let index = data.playlist_bin.len();
        let source = json!({"kind": "title", "mode": mode, "text": text});
        let entry = PlaylistEntry { index, name: format!("Title: {text}"), source, duration_frames };
        data.playlist_bin.push(entry.clone());
        data.bin_producers.insert(index, producer);
        data.dirty = true;
        Ok(entry)
    }

    fn subtitles_add_track(&mut self, project_id: &str) -> BackendResult<SubtitleTrackInfo> {
        let data = self.project_mut(project_id)?;
        let track_index = data.subtitle_tracks.len();
        // Real Shotcut backs subtitles via SRT I/O (SubtitlesModel/Subtitles,
        // per 01-jsonrpc-spec.md's subtitles.* namespace note) -- an SRT
        // sidecar file is the faithful choice for storage. Unlike an
        // earlier version of this comment claimed, the sidecar *is* real
        // MLT-embedded burn-in at export time: `build_mlt_xml` attaches an
        // `avfilter.subtitles` filter (`av.filename=<this path>`) to the
        // tractor, which really burns the cues into exported pixels --
        // empirically confirmed (real Shotcut's own `subtitle_feed`
        // mechanism does not work standalone via `melt` CLI; see the
        // module doc comment for what was tested and why).
        let subs_dir = data.root.join("subtitles");
        fs::create_dir_all(&subs_dir).map_err(|e| BackendError::InvalidParams(format!("failed to create subtitles dir: {e}")))?;
        let path = subs_dir.join(format!("track{track_index}.srt"));
        fs::write(&path, b"").map_err(|e| BackendError::InvalidParams(format!("failed to create {}: {e}", path.display())))?;
        data.subtitle_tracks.push(path);
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
        let data = self.project_mut(project_id)?;
        let path = data
            .subtitle_tracks
            .get(track_index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("subtitle track {track_index}")))?;
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let cue_number = existing.matches("-->").count() + 1;
        let start_ts = frames_to_srt_timestamp(start_frame, DEFAULT_FPS);
        let end_ts = frames_to_srt_timestamp(end_frame, DEFAULT_FPS);
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| BackendError::InvalidParams(format!("failed to open {}: {e}", path.display())))?;
        writeln!(f, "{cue_number}\n{start_ts} --> {end_ts}\n{text}\n")
            .map_err(|e| BackendError::InvalidParams(format!("failed to write subtitle item: {e}")))?;
        data.dirty = true;
        Ok(())
    }

    fn subtitles_remove_items(
        &mut self,
        project_id: &str,
        track_index: usize,
        item_indices: &[usize],
    ) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        let path = data
            .subtitle_tracks
            .get(track_index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("subtitle track {track_index}")))?;
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let rewritten = remove_srt_cues(&existing, item_indices).map_err(BackendError::InvalidParams)?;
        fs::write(&path, rewritten).map_err(|e| {
            BackendError::InvalidParams(format!("failed to rewrite {}: {e}", path.display()))
        })?;
        data.dirty = true;
        Ok(())
    }

    fn subtitles_import_srt(
        &mut self,
        project_id: &str,
        path: &str,
        new_track: bool,
    ) -> BackendResult<SubtitleTrackInfo> {
        let root = self.project_ref(project_id)?.root.clone();
        let requested = Path::new(path);
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            root.join(requested)
        };
        // Allow any readable path (absolute outside the project root is OK):
        // agents commonly import caption files from outside the project tree.
        // Relative paths resolve against the project root for convenience.
        let content = fs::read_to_string(&candidate).map_err(|e| {
            BackendError::InvalidParams(format!(
                "subtitles.importSrt path {} is not readable: {e}",
                candidate.display()
            ))
        })?;
        // Normalize numbering by parse+rewrite even if the source is already valid SRT.
        let normalized = format_srt(&parse_srt(&content));

        let track_index = if new_track || self.project_ref(project_id)?.subtitle_tracks.is_empty() {
            self.subtitles_add_track(project_id)?.track_index
        } else {
            0
        };

        let data = self.project_mut(project_id)?;
        let dest = data
            .subtitle_tracks
            .get(track_index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("subtitle track {track_index}")))?;
        fs::write(&dest, normalized).map_err(|e| {
            BackendError::InvalidParams(format!("failed to write {}: {e}", dest.display()))
        })?;
        data.dirty = true;
        Ok(SubtitleTrackInfo { track_index })
    }

    fn subtitles_export_srt(
        &mut self,
        project_id: &str,
        path: &str,
        track_index: usize,
    ) -> BackendResult<String> {
        let data = self.project_ref(project_id)?;
        let srt_src = data
            .subtitle_tracks
            .get(track_index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("subtitle track {track_index}")))?;
        let content = fs::read_to_string(&srt_src).unwrap_or_default();
        let requested = Path::new(path);
        let dest = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            data.root.join(requested)
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                BackendError::InvalidParams(format!("failed to create export dir: {e}"))
            })?;
        }
        fs::write(&dest, content).map_err(|e| {
            BackendError::InvalidParams(format!("failed to write SRT to {}: {e}", dest.display()))
        })?;
        Ok(dest.to_string_lossy().into_owned())
    }

    fn file_export(&mut self, project_id: &str, output_path: &str, codec: &str, container: &str) -> BackendResult<String> {
        let xml = {
            let data = self.project_ref(project_id)?;
            let has_clips = data.clips.values().any(|c| !c.is_empty());
            if !has_clips {
                return Err(BackendError::InvalidParams("cannot export a project with no clips".into()));
            }
            build_mlt_xml(data)?
        };
        let data = self.project_mut(project_id)?;
        let mlt_path = data.root.join("project.mlt");
        fs::write(&mlt_path, &xml).map_err(|e| BackendError::InvalidParams(format!("failed to write {}: {e}", mlt_path.display())))?;

        let resolved_output = resolve_output_path(&data.root, output_path, container);
        if let Some(parent) = resolved_output.parent() {
            fs::create_dir_all(parent).map_err(|e| BackendError::InvalidParams(format!("failed to create export dir: {e}")))?;
        }

        let vcodec = normalize_vcodec(codec);
        let melt_bin = resolve_melt_binary();
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".to_string());
        // melt links Qt; without a display the xcb plugin aborts. Prefer an
        // explicit QT_QPA_PLATFORM from the environment, otherwise offscreen.
        let qt_platform = std::env::var("QT_QPA_PLATFORM").unwrap_or_else(|_| "offscreen".to_string());

        let mut cmd = Command::new(&melt_bin);
        cmd.arg(&mlt_path)
            .arg("-consumer")
            .arg(format!("avformat:{}", resolved_output.display()))
            .arg(format!("vcodec={vcodec}"))
            .arg("acodec=aac")
            .env("DISPLAY", &display)
            .env("QT_QPA_PLATFORM", &qt_platform)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd
            .spawn()
            .map_err(|e| BackendError::InvalidParams(format!("failed to spawn `{melt_bin}`: {e} (is melt on PATH, or MELT_BIN set?)")))?;

        let job_id = uuid::Uuid::new_v4().to_string();
        {
            let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
            jobs.insert(
                job_id.clone(),
                JobStatus {
                    job_id: job_id.clone(),
                    status: "running".into(),
                    percent: 0.0,
                    result_path: Some(resolved_output.to_string_lossy().into_owned()),
                    error: None,
                },
            );
        }
        self.job_projects.insert(job_id.clone(), project_id.to_string());

        // Keep a kill handle so `jobs.stop` can terminate the melt process.
        // The waiter polls `try_wait` rather than taking ownership via
        // `wait_with_output`, so either side can claim the Child.
        let child_slot = Arc::new(Mutex::new(Some(child)));
        self.job_children.insert(job_id.clone(), child_slot.clone());

        // `file.export` must return jobId immediately (01-jsonrpc-spec.md);
        // the actual render happens on a plain OS thread, *not* the shared
        // single-writer dispatcher, so a multi-second/minute melt run never
        // blocks any other client's requests.
        let jobs = self.jobs.clone();
        let job_id_bg = job_id.clone();
        std::thread::spawn(move || {
            let outcome = loop {
                let mut guard = child_slot.lock().expect("job child mutex poisoned");
                match guard.as_mut() {
                    None => {
                        // `jobs_stop` took the Child and already set status.
                        return;
                    }
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => {
                            let mut finished = guard.take().expect("child present after try_wait");
                            let mut stderr = String::new();
                            if let Some(mut pipe) = finished.stderr.take() {
                                let _ = pipe.read_to_string(&mut stderr);
                            }
                            break Ok((status, stderr));
                        }
                        Ok(None) => {
                            drop(guard);
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(e) => {
                            *guard = None;
                            break Err(e);
                        }
                    },
                }
            };

            let mut jobs = jobs.lock().expect("jobs mutex poisoned");
            if let Some(job) = jobs.get_mut(&job_id_bg) {
                // Don't overwrite a client-initiated stop.
                if job.status != "running" {
                    return;
                }
                match outcome {
                    Ok((status, stderr)) if status.success() => {
                        // `melt`/libavcodec exit 0 even when a requested
                        // video/audio codec name isn't a real encoder --
                        // it just logs "<codec> unrecognised - ignoring"
                        // to stderr and silently drops that stream (this
                        // is exactly how an agent-supplied codec like
                        // "h264" instead of the real encoder name
                        // "libx264" produced an audio-only "successful"
                        // export before normalize_vcodec existed). Treat
                        // that as a real failure, not success, so
                        // jobs.get never reports "done" for a broken
                        // export.
                        if let Some(bad) = detect_unrecognised_codec(&stderr) {
                            job.status = "error".into();
                            job.error = Some(format!(
                                "melt exited 0 but dropped a stream: {bad} (stderr: {stderr})"
                            ));
                        } else {
                            job.status = "done".into();
                            job.percent = 100.0;
                        }
                    }
                    Ok((status, stderr)) => {
                        job.status = "error".into();
                        job.error = Some(format!("melt exited with {status}: {stderr}"));
                    }
                    Err(e) => {
                        job.status = "error".into();
                        job.error = Some(format!("failed to wait on melt: {e}"));
                    }
                }
            }
        });

        Ok(job_id)
    }

    fn file_probe(&mut self, path: &str) -> BackendResult<FileProbe> {
        probe_media(path)
    }

    fn jobs_get(&mut self, job_id: &str) -> BackendResult<JobStatus> {
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .get(job_id)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("job {job_id}")))
    }

    fn jobs_list(&mut self, project_id: &str) -> BackendResult<Vec<JobStatus>> {
        let jobs = self.jobs.lock().expect("jobs mutex poisoned");
        let mut out = self
            .job_projects
            .iter()
            .filter(|(_, owner)| owner.as_str() == project_id)
            .filter_map(|(job_id, _)| jobs.get(job_id).cloned())
            .collect::<Vec<_>>();
        out.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        Ok(out)
    }

    fn jobs_stop(&mut self, job_id: &str) -> BackendResult<()> {
        {
            let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
            let job = jobs
                .get_mut(job_id)
                .ok_or_else(|| BackendError::NotFound(format!("job {job_id}")))?;
            if job.status != "running" {
                // Already terminal (done/error/stopped) — idempotent success.
                return Ok(());
            }
            job.status = "stopped".into();
            job.error = Some("stopped by client".into());
        }
        if let Some(slot) = self.job_children.remove(job_id) {
            if let Some(mut child) = slot.lock().expect("job child mutex poisoned").take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Ok(())
    }

    fn playback_get_frame(&mut self, project_id: &str, frame: i64, format: &str) -> BackendResult<String> {
        let xml = build_mlt_xml(self.project_ref(project_id)?)?;
        let data = self.project_mut(project_id)?;
        let mlt_path = data.root.join("project.mlt");
        fs::write(&mlt_path, &xml).map_err(|e| BackendError::InvalidParams(format!("failed to write {}: {e}", mlt_path.display())))?;

        let snapshot_dir = data.root.join(".snapshot");
        fs::create_dir_all(&snapshot_dir).map_err(|e| BackendError::InvalidParams(format!("failed to create .snapshot dir: {e}")))?;
        let ext = if format.eq_ignore_ascii_case("jpeg") || format.eq_ignore_ascii_case("jpg") { "jpg" } else { "png" };
        let frame_path = snapshot_dir.join(format!("frame-{frame}.{ext}"));

        let melt_bin = resolve_melt_binary();
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".to_string());
        let qt_platform = std::env::var("QT_QPA_PLATFORM").unwrap_or_else(|_| "offscreen".to_string());

        // Blocks the shared dispatcher for the duration of one frame render
        // (a real `melt` single-frame grab is sub-second) -- acceptable for
        // a one-off grab, unlike `file_export`'s full render, which is
        // deliberately backgrounded above.
        let output = Command::new(&melt_bin)
            .arg(&mlt_path)
            .arg(format!("in={frame}"))
            .arg(format!("out={frame}"))
            .arg("-consumer")
            .arg(format!("avformat:{}", frame_path.display()))
            .env("DISPLAY", &display)
            .env("QT_QPA_PLATFORM", &qt_platform)
            .output()
            .map_err(|e| BackendError::InvalidParams(format!("failed to spawn `{melt_bin}` for frame grab: {e}")))?;

        if !output.status.success() || !frame_path.exists() {
            return Err(BackendError::InvalidParams(format!(
                "melt frame grab failed (frame {frame}): {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let bytes = fs::read(&frame_path).map_err(|e| BackendError::InvalidParams(format!("failed to read grabbed frame: {e}")))?;
        Ok(base64_encode(&bytes))
    }

    fn markers_append(
        &mut self,
        project_id: &str,
        frame: i64,
        text: Option<String>,
        color: Option<String>,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id)?;
        let marker = Marker {
            index: data.markers.len(),
            frame,
            text: text.unwrap_or_default(),
            color: color.unwrap_or_else(|| "#000000".to_string()),
            end_frame: None,
        };
        data.markers.push(marker.clone());
        data.dirty = true;
        data.persist_markers()?;
        Ok(marker)
    }

    fn markers_remove(&mut self, project_id: &str, marker_index: usize) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        if marker_index >= data.markers.len() {
            return Err(BackendError::NotFound(format!("marker {marker_index}")));
        }
        data.markers.remove(marker_index);
        for (i, m) in data.markers.iter_mut().enumerate() {
            m.index = i;
        }
        data.dirty = true;
        data.persist_markers()?;
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
        let data = self.project_mut(project_id)?;
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
        let out = data.markers[marker_index].clone();
        data.persist_markers()?;
        Ok(out)
    }

    fn markers_move(
        &mut self,
        project_id: &str,
        marker_index: usize,
        start: i64,
        end: i64,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id)?;
        {
            let marker = data
                .markers
                .get_mut(marker_index)
                .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))?;
            marker.frame = start;
            marker.end_frame = if end != start { Some(end) } else { None };
        }
        data.dirty = true;
        let out = data.markers[marker_index].clone();
        data.persist_markers()?;
        Ok(out)
    }

    fn markers_set_color(
        &mut self,
        project_id: &str,
        marker_index: usize,
        color: &str,
    ) -> BackendResult<Marker> {
        let data = self.project_mut(project_id)?;
        {
            let marker = data
                .markers
                .get_mut(marker_index)
                .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))?;
            marker.color = color.to_string();
        }
        data.dirty = true;
        let out = data.markers[marker_index].clone();
        data.persist_markers()?;
        Ok(out)
    }

    fn markers_clear(&mut self, project_id: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        data.markers.clear();
        data.dirty = true;
        data.persist_markers()?;
        Ok(())
    }

    fn markers_list(&mut self, project_id: &str) -> BackendResult<Vec<Marker>> {
        Ok(self.project_mut(project_id)?.markers.clone())
    }

    fn markers_get(&mut self, project_id: &str, marker_index: usize) -> BackendResult<Marker> {
        self.project_mut(project_id)?
            .markers
            .get(marker_index)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("marker {marker_index}")))
    }

    fn markers_next(&mut self, project_id: &str, from_frame: i64) -> BackendResult<Option<i64>> {
        let mut frames: Vec<i64> = self
            .project_mut(project_id)?
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
            .project_mut(project_id)?
            .markers
            .iter()
            .map(|m| m.frame)
            .filter(|f| *f < from_frame)
            .collect();
        frames.sort_unstable();
        Ok(frames.into_iter().next_back())
    }

    fn recent_add(&mut self, project_id: &str, path: &str) -> BackendResult<()> {
        let data = self.project_mut(project_id)?;
        data.recent.retain(|p| p != path);
        data.recent.insert(0, path.to_string());
        data.persist_recent()?;
        Ok(())
    }

    fn recent_remove(&mut self, project_id: &str, path: &str) -> BackendResult<String> {
        let data = self.project_mut(project_id)?;
        let before = data.recent.len();
        data.recent.retain(|p| p != path);
        if data.recent.len() == before {
            return Err(BackendError::NotFound(format!("recent path {path}")));
        }
        data.persist_recent()?;
        Ok(path.to_string())
    }

    fn recent_list(&mut self, project_id: &str) -> BackendResult<Vec<String>> {
        Ok(self.project_mut(project_id)?.recent.clone())
    }
}

// --------------------------------------------------------------------
// Source resolution
// --------------------------------------------------------------------

fn resolve_source_direct(source: &Value) -> BackendResult<ProducerSpec> {
    if let Some(path) = source.get("path").and_then(|v| v.as_str()) {
        return Ok(ProducerSpec::File { path: path.to_string() });
    }
    if let Some(frames) = source.get("blank").and_then(|v| v.as_i64()) {
        if frames <= 0 {
            return Err(BackendError::InvalidParams("blank spacer must have a positive frame count".into()));
        }
        return Ok(ProducerSpec::Blank { frames });
    }
    if source.get("xml").is_some() {
        return Err(BackendError::InvalidParams(
            "raw MLT producer XML sources are not supported by MltBackend (only {path} / {playlistIndex})".into(),
        ));
    }
    Err(BackendError::InvalidParams("source must be {path: ...} or {playlistIndex: ...}".into()))
}

fn resolve_source(data: &MltProjectData, source: &Value) -> BackendResult<ProducerSpec> {
    if let Some(idx) = source.get("playlistIndex").and_then(|v| v.as_u64()) {
        return data
            .bin_producers
            .get(&(idx as usize))
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("playlist index {idx}")));
    }
    resolve_source_direct(source)
}

/// `bin_producers` is keyed by playlist-bin index and must always stay
/// index-aligned with `playlist_bin` (invariant established by
/// `playlist_append`/`file_import`). `playlist.insert/remove/move` need to
/// shift that alignment the same way they shift `playlist_bin` itself --
/// round-tripping through a plain `Vec` (ordered 0..len) makes that a
/// single, obviously-correct `Vec` operation instead of separately-derived
/// index arithmetic on the `HashMap` for each of the three operations.
fn bin_producers_as_vec(data: &MltProjectData) -> Vec<ProducerSpec> {
    (0..data.playlist_bin.len())
        .map(|i| {
            data.bin_producers
                .get(&i)
                .cloned()
                .expect("bin_producers must be index-aligned with playlist_bin")
        })
        .collect()
}

fn set_bin_producers_from_vec(data: &mut MltProjectData, producers: Vec<ProducerSpec>) {
    data.bin_producers = producers.into_iter().enumerate().collect();
}

fn default_in_out(producer: &ProducerSpec) -> BackendResult<(i64, i64)> {
    match producer {
        ProducerSpec::File { path } => {
            let frames = probe_media(path)?.duration_frames;
            if frames <= 0 {
                return Err(BackendError::InvalidParams(format!("source {path} has zero frames")));
            }
            Ok((0, frames - 1))
        }
        ProducerSpec::Title { .. } => Ok((0, DEFAULT_TITLE_DURATION_FRAMES - 1)),
        ProducerSpec::Blank { frames } => Ok((0, frames - 1)),
    }
}

/// Maps the friendly/spec-level codec names agents naturally reach for
/// (matching `memory/head/gen/rust-fork/11-e2e-scenario-tests.md`'s own
/// `file.export({..., codec: "h264", ...})` example, and what a real
/// `claude -p` MCP agent supplied end-to-end when this normalization was
/// missing) to the real libavcodec encoder names `melt`'s `avformat`
/// consumer requires via `vcodec=...`. Without this, `melt` logs
/// "<name> unrecognised - ignoring" to stderr, silently drops the video
/// stream, and still exits 0 -- producing an audio-only "successful"
/// export. Unknown/already-correct values (e.g. an operator passing
/// "libx264" or "prores_ks" directly, as this crate's own tests do) pass
/// through unchanged.
fn normalize_vcodec(codec: &str) -> String {
    if codec.is_empty() {
        return "libx264".to_string();
    }
    match codec.to_ascii_lowercase().as_str() {
        "h264" | "avc" | "avc1" => "libx264".to_string(),
        "h265" | "hevc" => "libx265".to_string(),
        "vp8" => "libvpx".to_string(),
        "vp9" => "libvpx-vp9".to_string(),
        "av1" => "libaom-av1".to_string(),
        "prores" => "prores_ks".to_string(),
        _ => codec.to_string(),
    }
}

/// Defense-in-depth companion to `normalize_vcodec`: scans `melt`/libavcodec
/// stderr for the "<codec> unrecognised - ignoring" pattern that indicates a
/// stream was silently dropped despite a zero exit status, so `file_export`
/// can report the job as failed instead of "done" even for a codec value
/// this module doesn't already know how to normalize. Returns the offending
/// line if found.
fn detect_unrecognised_codec(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .find(|line| line.contains("unrecognised - ignoring") || line.contains("unrecognized - ignoring"))
}

fn resolve_melt_binary() -> String {
    if let Ok(p) = std::env::var("MELT_BIN") {
        return p;
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = format!("{home}/.local/bin/melt");
        if Path::new(&candidate).exists() {
            return candidate;
        }
    }
    "melt".to_string()
}

fn resolve_ffprobe_binary() -> String {
    std::env::var("FFPROBE_BIN").unwrap_or_else(|_| "ffprobe".to_string())
}

/// Real `ffprobe` invocation -- no guessed codecs or durations. Frame count
/// prefers `nb_frames` when the container reports it, else falls back to
/// `duration * DEFAULT_FPS` (accurate as long as the source's real frame rate
/// matches the project's fixed `DEFAULT_FPS`).
fn probe_media(path: &str) -> BackendResult<FileProbe> {
    let output = Command::new(resolve_ffprobe_binary())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_name,codec_type,nb_frames,duration",
            "-of",
            "json",
            path,
        ])
        .output()
        .map_err(|e| BackendError::InvalidParams(format!("failed to run ffprobe on {path}: {e}")))?;
    if !output.status.success() {
        return Err(BackendError::InvalidParams(format!(
            "ffprobe failed on {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| BackendError::InvalidParams(format!("bad ffprobe JSON for {path}: {e}")))?;
    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no streams for {path}")))?;
    let stream = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .or_else(|| streams.first())
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no streams for {path}")))?;
    let codec = stream
        .get("codec_name")
        .and_then(Value::as_str)
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no codec for {path}")))?;
    let duration_seconds = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| stream.get("duration").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()))
        .ok_or_else(|| BackendError::InvalidParams(format!("ffprobe returned no duration for {path}")))?;
    let duration_frames = stream
        .get("nb_frames")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|frames| *frames > 0)
        .unwrap_or_else(|| (duration_seconds * DEFAULT_FPS as f64).round() as i64);
    if duration_frames <= 0 || !duration_seconds.is_finite() || duration_seconds < 0.0 {
        return Err(BackendError::InvalidParams(format!("ffprobe returned an invalid duration for {path}")));
    }
    Ok(FileProbe {
        path: path.to_string(),
        duration_seconds,
        duration_frames,
        codec: codec.to_string(),
    })
}

fn resolve_output_path(root: &Path, output_path: &str, container: &str) -> PathBuf {
    let p = Path::new(output_path);
    let mut resolved = if p.is_absolute() { p.to_path_buf() } else { root.join("exports").join(output_path) };
    if resolved.extension().is_none() {
        resolved.set_extension(if container.is_empty() { "mp4" } else { container });
    }
    resolved
}

// --------------------------------------------------------------------
// MLT XML serialization
// --------------------------------------------------------------------

struct XmlCtx {
    /// Producer / nested-tractor XML fragments, in dependency order (every
    /// id referenced later is defined earlier, since MLT's XML parser
    /// resolves ids as it reads, not via a forward-reference pass).
    defs: Vec<String>,
    next_id: u64,
}

impl XmlCtx {
    fn alloc(&mut self, prefix: &str) -> String {
        let id = format!("{prefix}{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn emit_clip_producer(&mut self, clip: &MltClip) -> String {
        let id = self.alloc("producer");
        let mut xml = String::new();
        match &clip.producer {
            ProducerSpec::File { path } => {
                xml.push_str(&format!(
                    "  <producer id=\"{id}\" in=\"{}\" out=\"{}\">\n    <property name=\"resource\">{}</property>\n",
                    clip.in_frame,
                    clip.out_frame,
                    xml_escape(path)
                ));
            }
            ProducerSpec::Title { mode, text, bg, .. } => {
                xml.push_str(&format!(
                    "  <producer id=\"{id}\" in=\"{}\" out=\"{}\">\n    <property name=\"resource\">{}</property>\n    <property name=\"mlt_service\">color</property>\n",
                    clip.in_frame,
                    clip.out_frame,
                    xml_escape(bg)
                ));
                let fg = match &clip.producer {
                    ProducerSpec::Title { fg, .. } => fg.clone(),
                    _ => unreachable!(),
                };
                xml.push_str(&title_filter_xml(mode, text, &fg));
            }
            ProducerSpec::Blank { .. } => {
                // Fully transparent color producer -- a real MLT spacer
                // clip, not a semantic "no-op": qtblend composites it as
                // see-through, letting whatever's on the track(s) below
                // show through untouched (empirically verified, see the
                // module doc comment).
                xml.push_str(&format!(
                    "  <producer id=\"{id}\" in=\"{}\" out=\"{}\">\n    <property name=\"resource\">#00000000</property>\n    <property name=\"mlt_service\">color</property>\n",
                    clip.in_frame, clip.out_frame
                ));
            }
        }
        for filter in &clip.filters {
            xml.push_str(&filter_xml(filter));
        }
        xml.push_str("  </producer>\n");
        self.defs.push(xml);
        id
    }

    /// Emits a producer for the same source but a different in/out window
    /// (used for the trimmed tail/head clones a crossfade needs).
    fn emit_trimmed_clone(&mut self, clip: &MltClip, in_frame: i64, out_frame: i64) -> String {
        let mut cloned = clip.clone();
        cloned.in_frame = in_frame;
        cloned.out_frame = out_frame;
        self.emit_clip_producer(&cloned)
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn json_value_to_mlt_prop(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn mlt_prop_to_json(s: &str) -> Value {
    if let Ok(n) = s.parse::<i64>() {
        json!(n)
    } else if let Ok(n) = s.parse::<f64>() {
        json!(n)
    } else {
        json!(s)
    }
}

fn title_filter_xml(mode: &str, text: &str, fg: &str) -> String {
    if mode == "simple" {
        format!(
            "    <filter>\n      <property name=\"mlt_service\">dynamictext</property>\n      <property name=\"argument\">{}</property>\n      <property name=\"geometry\">0%,0%:100%x100%</property>\n      <property name=\"family\">Sans</property>\n      <property name=\"size\">48</property>\n      <property name=\"weight\">400</property>\n      <property name=\"fgcolour\">{}</property>\n      <property name=\"bgcolour\">#00000000</property>\n      <property name=\"halign\">center</property>\n      <property name=\"valign\">middle</property>\n    </filter>\n",
            xml_escape(text),
            xml_escape(fg)
        )
    } else {
        // richText / typewriter -> qtext with an html argument, per
        // TextProducerWidget::newProducer (01-jsonrpc-spec.md's generator.*
        // row).
        format!(
            "    <filter>\n      <property name=\"mlt_service\">qtext</property>\n      <property name=\"html\">{}</property>\n      <property name=\"geometry\">0%,0%:100%x100%</property>\n      <property name=\"fgcolour\">{}</property>\n    </filter>\n",
            xml_escape(text),
            xml_escape(fg)
        )
    }
}

fn filter_xml(filter: &MltFilter) -> String {
    let mut s = String::from("    <filter>\n");
    s.push_str(&format!("      <property name=\"mlt_service\">{}</property>\n", xml_escape(&filter.mlt_service)));
    for (k, v) in &filter.properties {
        if filter.keyframes.contains_key(k) {
            continue; // keyframed properties are emitted below instead of their static initial value
        }
        s.push_str(&format!("      <property name=\"{}\">{}</property>\n", xml_escape(k), xml_escape(v)));
    }
    for (prop, kfs) in &filter.keyframes {
        let mut sorted = kfs.clone();
        sorted.sort_by_key(|(p, _)| *p);
        let joined = sorted.into_iter().map(|(_, kf)| kf).collect::<Vec<_>>().join(";");
        s.push_str(&format!("      <property name=\"{}\">{}</property>\n", xml_escape(prop), xml_escape(&joined)));
    }
    s.push_str("    </filter>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use serde_json::json;

    #[test]
    fn filter_set_property_last_static_write_is_serialized() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-unit-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        backend.project_select("project").unwrap();
        backend.generator_create_title("project", json!({"text": "title"})).unwrap();
        backend.edit_add_track("project", "video").unwrap();
        let clip = backend.edit_append_clip("project", 0, json!({"playlistIndex": 0})).unwrap();
        backend.filter_add("project", &clip.clip_id, "brightness", json!({})).unwrap();
        backend.filter_set_property("project", &clip.clip_id, 0, "level", json!(0.25), None).unwrap();
        backend.filter_set_property("project", &clip.clip_id, 0, "level", json!(0.75), None).unwrap();
        backend.project_save("project").unwrap();

        let xml = std::fs::read_to_string(root.join("project/project.mlt")).unwrap();
        assert!(xml.contains("<property name=\"level\">0.75</property>"));
        assert!(!xml.contains("<property name=\"level\">0.25</property>"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn edit_split_clip_and_filter_lifecycle() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-split-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        backend.project_select("project").unwrap();
        backend.generator_create_title("project", json!({"text": "title"})).unwrap();
        backend.edit_add_track("project", "video").unwrap();
        let clip = backend.edit_append_clip("project", 0, json!({"playlistIndex": 0})).unwrap();
        // Title default is 0..=149; split at mid-clip.
        let split = backend.edit_split_clip("project", 0, 0, 75).unwrap();
        assert_eq!(split.left_clip_id, clip.clip_id);
        assert_eq!(split.left_index, 0);
        assert_eq!(split.right_index, 1);
        let clips = backend.edit_list_clips("project", 0).unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].out_frame, 74);
        assert_eq!(clips[1].in_frame, 75);
        assert_eq!(clips[1].out_frame, clip.out_frame);

        let left_id = split.left_clip_id;
        backend
            .filter_add("project", &left_id, "qtcrop", json!({"rect": "0 0 50 50"}))
            .unwrap();
        backend
            .filter_add("project", &left_id, "brightness", json!({"level": 0.5}))
            .unwrap();
        backend.filter_reorder("project", &left_id, 0, 1).unwrap();
        let listed = backend.filter_list("project", &left_id).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].mlt_service, "brightness");
        assert_eq!(listed[1].mlt_service, "qtcrop");

        backend
            .filter_add_keyframe("project", &left_id, 0, "level", 10, json!(0.2), "linear")
            .unwrap();
        backend
            .filter_add_keyframe("project", &left_id, 0, "level", 20, json!(0.9), "smooth")
            .unwrap();
        let kfs = backend.filter_list_keyframes("project", &left_id, 0, "level").unwrap();
        assert_eq!(kfs.len(), 2);
        assert_eq!(kfs[0].interpolation, "linear");
        assert_eq!(kfs[1].interpolation, "smooth");
        backend.filter_remove_keyframe("project", &left_id, 0, "level", 10).unwrap();
        assert_eq!(backend.filter_list_keyframes("project", &left_id, 0, "level").unwrap().len(), 1);

        backend.filter_remove("project", &left_id, 0).unwrap();
        let listed = backend.filter_list("project", &left_id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].mlt_service, "qtcrop");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fixed_root_writes_project_file_without_project_id_suffix() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-fixed-root-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new_fixed_root(&root);
        backend.project_select("bound-project").unwrap();
        backend.generator_create_title("bound-project", json!({"text": "title"})).unwrap();
        backend.edit_add_track("bound-project", "video").unwrap();
        backend
            .edit_append_clip("bound-project", 0, json!({"playlistIndex": 0}))
            .unwrap();
        backend.project_save("bound-project").unwrap();

        assert!(root.join("project.mlt").is_file());
        assert!(!root.join("bound-project/project.mlt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn new_keeps_multi_project_directory_layout() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-projects-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        backend.project_select("project-a").unwrap();
        backend.project_select("project-b").unwrap();

        assert!(root.join("project-a").is_dir());
        assert!(root.join("project-b").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parse_format_srt_round_trip_and_remove() {
        let raw = "1\n00:00:02,000 --> 00:00:03,000\nHighlight One\n\n2\n00:00:06,667 --> 00:00:07,667\nHighlight Two\n\n3\n00:00:10,000 --> 00:00:11,000\nThree\n\n";
        let cues = parse_srt(raw);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].text, "Highlight One");
        assert_eq!(cues[1].start, "00:00:06,667");
        assert_eq!(cues[2].text, "Three");

        let rewritten = remove_srt_cues(raw, &[1]).unwrap();
        let remaining = parse_srt(&rewritten);
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].text, "Highlight One");
        assert_eq!(remaining[1].text, "Three");
        // Re-numbered 1..N
        assert!(rewritten.starts_with("1\n"));
        assert!(rewritten.contains("\n2\n"));
        assert!(!rewritten.contains("Highlight Two"));

        assert!(remove_srt_cues(raw, &[99]).is_err());
    }

    #[test]
    fn subtitles_remove_import_export_round_trip() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-subs-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        backend.project_select("project").unwrap();
        backend.subtitles_add_track("project").unwrap();
        backend
            .subtitles_append_item("project", 0, 60, 90, "One")
            .unwrap();
        backend
            .subtitles_append_item("project", 0, 200, 230, "Two")
            .unwrap();
        backend
            .subtitles_append_item("project", 0, 300, 330, "Three")
            .unwrap();

        backend.subtitles_remove_items("project", 0, &[1]).unwrap();
        let track_path = root.join("project/subtitles/track0.srt");
        let after_remove = fs::read_to_string(&track_path).unwrap();
        let cues = parse_srt(&after_remove);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[1].text, "Three");

        let external = root.join("external.srt");
        fs::write(
            &external,
            "1\n00:00:01,000 --> 00:00:02,000\nImported\n\n",
        )
        .unwrap();
        let info = backend
            .subtitles_import_srt("project", external.to_str().unwrap(), true)
            .unwrap();
        assert_eq!(info.track_index, 1);

        let export_path = root.join("out.srt");
        let exported = backend
            .subtitles_export_srt("project", export_path.to_str().unwrap(), 1)
            .unwrap();
        assert_eq!(exported, export_path.to_string_lossy());
        let exported_cues = parse_srt(&fs::read_to_string(&export_path).unwrap());
        assert_eq!(exported_cues.len(), 1);
        assert_eq!(exported_cues[0].text, "Imported");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn jobs_stop_marks_unknown_as_not_found() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-jobs-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        let err = backend.jobs_stop("no-such-job").unwrap_err();
        match err {
            BackendError::NotFound(_) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// Proof for playlist.insert/remove/move/get, mirroring the existing
    /// `edit_split_clip_and_filter_lifecycle` style: real `MltBackend`
    /// state, no real media files needed (blank spacers are real MLT
    /// producers too, just transparent ones -- see the module doc comment's
    /// "Mid-timeline positioning" note), which also exercises that
    /// `bin_producers` stays correctly index-aligned with `playlist_bin`
    /// across all three mutations (a later `edit.appendClip` by
    /// `playlistIndex` would silently resolve the wrong producer if it
    /// didn't).
    #[test]
    fn playlist_insert_remove_move_get_keep_bin_producers_aligned() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-playlist-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        backend.project_select("project").unwrap();

        backend.playlist_append("project", json!({"blank": 10}), Some("a".into())).unwrap();
        backend.playlist_append("project", json!({"blank": 30}), Some("c".into())).unwrap();
        let inserted = backend
            .playlist_insert("project", 1, json!({"blank": 20}), Some("b".into()))
            .unwrap();
        assert_eq!(inserted.index, 1);
        assert_eq!(inserted.duration_frames, 20);
        let names: Vec<String> = backend.playlist_list("project").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);

        // playlist.get: full metadata, probe is honestly None for a blank
        // spacer (nothing real to ffprobe).
        let got = backend.playlist_get("project", 1).unwrap();
        assert_eq!(got.name, "b");
        assert_eq!(got.duration_frames, 20);
        assert!(got.probe.is_none());
        assert!(backend.playlist_get("project", 99).is_err());

        // bin_producers alignment check: edit.appendClip by playlistIndex 1
        // ("b", 20 frames) must resolve to the right producer post-insert.
        backend.edit_add_track("project", "video").unwrap();
        let clip = backend.edit_append_clip("project", 0, json!({"playlistIndex": 1})).unwrap();
        assert_eq!(clip.out_frame - clip.in_frame + 1, 20, "clip appended via playlistIndex 1 should resolve to entry \"b\" (20 frames), not a stale producer");

        // Move "c" (index 2) to the front; bin_producers must move with it.
        backend.playlist_move("project", 2, 0).unwrap();
        let names: Vec<String> = backend.playlist_list("project").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["c", "a", "b"]);
        let clip = backend.edit_append_clip("project", 0, json!({"playlistIndex": 0})).unwrap();
        assert_eq!(clip.out_frame - clip.in_frame + 1, 30, "playlistIndex 0 should now resolve to \"c\" (30 frames) after the move");

        // Remove "a" (now index 1); reindexing + producer alignment again.
        backend.playlist_remove("project", 1).unwrap();
        let names: Vec<String> = backend.playlist_list("project").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["c", "b"]);
        let clip = backend.edit_append_clip("project", 0, json!({"playlistIndex": 1})).unwrap();
        assert_eq!(clip.out_frame - clip.in_frame + 1, 20, "playlistIndex 1 should now resolve to \"b\" (20 frames) after the remove");

        assert!(backend.playlist_remove("project", 99).is_err());
        assert!(backend.playlist_move("project", 0, 99).is_err());
        assert!(backend.playlist_insert("project", 99, json!({"blank": 5}), None).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Proof for testing-plan.md Phase 3's `recent.*` row against the real
    /// (`MltBackend`, disk-persisted) implementor, not just `MockBackend`.
    #[test]
    fn project_select_adds_the_project_to_its_own_recent_list() {
        let root = std::env::temp_dir().join(format!("sap-rust-mlt-recent-{}", uuid::Uuid::new_v4()));
        let mut backend = MltBackend::new(&root);
        assert!(backend.recent_list("proj").unwrap().is_empty());

        backend.project_select("proj").unwrap();
        assert_eq!(backend.recent_list("proj").unwrap(), vec!["proj".to_string()]);

        backend.project_select("proj").unwrap();
        assert_eq!(backend.recent_list("proj").unwrap(), vec!["proj".to_string()]);

        // Persisted to disk too (same `.snapshot/recent.json` `recent_add`
        // itself already persists to), not just held in memory.
        let persisted = std::fs::read_to_string(root.join("proj/.snapshot/recent.json")).unwrap();
        assert!(persisted.contains("proj"));
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Builds one track's `<playlist>` element, splicing in a nested-tractor
/// crossfade (real `luma` + `mix` MLT transitions) wherever
/// `transitions.addCrossfade` was called for that track. See the module
/// doc comment for exactly what's simplified here relative to real
/// Shotcut's `MultitrackModel::addTransition`.
/// Builds one track's `<playlist>` element, splicing in nested-tractor
/// crossfades (real `luma` + `mix` MLT transitions) wherever
/// `transitions.addCrossfade` was called for that track.
///
/// Rewritten (was originally single-crossfade-per-pass, which silently
/// dropped any second crossfade whose `betweenClips.0` was the clip index
/// just consumed by the first -- i.e. it could not handle two *chained*
/// crossfades sharing a middle clip, e.g. `(0,1)` and `(1,2)` on the same
/// track, exactly the case `11-e2e-scenario-tests.md`'s Phase A exercises
/// with three highlight segments and two crossfades). The fix: instead of
/// walking clip-pairs and jumping the cursor past a consumed pair, compute
/// each clip's `(head_overlap, tail_overlap)` independently up front (a
/// clip can have both -- a head overlap from the crossfade before it and a
/// tail overlap from the crossfade after it, at the same time), then emit
/// every clip's own untouched "middle" portion plus one mix-tractor per
/// tail overlap, walking the clip list exactly once, in order.
fn build_track_playlist(ctx: &mut XmlCtx, clips: &[MltClip], crossfades: &[CrossfadeRecord]) -> (String, String) {
    let playlist_id = ctx.alloc("playlist");
    let mut body = format!("  <playlist id=\"{playlist_id}\">\n");

    let mut head_overlap = vec![0i64; clips.len()];
    let mut tail_overlap = vec![0i64; clips.len()];
    for cf in crossfades {
        let (a, b) = cf.between_clips;
        if a >= clips.len() || b >= clips.len() {
            continue; // validated in transitions_add_crossfade; defensive only
        }
        let clip_a_len = clips[a].out_frame - clips[a].in_frame + 1;
        let clip_b_len = clips[b].out_frame - clips[b].in_frame + 1;
        let d = cf.duration_frames.min(clip_a_len).min(clip_b_len).max(0);
        tail_overlap[a] = d;
        head_overlap[b] = d;
    }

    for i in 0..clips.len() {
        let clip = &clips[i];
        let head = head_overlap[i];
        let tail = tail_overlap[i];
        let mid_in = clip.in_frame + head;
        let mid_out = clip.out_frame - tail;

        // This clip's own untouched middle (excludes the head frames
        // already emitted as part of the *previous* clip's tail mix-
        // tractor below, and excludes the tail frames about to be emitted
        // as part of *this* clip's own tail mix-tractor).
        if mid_in <= mid_out {
            let pid = ctx.emit_trimmed_clone(clip, mid_in, mid_out);
            body.push_str(&format!("    <entry producer=\"{pid}\" in=\"{mid_in}\" out=\"{mid_out}\"/>\n"));
        }

        if tail > 0 && i + 1 < clips.len() {
            let next = &clips[i + 1];
            let tail_id = ctx.emit_trimmed_clone(clip, clip.out_frame - tail + 1, clip.out_frame);
            let head_id = ctx.emit_trimmed_clone(next, next.in_frame, next.in_frame + tail - 1);
            let mix_id = ctx.alloc("tractor");
            let mix_xml = format!(
                "  <tractor id=\"{mix_id}\" in=\"0\" out=\"{last}\">\n    <track producer=\"{tail_id}\"/>\n    <track producer=\"{head_id}\"/>\n    <transition>\n      <property name=\"mlt_service\">luma</property>\n      <property name=\"a_track\">0</property>\n      <property name=\"b_track\">1</property>\n    </transition>\n    <transition>\n      <property name=\"mlt_service\">mix</property>\n      <property name=\"a_track\">0</property>\n      <property name=\"b_track\">1</property>\n      <property name=\"combine\">1</property>\n    </transition>\n  </tractor>\n",
                last = tail - 1
            );
            ctx.defs.push(mix_xml);
            body.push_str(&format!("    <entry producer=\"{mix_id}\" in=\"0\" out=\"{}\"/>\n", tail - 1));
        }
    }

    body.push_str("  </playlist>\n");
    (body, playlist_id)
}

/// Serializes the full project state to MLT XML: producers per clip
/// (including nested title filters and attached filter chains), one
/// `<playlist>` per track (with crossfade splicing), combined by a single
/// `<tractor>`, per doc 09's `<projectRoot>/project.mlt` convention.
fn build_mlt_xml(data: &MltProjectData) -> BackendResult<String> {
    if data.tracks.is_empty() {
        return Err(BackendError::InvalidParams("cannot export a project with no tracks".into()));
    }

    let mut ctx = XmlCtx { defs: Vec::new(), next_id: 0 };
    let mut playlists = String::new();
    let mut track_refs: Vec<(String, String)> = Vec::new();

    for (track_index, track) in data.tracks.iter().enumerate() {
        let empty = Vec::new();
        let clips = data.clips.get(&track_index).unwrap_or(&empty);
        if clips.is_empty() {
            let pid = ctx.alloc("playlist");
            playlists.push_str(&format!("  <playlist id=\"{pid}\"/>\n"));
            track_refs.push((pid, track.kind.clone()));
            continue;
        }
        let empty_transitions = Vec::new();
        let crossfades = data.transitions.get(&track_index).unwrap_or(&empty_transitions);
        let (xml, pid) = build_track_playlist(&mut ctx, clips, crossfades);
        playlists.push_str(&xml);
        track_refs.push((pid, track.kind.clone()));
    }

    let tractor_id = ctx.alloc("tractor");
    let mut tractor = format!("  <tractor id=\"{tractor_id}\" title=\"project\">\n");
    for (pid, kind) in &track_refs {
        if kind == "audio" {
            tractor.push_str(&format!("    <track producer=\"{pid}\" hide=\"video\"/>\n"));
        } else {
            tractor.push_str(&format!("    <track producer=\"{pid}\"/>\n"));
        }
    }

    // Real multi-track video compositing: plant a `qtblend` transition
    // between every pair of consecutive video tracks, bottom-up (each new
    // video track composites over the highest video track added before
    // it) -- the same real primitive and ordering
    // `MultitrackModel::getVideoBlendTransition`/`addVideoTrack` in real
    // Shotcut's `multitrackmodel.cpp` uses, empirically verified against
    // the installed `melt` (see module doc comment). `a_track`/`b_track`
    // are the 0-based positions of the `<track>` elements just emitted
    // above, which match `track_refs`'/`data.tracks`' indices exactly
    // since every track gets exactly one `<track>` entry, in order.
    let mut last_video_index: Option<usize> = None;
    for (index, (_, kind)) in track_refs.iter().enumerate() {
        if kind != "audio" {
            if let Some(prev) = last_video_index {
                tractor.push_str(&format!(
                    "    <transition>\n      <property name=\"mlt_service\">qtblend</property>\n      <property name=\"a_track\">{prev}</property>\n      <property name=\"b_track\">{index}</property>\n    </transition>\n"
                ));
            }
            last_video_index = Some(index);
        }
    }

    // Real subtitle burn-in: one `avfilter.subtitles` filter per subtitle
    // track, attached at the tractor level (post-composite, over whatever
    // the tracks above produced) -- see the module doc comment for why
    // this is the mechanism that actually works via `melt` CLI, not
    // Shotcut's own player-only `subtitle_feed`.
    for path in &data.subtitle_tracks {
        tractor.push_str(&format!(
            "    <filter>\n      <property name=\"mlt_service\">avfilter.subtitles</property>\n      <property name=\"av.filename\">{}</property>\n    </filter>\n",
            xml_escape(&path.to_string_lossy())
        ));
    }

    tractor.push_str("  </tractor>\n");

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!("<mlt LC_NUMERIC=\"C\" version=\"7.0.0\" title=\"project\" producer=\"{tractor_id}\">\n"));
    out.push_str(&format!(
        "  <profile description=\"HD 720p {fps} fps\" width=\"1280\" height=\"720\" progressive=\"1\" sample_aspect_num=\"1\" sample_aspect_den=\"1\" display_aspect_num=\"16\" display_aspect_den=\"9\" frame_rate_num=\"{fps}\" frame_rate_den=\"1\" colorspace=\"709\"/>\n",
        fps = DEFAULT_FPS
    ));
    for def in &ctx.defs {
        out.push_str(def);
    }
    out.push_str(&playlists);
    out.push_str(&tractor);
    out.push_str("</mlt>\n");
    Ok(out)
}

/// One SRT cue after parse (0-based cue order matches append order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtCue {
    pub start: String,
    pub end: String,
    pub text: String,
}

/// Parse SubRip content into ordered cues. Cue index numbers in the file are
/// ignored; order is document order (same as `subtitles.appendItem` order).
pub fn parse_srt(content: &str) -> Vec<SrtCue> {
    let mut cues = Vec::new();
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
        let Some((start, end)) = timing.split_once("-->") else {
            continue;
        };
        let start = start.trim().to_string();
        let end = end.trim().to_string();
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
        cues.push(SrtCue {
            start,
            end,
            text: text_lines.join("\n"),
        });
    }
    cues
}

/// Serialize cues with contiguous 1-based numbering.
pub fn format_srt(cues: &[SrtCue]) -> String {
    let mut out = String::new();
    for (i, cue) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            cue.start,
            cue.end,
            cue.text
        ));
    }
    out
}

/// Remove cues by 0-based indices and re-number the remainder.
pub fn remove_srt_cues(content: &str, item_indices: &[usize]) -> Result<String, String> {
    let mut cues = parse_srt(content);
    let mut remove: Vec<usize> = item_indices.to_vec();
    remove.sort_unstable();
    remove.dedup();
    for &idx in remove.iter().rev() {
        if idx >= cues.len() {
            return Err(format!(
                "subtitle item index {idx} out of range (len {})",
                cues.len()
            ));
        }
        cues.remove(idx);
    }
    Ok(format_srt(&cues))
}

fn frames_to_srt_timestamp(frame: i64, fps: i64) -> String {
    let total_ms = (frame.max(0) as f64 / fps as f64 * 1000.0).round() as i64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

/// Minimal base64 encoder (no external dependency) for
/// `playback_get_frame`'s returned image bytes.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}
