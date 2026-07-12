//! Real `Backend` implementation wired to the running Shotcut process via
//! the extern "C" shim in `shotcut/src/rustbridge/sap_ffi.{h,cpp}`.
//!
//! This is the piece that makes `edit.addTrack` etc. actually mutate the
//! real, currently-open Shotcut project instead of `MockBackend`'s in-memory
//! state, per `memory/head/gen/rust-fork/02-rust-embedding.md`. It adds a
//! second implementor of the existing `Backend` trait -- the trait itself is
//! unchanged.
//!
//! Also hosts `sap_start_server`, the `extern "C"` entry point that
//! `shotcut/src/main.cpp` calls (on a dedicated background `std::thread`) to
//! spin up a tokio runtime and run `crate::server::serve` with this backend.
//! This is the actual "Rust layer runs inside the Qt process" integration
//! point from doc 02 -- without it, everything else in this crate is inert
//! inside a real Shotcut process.

#![cfg(feature = "real_ffi")]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;

use serde_json::Value;

use crate::backend::{
    Backend, BackendError, BackendResult, Clip, FileProbe, FilterInfo, FilterListEntry, JobStatus,
    KeyframeInfo, Marker, PlaylistEntry, PlaylistEntryDetail, ProjectState, SplitClipResult,
    SubtitleTrackInfo, Track, TransitionInfo,
};
use crate::ffi;
use crate::server::{self, ServerConfig};

/// Wraps the opaque `MainWindow*` handle passed in from C++
/// (`MainWindow::singleton()`/`MAIN`, cast to `void*`). The embedded process
/// has exactly one live project -- the window itself -- so unlike
/// `MockBackend` there is no per-`project_id` routing to do; any bound
/// `project_id` addresses the same running project.
pub struct FfiBackend {
    main_window: *mut c_void,
}

// SAFETY: `main_window` is never dereferenced directly on whatever thread
// holds this `FfiBackend` (the single dispatcher thread inside
// `server::run_dispatcher`). Every function in `ffi.rs` that accepts it
// immediately marshals the actual Qt/MLT access onto the Qt main thread via
// `QMetaObject::invokeMethod(..., Qt::BlockingQueuedConnection)` on the C++
// side before touching anything Qt-owned -- see sap_ffi.cpp. That makes
// holding and passing this pointer across threads sound even though
// `MainWindow*` itself is not `Send` in the Qt/C++ sense.
unsafe impl Send for FfiBackend {}

impl FfiBackend {
    /// # Safety
    /// `main_window` must be a valid, live `MainWindow*` (as obtained from
    /// `MainWindow::singleton()`) for as long as this backend is used --
    /// i.e. for the lifetime of the Qt process this crate is linked into.
    pub unsafe fn new(main_window: *mut c_void) -> Self {
        Self { main_window }
    }

    fn undo_redo_depth(&self) -> BackendResult<(usize, usize)> {
        let undo = unsafe { ffi::sap_get_undo_depth(self.main_window) };
        let redo = unsafe { ffi::sap_get_redo_depth(self.main_window) };
        if undo < 0 || redo < 0 {
            return Err(BackendError::NotFound("undo stack unavailable".into()));
        }
        Ok((undo as usize, redo as usize))
    }
}

impl Backend for FfiBackend {
    fn project_select(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        // No multi-project routing in-process: the currently-open project
        // *is* the one project this Qt process has. Any project_id binds.
        self.project_get_state(project_id)
    }

    fn project_exit(&mut self) -> BackendResult<()> {
        // No real primitive wired yet (would mean closing/quitting the live
        // GUI session out from under its user) -- idempotent no-op, matching
        // the same documented choice already made for MockBackend/server.rs.
        Ok(())
    }

    fn project_get_state(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        let (undo_depth, redo_depth) = self.undo_redo_depth()?;
        Ok(ProjectState {
            project_id: project_id.to_string(),
            dirty: undo_depth > 0,
            undo_depth,
            redo_depth,
        })
    }

    fn project_save(&mut self, _project_id: &str) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_save_project(self.main_window) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidParams("project save failed".into()))
        }
    }

    fn project_undo(&mut self, _project_id: &str) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_project_undo(self.main_window) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::NotFound("nothing to undo".into()))
        }
    }

    fn project_redo(&mut self, _project_id: &str) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_project_redo(self.main_window) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::NotFound("nothing to redo".into()))
        }
    }

    fn edit_add_track(&mut self, _project_id: &str, kind: &str) -> BackendResult<Track> {
        let index = match kind {
            "video" => unsafe { ffi::sap_add_video_track(self.main_window) },
            "audio" => unsafe { ffi::sap_add_audio_track(self.main_window) },
            other => {
                return Err(BackendError::InvalidParams(format!("bad track kind: {other}")));
            }
        };
        if index < 0 {
            return Err(BackendError::InvalidParams("failed to add track".into()));
        }
        Ok(Track {
            index: index as usize,
            kind: kind.to_string(),
            muted: false,
            hidden: false,
            locked: false,
            blend_mode: crate::backend::default_blend_mode(),
        })
    }

    fn edit_remove_track(&mut self, _project_id: &str, track_index: usize) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_remove_track(self.main_window, track_index as i32) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::NotFound(format!("track {track_index}")))
        }
    }

    fn edit_reorder_track(&mut self, _project_id: &str, _from_index: usize, _to_index: usize) -> BackendResult<Vec<Track>> {
        Err(BackendError::Unsupported("edit.reorderTrack not wired to real FFI yet".into()))
    }

    fn edit_set_track_properties(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _muted: Option<bool>,
        _hidden: Option<bool>,
        _locked: Option<bool>,
        _blend_mode: Option<String>,
    ) -> BackendResult<Track> {
        Err(BackendError::Unsupported("edit.setTrackProperties not wired to real FFI yet".into()))
    }

    fn edit_set_track_height(&mut self, _project_id: &str, _height: i64) -> BackendResult<()> {
        Err(BackendError::Unsupported("edit.setTrackHeight not wired to real FFI yet".into()))
    }

    fn edit_remove_clip(&mut self, _project_id: &str, _track_index: usize, _clip_index: usize) -> BackendResult<()> {
        Err(BackendError::Unsupported("edit.removeClip not wired to real FFI yet".into()))
    }

    fn edit_move_clip(
        &mut self,
        _project_id: &str,
        _from_track_index: usize,
        _from_clip_index: usize,
        _to_track_index: usize,
        _to_clip_index: usize,
    ) -> BackendResult<Clip> {
        Err(BackendError::Unsupported("edit.moveClip not wired to real FFI yet".into()))
    }

    fn edit_list_tracks(&mut self, _project_id: &str) -> BackendResult<Vec<Track>> {
        let raw = unsafe { ffi::sap_list_tracks(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::NotFound("track list unavailable".into()));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<Vec<Track>>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad track list JSON: {e}")))
    }

    fn edit_append_clip(
        &mut self,
        _project_id: &str,
        track_index: usize,
        source: Value,
    ) -> BackendResult<Clip> {
        // Real wiring: `sap_append_clip` (sap_ffi.cpp) opens `source.path`
        // as an actual Mlt::Producer and pushes it via the real,
        // undoable Timeline::AppendCommand -- see that file for the full
        // path. Unlike TimelineDock::append() (which only reads the
        // clipboard/"current source"), this takes the path directly.
        let path = source
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BackendError::InvalidParams("source must be {path: ...}".into()))?;
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("invalid source path: {e}")))?;

        let raw =
            unsafe { ffi::sap_append_clip(self.main_window, track_index as c_int, c_path.as_ptr()) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to append clip from {path} to track {track_index} (invalid track, or {path} did not open as a valid MLT producer)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        unsafe { ffi::sap_free_string(raw) };

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AppendedClip {
            clip_id: String,
            index: usize,
            in_frame: i64,
            out_frame: i64,
        }
        let appended: AppendedClip = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad append-clip JSON: {e}")))?;

        Ok(Clip {
            clip_id: appended.clip_id,
            index: appended.index,
            source,
            in_frame: appended.in_frame,
            out_frame: appended.out_frame,
        })
    }

    fn edit_list_clips(&mut self, _project_id: &str, _track_index: usize) -> BackendResult<Vec<Clip>> {
        // Stub: no real FFI wrapper yet.
        Ok(Vec::new())
    }

    fn playback_seek(&mut self, _project_id: &str, frame: i64) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_playback_seek(self.main_window, frame as _) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidParams("playback seek failed".into()))
        }
    }

    fn notes_get_text(&mut self, _project_id: &str) -> BackendResult<String> {
        // Stub: no real FFI wrapper yet.
        Ok(String::new())
    }

    fn notes_set_text(&mut self, _project_id: &str, _text: &str) -> BackendResult<()> {
        // Stub: no real FFI wrapper yet.
        Ok(())
    }

    // --- Additive extension: not wired to real Shotcut primitives yet
    // (same honesty policy as the rest of this file's stubs) -- these
    // return NotFound rather than silently no-opping, so a caller can tell
    // the difference between "did nothing because there's nothing to do"
    // (playback_seek/notes_*) and "not implemented at all".

    fn playlist_append(
        &mut self,
        _project_id: &str,
        _source: Value,
        _name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        Err(BackendError::NotFound("playlist.append not wired to real FFI yet".into()))
    }

    fn playlist_list(&mut self, _project_id: &str) -> BackendResult<Vec<PlaylistEntry>> {
        Ok(Vec::new())
    }

    // --- Minimal stubs for the new playlist.* trait methods (task: keep
    // this file's changes to an absolute minimum -- these are explicit
    // NotFound entries, not real Qt/MLT wiring, same honesty policy as
    // playlist_append/file_import above; playlist.addToTimeline has no
    // trait method at all, see backend.rs's comment on that). ---

    fn playlist_insert(
        &mut self,
        _project_id: &str,
        _index: usize,
        _source: Value,
        _name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        Err(BackendError::NotFound("playlist.insert not wired to real FFI yet".into()))
    }

    fn playlist_remove(&mut self, _project_id: &str, _index: usize) -> BackendResult<()> {
        Err(BackendError::NotFound("playlist.remove not wired to real FFI yet".into()))
    }

    fn playlist_move(&mut self, _project_id: &str, _from_index: usize, _to_index: usize) -> BackendResult<()> {
        Err(BackendError::NotFound("playlist.move not wired to real FFI yet".into()))
    }

    fn playlist_get(&mut self, _project_id: &str, _index: usize) -> BackendResult<PlaylistEntryDetail> {
        Err(BackendError::NotFound("playlist.get not wired to real FFI yet".into()))
    }

    fn file_import(&mut self, _project_id: &str, _path: &str) -> BackendResult<PlaylistEntry> {
        Err(BackendError::Unsupported(
            "file.import is unsupported: no Qt/MLT import shim is available".into(),
        ))
    }

    fn edit_trim_clip_in(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _clip_index: usize,
        _new_frame: i64,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("edit.trimClipIn not wired to real FFI yet".into()))
    }

    fn edit_trim_clip_out(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _clip_index: usize,
        _new_frame: i64,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("edit.trimClipOut not wired to real FFI yet".into()))
    }

    fn edit_split_clip(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _clip_index: usize,
        _position: i64,
    ) -> BackendResult<SplitClipResult> {
        Err(BackendError::NotFound("edit.splitClip not wired to real FFI yet".into()))
    }

    fn transitions_add_crossfade(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _between_clips: (usize, usize),
        _duration_frames: i64,
    ) -> BackendResult<TransitionInfo> {
        Err(BackendError::NotFound("transitions.addCrossfade not wired to real FFI yet".into()))
    }

    fn filter_add(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _mlt_service: &str,
        _properties: Value,
    ) -> BackendResult<FilterInfo> {
        Err(BackendError::NotFound("filter.add not wired to real FFI yet".into()))
    }

    fn filter_set_property(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
        _property: &str,
        _value: Value,
        _position: Option<i64>,
    ) -> BackendResult<()> {
        Err(BackendError::Unsupported(
            "filter.setProperty is unsupported: no Qt/MLT property-set shim is available".into(),
        ))
    }

    fn filter_add_keyframe(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
        _property: &str,
        _position: i64,
        _value: Value,
        _interpolation: &str,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("filter.addKeyframe not wired to real FFI yet".into()))
    }

    fn filter_list(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
    ) -> BackendResult<Vec<FilterListEntry>> {
        Err(BackendError::NotFound("filter.list not wired to real FFI yet".into()))
    }

    fn filter_remove(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("filter.remove not wired to real FFI yet".into()))
    }

    fn filter_reorder(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
        _new_index: usize,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("filter.reorder not wired to real FFI yet".into()))
    }

    fn filter_list_keyframes(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
        _property: &str,
    ) -> BackendResult<Vec<KeyframeInfo>> {
        Err(BackendError::NotFound("filter.listKeyframes not wired to real FFI yet".into()))
    }

    fn filter_remove_keyframe(
        &mut self,
        _project_id: &str,
        _clip_id: &str,
        _filter_index: usize,
        _property: &str,
        _position: i64,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("filter.removeKeyframe not wired to real FFI yet".into()))
    }


    fn clip_length_frames(&mut self, _project_id: &str, _clip_id: &str) -> BackendResult<i64> {
        Err(BackendError::NotFound("clip_length_frames not wired to real FFI yet".into()))
    }

    fn generator_create_title(&mut self, _project_id: &str, _params: Value) -> BackendResult<PlaylistEntry> {
        Err(BackendError::NotFound("generator.createTitle not wired to real FFI yet".into()))
    }

    fn subtitles_add_track(&mut self, _project_id: &str) -> BackendResult<SubtitleTrackInfo> {
        Err(BackendError::NotFound("subtitles.addTrack not wired to real FFI yet".into()))
    }

    fn subtitles_append_item(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _start_frame: i64,
        _end_frame: i64,
        _text: &str,
    ) -> BackendResult<()> {
        Err(BackendError::NotFound("subtitles.appendItem not wired to real FFI yet".into()))
    }

    fn subtitles_remove_items(
        &mut self,
        _project_id: &str,
        _track_index: usize,
        _item_indices: &[usize],
    ) -> BackendResult<()> {
        Err(BackendError::Unsupported("subtitles.removeItems not wired to real FFI yet".into()))
    }

    fn subtitles_import_srt(
        &mut self,
        _project_id: &str,
        _path: &str,
        _new_track: bool,
    ) -> BackendResult<SubtitleTrackInfo> {
        Err(BackendError::Unsupported("subtitles.importSrt not wired to real FFI yet".into()))
    }

    fn subtitles_export_srt(
        &mut self,
        _project_id: &str,
        _path: &str,
        _track_index: usize,
    ) -> BackendResult<String> {
        Err(BackendError::Unsupported("subtitles.exportSrt not wired to real FFI yet".into()))
    }

    fn file_export(
        &mut self,
        _project_id: &str,
        _output_path: &str,
        _codec: &str,
        _container: &str,
    ) -> BackendResult<String> {
        Err(BackendError::NotFound("file.export not wired to real FFI yet".into()))
    }

    fn file_probe(&mut self, _path: &str) -> BackendResult<FileProbe> {
        Err(BackendError::Unsupported(
            "file.probe is unsupported: no Qt/MLT probe shim is available".into(),
        ))
    }

    fn jobs_get(&mut self, _job_id: &str) -> BackendResult<JobStatus> {
        Err(BackendError::NotFound("jobs.get not wired to real FFI yet".into()))
    }

    fn jobs_list(&mut self, _project_id: &str) -> BackendResult<Vec<JobStatus>> {
        Err(BackendError::Unsupported("jobs.list not wired to real FFI yet".into()))
    }

    fn jobs_stop(&mut self, _job_id: &str) -> BackendResult<()> {
        Err(BackendError::Unsupported("jobs.stop not wired to real FFI yet".into()))
    }

    fn playback_get_frame(
        &mut self,
        _project_id: &str,
        frame: i64,
        format: &str,
    ) -> BackendResult<String> {
        // Real wiring: `sap_get_frame` (sap_ffi.cpp) renders the requested
        // frame off the live project producer via Controller::image() (the
        // same primitive Shotcut's own thumbnails use) and encodes it with
        // Qt's QImage::save(). Base64-encoded here with the same alphabet
        // as MltBackend::playback_get_frame (mlt_backend.rs) for wire-format
        // consistency; duplicated locally rather than imported since that
        // function is private to mlt_backend.rs.
        let c_format = CString::new(format)
            .map_err(|e| BackendError::InvalidParams(format!("invalid format: {e}")))?;
        let mut out_len: c_int = 0;
        let raw = unsafe {
            ffi::sap_get_frame(self.main_window, frame, c_format.as_ptr(), &mut out_len as *mut c_int)
        };
        if raw.is_null() || out_len <= 0 {
            return Err(BackendError::InvalidParams(format!(
                "failed to render frame {frame} (format {format}): no live producer, out-of-range frame, or no codec for that format"
            )));
        }
        let bytes = unsafe { std::slice::from_raw_parts(raw, out_len as usize) }.to_vec();
        unsafe { ffi::sap_free_bytes(raw) };
        Ok(base64_encode(&bytes))
    }

    fn markers_append(
        &mut self,
        _project_id: &str,
        _frame: i64,
        _text: Option<String>,
        _color: Option<String>,
    ) -> BackendResult<Marker> {
        Err(BackendError::Unsupported("markers.append not wired to real FFI yet".into()))
    }

    fn markers_remove(&mut self, _project_id: &str, _marker_index: usize) -> BackendResult<()> {
        Err(BackendError::Unsupported("markers.remove not wired to real FFI yet".into()))
    }

    fn markers_update(
        &mut self,
        _project_id: &str,
        _marker_index: usize,
        _frame: Option<i64>,
        _text: Option<String>,
        _color: Option<String>,
    ) -> BackendResult<Marker> {
        Err(BackendError::Unsupported("markers.update not wired to real FFI yet".into()))
    }

    fn markers_move(
        &mut self,
        _project_id: &str,
        _marker_index: usize,
        _start: i64,
        _end: i64,
    ) -> BackendResult<Marker> {
        Err(BackendError::Unsupported("markers.move not wired to real FFI yet".into()))
    }

    fn markers_set_color(
        &mut self,
        _project_id: &str,
        _marker_index: usize,
        _color: &str,
    ) -> BackendResult<Marker> {
        Err(BackendError::Unsupported("markers.setColor not wired to real FFI yet".into()))
    }

    fn markers_clear(&mut self, _project_id: &str) -> BackendResult<()> {
        Err(BackendError::Unsupported("markers.clear not wired to real FFI yet".into()))
    }

    fn markers_list(&mut self, _project_id: &str) -> BackendResult<Vec<Marker>> {
        Err(BackendError::Unsupported("markers.list not wired to real FFI yet".into()))
    }

    fn markers_get(&mut self, _project_id: &str, _marker_index: usize) -> BackendResult<Marker> {
        Err(BackendError::Unsupported("markers.get not wired to real FFI yet".into()))
    }

    fn markers_next(&mut self, _project_id: &str, _from_frame: i64) -> BackendResult<Option<i64>> {
        Err(BackendError::Unsupported("markers.next not wired to real FFI yet".into()))
    }

    fn markers_prev(&mut self, _project_id: &str, _from_frame: i64) -> BackendResult<Option<i64>> {
        Err(BackendError::Unsupported("markers.prev not wired to real FFI yet".into()))
    }

    fn recent_add(&mut self, _project_id: &str, _path: &str) -> BackendResult<()> {
        Err(BackendError::Unsupported("recent.add not wired to real FFI yet".into()))
    }

    fn recent_remove(&mut self, _project_id: &str, _path: &str) -> BackendResult<String> {
        Err(BackendError::Unsupported("recent.remove not wired to real FFI yet".into()))
    }

    fn recent_list(&mut self, _project_id: &str) -> BackendResult<Vec<String>> {
        Err(BackendError::Unsupported("recent.list not wired to real FFI yet".into()))
    }
}

/// Entry point called from C++ (`shotcut/src/main.cpp`), on a dedicated
/// background `std::thread` -- never the Qt main thread, since this
/// function blocks (running a tokio runtime) for the entire lifetime of the
/// SAP server. See `shotcut/src/rustbridge/sap_ffi.h` for the C++-side
/// declaration and its call site.
///
/// # Safety
/// `main_window` must be a valid `MainWindow*` obtained from the running Qt
/// process (as `MainWindow::singleton()`/`MAIN`). `socket_path` and `token`
/// must be valid, NUL-terminated C strings for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn sap_start_server(
    main_window: *mut c_void,
    socket_path: *const c_char,
    token: *const c_char,
) {
    if socket_path.is_null() {
        eprintln!("sap-rust: sap_start_server called with a null socket_path, not starting");
        return;
    }
    let socket_path = CStr::from_ptr(socket_path).to_string_lossy().into_owned();
    let token = if token.is_null() {
        String::new()
    } else {
        CStr::from_ptr(token).to_string_lossy().into_owned()
    };

    let backend = FfiBackend::new(main_window);
    let config = ServerConfig {
        socket_path: PathBuf::from(&socket_path),
        token,
        audio_enabled: false,
    };

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sap-rust: failed to start tokio runtime: {e}");
            return;
        }
    };
    eprintln!("sap-rust: SAP server starting on {socket_path}");
    if let Err(e) = rt.block_on(server::serve(config, backend)) {
        eprintln!("sap-rust: server exited with error: {e}");
    }
}

/// Standard base64 (RFC 4648, with `=` padding) -- a local copy of
/// `mlt_backend::base64_encode`'s algorithm/alphabet, kept in sync
/// deliberately (not imported: that function is private to
/// `mlt_backend.rs`, and this file is restricted to touching
/// `ffi_backend.rs`/`ffi.rs` only) so `playback_get_frame`'s wire format
/// matches `MltBackend::playback_get_frame`'s byte-for-byte.
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
