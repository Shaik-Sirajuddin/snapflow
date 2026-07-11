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
