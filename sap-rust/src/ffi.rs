//! `extern "C"` declarations for the real C-ABI shim in
//! `shotcut/src/rustbridge/sap_ffi.h` / `sap_ffi.cpp`, per
//! `memory/head/gen/rust-fork/02-rust-embedding.md` ("Option A: thin C-ABI
//! shim"). These now describe the *actual*, implemented shim (see that file)
//! rather than a hypothetical one -- every mutating call on the C++ side
//! crosses to the Qt main thread via
//! `QMetaObject::invokeMethod(..., Qt::BlockingQueuedConnection)` before
//! touching any Qt/MLT state, so calling these from any thread on the Rust
//! side is sound as long as the `MainWindow*` handle is still valid (i.e.
//! the Qt process this crate is linked into is still alive).
//!
//! Gated behind the `real_ffi` Cargo feature, which is OFF by default.
//! Meaningful only when this crate is built as part of shotcut's CMake
//! build (via `corrosion_import_crate(... FEATURES real_ffi)`) and linked
//! against the real `sap_ffi.cpp` translation unit -- a plain `cargo build
//! --features real_ffi` run standalone in `sap-rust/` will compile these
//! declarations, but nothing in this crate's own `bin`/tests references
//! them directly (only `ffi_backend.rs`, itself only reachable from the C++
//! side via `sap_start_server`), so it does not attempt to link a missing
//! shim.

#![cfg(feature = "real_ffi")]
#![allow(dead_code)]

use std::os::raw::{c_char, c_int, c_longlong, c_void};

extern "C" {
    /// C++ side: `int sap_add_video_track(void* mainWindowHandle);`
    /// Returns the new track's 0-based index (per the wrapped
    /// `TimelineDock::addVideoTrack()`), or -1 on error.
    pub fn sap_add_video_track(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `int sap_add_audio_track(void* mainWindowHandle);`
    pub fn sap_add_audio_track(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `int sap_remove_track(void* mainWindowHandle, int trackIndex);`
    /// Returns 0 on success, -1 on error (invalid handle/index).
    pub fn sap_remove_track(main_window_handle: *mut c_void, track_index: c_int) -> c_int;

    /// C++ side: `int sap_set_track_muted/hidden/locked(void* mainWindowHandle,
    /// int trackIndex, int value);` -- real
    /// `MultitrackModel::setTrackMute`/`setTrackHidden`/`setTrackLock`.
    /// Returns 0 on success, -1 on error (invalid handle/index).
    pub fn sap_set_track_muted(main_window_handle: *mut c_void, track_index: c_int, muted: c_int) -> c_int;
    pub fn sap_set_track_hidden(main_window_handle: *mut c_void, track_index: c_int, hidden: c_int) -> c_int;
    pub fn sap_set_track_locked(main_window_handle: *mut c_void, track_index: c_int, locked: c_int) -> c_int;

    /// C++ side: `int sap_reorder_track(void* mainWindowHandle, int
    /// fromTrackIndex, int toTrackIndex);` -- real
    /// `TimelineDock::moveTrack()`/`Timeline::MoveTrackCommand` (undoable).
    /// Returns 0 on success, -1 on error (invalid handle/index, or
    /// mismatched track types).
    pub fn sap_reorder_track(main_window_handle: *mut c_void, from_track_index: c_int, to_track_index: c_int) -> c_int;

    /// C++ side: `int sap_remove_clip(void* mainWindowHandle, int
    /// trackIndex, int clipIndex);` -- real
    /// `TimelineDock::remove()`/`Timeline::RemoveCommand` (undoable).
    /// Returns 0 on success, -1 on error (invalid handle/index, locked
    /// track).
    pub fn sap_remove_clip(main_window_handle: *mut c_void, track_index: c_int, clip_index: c_int) -> c_int;

    /// C++ side: `char* sap_move_clip(void* mainWindowHandle, int
    /// fromTrackIndex, int fromClipIndex, int toTrackIndex, int
    /// toClipIndex, int ripple);` -- real
    /// `TimelineDock::moveClip()`/`Timeline::MoveClipCommand` (undoable).
    /// Returns a heap-allocated JSON object string describing the clip at
    /// its final position, re-read from the real destination playlist, or
    /// NULL on error/rejected move. Caller must free via `sap_free_string`.
    pub fn sap_move_clip(
        main_window_handle: *mut c_void,
        from_track_index: c_int,
        from_clip_index: c_int,
        to_track_index: c_int,
        to_clip_index: c_int,
        ripple: c_int,
    ) -> *mut c_char;

    /// C++ side: `char* sap_get_track_blend_mode(void* mainWindowHandle,
    /// int trackIndex);` -- real per-track qtblend/movit.overlay/
    /// cairoblend transition mode property, read back live. Returns a
    /// heap-allocated string, or NULL if the track has no blend transition
    /// or on error. Caller must free via `sap_free_string`.
    pub fn sap_get_track_blend_mode(main_window_handle: *mut c_void, track_index: c_int) -> *mut c_char;

    /// C++ side: `int sap_set_track_blend_mode(void* mainWindowHandle, int
    /// trackIndex, const char* mode);` -- real
    /// `Timeline::ChangeBlendModeCommand` (undoable). Returns 0 on success,
    /// -1 on error (invalid handle/index, or no blend transition present).
    pub fn sap_set_track_blend_mode(main_window_handle: *mut c_void, track_index: c_int, mode: *const c_char) -> c_int;

    /// C++ side: `int sap_set_track_height(void* mainWindowHandle, int
    /// height);` -- real `MultitrackModel::setTrackHeight()`, a single
    /// project-wide `shotcut:trackHeight` tractor property (not per-track),
    /// clamped to [10, 150] by the real setter. Returns 0 on success, -1 on
    /// error (invalid handle).
    pub fn sap_set_track_height(main_window_handle: *mut c_void, height: c_int) -> c_int;

    /// C++ side: `char* sap_filter_add(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, const char* mltService, const char*
    /// propertiesJson);` -- attaches a real MLT filter to the clip's
    /// per-instance "cut" producer. NOT undoable via Ctrl+Z (no lightweight
    /// QUndoCommand exists that doesn't also require the full
    /// QmlMetadata-driven filter-panel machinery). Returns a heap-allocated
    /// JSON object string `{"filterIndex":N,"mltService":"..."}`, or NULL
    /// on error. Caller must free via `sap_free_string`.
    pub fn sap_filter_add(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        mlt_service: *const c_char,
        properties_json: *const c_char,
    ) -> *mut c_char;

    /// C++ side: `int sap_filter_set_property(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, int filterIndex, const char* property,
    /// const char* valueJson);` -- `valueJson` is one JSON-encoded scalar.
    /// Same non-undoable caveat as `sap_filter_add`. Returns 0 on success,
    /// -1 on error.
    pub fn sap_filter_set_property(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        filter_index: c_int,
        property: *const c_char,
        value_json: *const c_char,
    ) -> c_int;

    /// C++ side: `char* sap_filter_list(void* mainWindowHandle, int
    /// trackIndex, int clipIndex);` -- returns a heap-allocated JSON array
    /// string `[{"filterIndex":0,"mltService":"..."},...]` in raw MLT
    /// filter-chain order, or NULL on error. Caller must free via
    /// `sap_free_string`.
    pub fn sap_filter_list(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
    ) -> *mut c_char;

    /// C++ side: `int sap_filter_remove(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, int filterIndex);` -- real
    /// `Mlt::Service::detach()`. Same non-undoable caveat as
    /// `sap_filter_add`. Returns 0 on success, -1 on error.
    pub fn sap_filter_remove(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        filter_index: c_int,
    ) -> c_int;

    /// C++ side: `int sap_filter_reorder(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, int fromIndex, int toIndex);` -- real
    /// `Mlt::Service::move_filter()`. Same non-undoable caveat as
    /// `sap_filter_add`. Returns 0 on success, -1 on error.
    pub fn sap_filter_reorder(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        from_index: c_int,
        to_index: c_int,
    ) -> c_int;

    /// C++ side: `char* sap_list_clips(void* mainWindowHandle, int
    /// trackIndex);` -- returns a heap-allocated JSON array string listing
    /// every clip on trackIndex, in playlist order, of the form
    /// `[{"clipId":"t0c0","index":0,"path":"...","inFrame":0,"outFrame":299},...]`,
    /// or NULL on error (invalid handle/trackIndex). Caller must free via
    /// `sap_free_string`.
    pub fn sap_list_clips(main_window_handle: *mut c_void, track_index: c_int) -> *mut c_char;

    /// C++ side: `int sap_trim_clip_in(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, long long newInFrame);` -- real
    /// `Timeline::TrimClipInCommand` (undoable), non-ripple. Returns 0 on
    /// success, -1 on error (invalid handle/track/clip/locked track, or
    /// out-of-range newInFrame).
    pub fn sap_trim_clip_in(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        new_in_frame: c_longlong,
    ) -> c_int;

    /// C++ side: `int sap_trim_clip_out(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, long long newOutFrame);` -- real
    /// `Timeline::TrimClipOutCommand` (undoable), non-ripple. Returns 0 on
    /// success, -1 on error.
    pub fn sap_trim_clip_out(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        new_out_frame: c_longlong,
    ) -> c_int;

    /// C++ side: `char* sap_split_clip(void* mainWindowHandle, int
    /// trackIndex, int clipIndex, long long position);` -- real
    /// `Timeline::SplitCommand` (undoable). Returns a heap-allocated JSON
    /// object string `{"leftClipId":...,"rightClipId":...,"leftIndex":...,
    /// "rightIndex":...}`, or NULL on error. Caller must free via
    /// `sap_free_string`.
    pub fn sap_split_clip(
        main_window_handle: *mut c_void,
        track_index: c_int,
        clip_index: c_int,
        position: c_longlong,
    ) -> *mut c_char;

    /// Playlist ("Source"/bin panel) operations via the real PlaylistModel
    /// slots. NOT part of the undo stack in real Shotcut either (bin
    /// management isn't undoable there). Each returns a heap-allocated
    /// JSON object/array of the form `{"index":N,"name":"...","path":"...",
    /// "durationFrames":N}`, or NULL/-1 on error. Caller must free string
    /// results via `sap_free_string`.
    pub fn sap_playlist_append(main_window_handle: *mut c_void, source_path: *const c_char) -> *mut c_char;
    pub fn sap_playlist_insert(main_window_handle: *mut c_void, index: c_int, source_path: *const c_char) -> *mut c_char;
    pub fn sap_playlist_remove(main_window_handle: *mut c_void, index: c_int) -> c_int;
    pub fn sap_playlist_move(main_window_handle: *mut c_void, from_index: c_int, to_index: c_int) -> c_int;
    pub fn sap_playlist_get(main_window_handle: *mut c_void, index: c_int) -> *mut c_char;
    pub fn sap_playlist_list(main_window_handle: *mut c_void) -> *mut c_char;

    /// C++ side: `char* sap_list_tracks(void* mainWindowHandle);` -- returns a
    /// heap-allocated, NUL-terminated JSON array string of the form
    /// `[{"index":0,"kind":"video"},...]` (built from the real
    /// `MultitrackModel::trackList()`), or NULL on error. The caller must
    /// free the returned pointer via `sap_free_string`.
    pub fn sap_list_tracks(main_window_handle: *mut c_void) -> *mut c_char;

    /// C++ side: `int sap_save_project(void* mainWindowHandle);` -- wraps
    /// `MainWindow::saveXML()` (which itself calls
    /// `Controller::saveXML()`, mltcontroller.cpp:489), saving to the
    /// project's current filename (or its untitled default). Returns 0 on
    /// success, -1 on failure.
    pub fn sap_save_project(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `int sap_project_undo(void* mainWindowHandle);` -- applies
    /// the next undo command on `MAIN.undoStack()`. Returns 0 on success,
    /// -1 when the handle or undo stack is unavailable.
    pub fn sap_project_undo(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `int sap_project_redo(void* mainWindowHandle);` -- applies
    /// the next redo command on `MAIN.undoStack()`. Returns 0 on success,
    /// -1 when the handle or redo stack is unavailable.
    pub fn sap_project_redo(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `int sap_playback_seek(void* mainWindowHandle, long long frame);`
    /// Returns 0 on success, -1 on failure.
    pub fn sap_playback_seek(main_window_handle: *mut c_void, frame: c_longlong) -> c_int;

    /// C++ side: `int sap_get_undo_depth(void* mainWindowHandle);` -- number
    /// of commands available to undo on `MAIN.undoStack()`. -1 on error.
    pub fn sap_get_undo_depth(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `int sap_get_redo_depth(void* mainWindowHandle);` -- number
    /// of commands available to redo on `MAIN.undoStack()`. -1 on error.
    pub fn sap_get_redo_depth(main_window_handle: *mut c_void) -> c_int;

    /// C++ side: `char* sap_append_clip(void* mainWindowHandle, int
    /// trackIndex, const char* sourcePath);` -- opens `sourcePath` as a
    /// real `Mlt::Producer` and appends it to `trackIndex` via the real,
    /// undoable `Timeline::AppendCommand` (pushed on `MAIN.undoStack()`).
    /// Returns a heap-allocated JSON object string, e.g.
    /// `{"clipId":"t0c0","index":0,"inFrame":0,"outFrame":119}`, or NULL on
    /// error. Caller must free the returned pointer via `sap_free_string`.
    pub fn sap_append_clip(
        main_window_handle: *mut c_void,
        track_index: c_int,
        source_path: *const c_char,
    ) -> *mut c_char;

    /// C++ side: `unsigned char* sap_get_frame(void* mainWindowHandle,
    /// long long frame, const char* format, int* outLen);` -- renders the
    /// given absolute timeline frame off the live project producer and
    /// encodes it (JPEG unless `format` is "png"), via
    /// `Controller::image()` (mltcontroller.cpp), the same primitive
    /// Shotcut's own thumbnails use. `*out_len` receives the byte length.
    /// Returns NULL (and `*out_len == 0`) on error. Caller must free the
    /// returned pointer via `sap_free_bytes`.
    pub fn sap_get_frame(
        main_window_handle: *mut c_void,
        frame: c_longlong,
        format: *const c_char,
        out_len: *mut c_int,
    ) -> *mut u8;

    /// Frees a byte buffer returned by `sap_get_frame`.
    pub fn sap_free_bytes(buf: *mut u8);

    /// Frees a string returned by `sap_list_tracks`.
    pub fn sap_free_string(s: *mut c_char);
}
