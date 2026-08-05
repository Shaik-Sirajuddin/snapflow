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

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Read;
use std::os::raw::{c_char, c_int, c_longlong, c_void};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::backend::{
    Backend, BackendError, BackendResult, Clip, FileProbe, FilterInfo, FilterListEntry, JobStatus,
    KeyframeInfo, Marker, PlaybackState, PlaylistEntry, PlaylistEntryDetail, ProfileInfo,
    ProjectState, SplitClipResult, SubtitleTrackInfo, Track, TransitionInfo,
};
use crate::ffi;
use crate::protocol::RpcNotification;
use crate::server::{self, ServerConfig};

/// Sender half of the external-notification bridge (see `serve`'s
/// `external_notify_rx` doc comment in `server.rs`), set once by
/// `sap_start_server` before the tokio runtime starts and read by
/// `sap_ffi_notify_bridge` (called from C++'s `sap_emit_event`, which may
/// fire before or after that point -- `None` here just means "server not
/// up yet, drop it", the same best-effort semantics the prior stderr-only
/// stub already had). A plain `std::sync::Mutex` is fine: this is set
/// once and read rarely (only on real Qt-side edits), never on the hot
/// per-RPC-call path.
static NOTIFY_BRIDGE_TX: Mutex<Option<mpsc::UnboundedSender<RpcNotification>>> = Mutex::new(None);
static EXPORT_NOTIFY_TX: Mutex<Option<mpsc::UnboundedSender<RpcNotification>>> = Mutex::new(None);

fn publish_export_notification(status: &JobStatus) {
    let method = match status.status.as_str() {
        "done" => "jobs.completed",
        "error" => "jobs.failed",
        "stopped" => "jobs.stopped",
        _ => return,
    };
    let mut params = json!({"jobId": status.job_id, "status": status.status, "percent": status.percent});
    if let Some(obj) = params.as_object_mut() {
        if let Some(path) = &status.result_path { obj.insert("outputPath".into(), json!(path)); }
        if let Some(error) = &status.error { obj.insert("error".into(), json!(error)); }
    }
    if let Ok(guard) = EXPORT_NOTIFY_TX.lock() {
        if let Some(tx) = guard.as_ref() { let _ = tx.send(RpcNotification::new(method, params)); }
    }
}

/// Set to `true` by `run_dispatcher` (`server.rs`) for the duration of
/// every single dispatched `Backend` call, regardless of method or which
/// `Backend` impl is in use. `sap_ffi_notify_bridge` below checks this to
/// decide whether to publish its own generic "qtGuiEdit"-reason
/// `edit.changed` notification.
///
/// Why this exists: `MultitrackModel::modified` (and friends) fires
/// synchronously on the Qt thread as a side effect of the *same*
/// `Qt::BlockingQueuedConnection` call an RPC-driven `FfiBackend` method
/// (e.g. `edit_add_track`) makes into C++ -- so a single `edit.addTrack`
/// RPC produces two notifications without this flag: `build_op`'s own
/// specific one (`{"reason": "addTrack", ...}`, attached to the RPC
/// response and fanned out normally) *and* this bridge's generic one
/// (`{"reason": "qtGuiEdit"}`, fanned out to every project via
/// `external_notify_rx`). Both are real, but racing them means a
/// subscriber sometimes observes the uninformative generic one first
/// (see `TestMCPAdapter_PhaseB_SameProjectConcurrency`'s addTrack
/// assertion). Since the dispatcher processes one op at a time (FIFO,
/// `05-multi-client-concurrency.md`) and the Qt-side signal fires
/// strictly within that op's own synchronous blocking call, wrapping
/// each dispatch with `store(true)`/`store(false)` here reliably
/// distinguishes "this Qt model change was already RPC-attributed" from
/// "this Qt model change came from something else (e.g. a real human
/// editing the same visible GUI)" -- only the latter still needs the
/// generic bridge notification.
pub static SUPPRESS_QT_BRIDGE_NOTIFICATION: AtomicBool = AtomicBool::new(false);

/// Wraps the opaque `MainWindow*` handle passed in from C++
/// (`MainWindow::singleton()`/`MAIN`, cast to `void*`). The embedded process
/// has exactly one live project -- the window itself -- so unlike
/// `MockBackend` there is no per-`project_id` routing to do; any bound
/// `project_id` addresses the same running project.
pub struct FfiBackend {
    main_window: *mut c_void,
    /// Export job registry -- mirrors `MltBackend`'s (same `JobStatus`
    /// shape, same background-thread-polls-`try_wait` pattern), because
    /// there is no live Qt/QML-side "job" concept exposed via C-ABI to
    /// wire instead: real Shotcut's own export path (`EncodeDock`/
    /// `JobQueue`) is a large, QML-metadata-driven UI surface, not a thin
    /// primitive worth shimming just for this. `file_export` here instead
    /// exports the *real* current project to a real MLT XML file via
    /// `sap_export_project_xml` (the same `MainWindow::saveXML()` "Save
    /// As" uses), then spawns the same real `melt` CLI MltBackend does --
    /// so the render itself is 100% real, only the job-tracking
    /// bookkeeping is duplicated Rust-side state rather than a Qt-side
    /// primitive.
    jobs: Arc<Mutex<HashMap<String, JobStatus>>>,
    job_children: HashMap<String, Arc<Mutex<Option<Child>>>>,
    /// The bound project's sandbox root (per
    /// `09-project-folder-layout.md`), read once from
    /// `SNAPSHOT_PROJECT_ROOT` at construction time -- unlike
    /// `MockBackend`/the removed `MltBackend`, this backend has no
    /// per-`project_id` router of its own (one Qt process == one live
    /// project), so there is nowhere else to source this from. `None`
    /// when the env var is unset or empty (e.g. a manual dev launch not
    /// going through `snapshotd`), in which case `file_import` skips the
    /// containment check entirely rather than rejecting everything.
    project_root: Option<PathBuf>,
    /// Guards the real disk-load/bind so a 2nd/3rd MCP client's
    /// `project.select` (Router.Bind fans every session's bind onto the
    /// same pooled connection) is a safe no-op, not a reload that would
    /// discard in-memory edits.
    opened: bool,
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

/// Reset the melt child's signal mask/dispositions AND close every
/// inherited file descriptor above stderr, immediately after fork, before
/// exec. Runs on the child side only (`Command::pre_exec`), unsafe per its
/// contract (async-signal-safe calls only between fork and exec).
///
/// Why: melt, forked directly from this live Qt process, reproducibly
/// wedges (all threads parked in `futex_do_wait`, zero forward CPU/output
/// progress) at a deterministic byte offset in the encode -- same offset,
/// every attempt, every fresh respawn -- while the identical invocation
/// run from a plain shell completes cleanly every time. That determinism
/// (not a random race) plus "only when forked from Qt" points at
/// inherited *resource* state, not signal state: `ls -la
/// /proc/<qt-pid>/fd` on the live headed process shows several
/// non-CLOEXEC fds Qt/the platform GPU stack holds open --
/// `/dev/udmabuf`, `memfd:lp_dma_buf`, `/dmabuf:`, `anon_inode:sync_file`
/// (a DRM/dma-buf fence) -- alongside assorted eventfd/socket fds. A
/// forked melt child inherits these raw. If melt's decode path (e.g. a
/// hardware-accelerated producer) ever touches a GPU sync fence the live
/// Qt renderer also holds, that is exactly the "wedged waiting on a
/// futex/fence, deterministic point, only when forked from Qt" signature
/// observed here. `Command`'s own stdio wiring (dup2 to 0/1/2) runs
/// before `pre_exec` closures, so it is safe to unconditionally
/// `close_range(3, MAX, 0)` here -- our own pipes are already in place on
/// 0/1/2 and everything else inherited from the parent is exactly what we
/// want gone before melt's own `main()` runs. Signal-mask/disposition
/// reset is kept alongside as cheap additional insurance (Qt's Unix
/// signal socketpair machinery could independently leave signals blocked
/// on the forking thread) even though it alone was insufficient --
/// confirmed by hand: wired in, rebuilt, retested, and the stall still
/// reproduced at the identical byte offset on all 3 watchdog attempts.
// `close_range` is a Linux-only syscall wrapper (glibc >= 2.34) -- it does
// not exist in the `libc` crate on macOS/BSD, so the fd-cleanup step below
// is Linux-only. The dma-buf/sync-fence wedge this guards against is itself
// Linux-specific (see doc comment above), so no macOS/BSD equivalent is
// needed here; those platforms still get the signal-mask/disposition reset.
#[cfg(target_os = "linux")]
fn reset_child_signals(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            // Mark every inherited fd above stderr close-on-exec -- in
            // particular the GPU dma-buf/sync-fence fds the live Qt process
            // holds open (see doc comment above). Do not close these fds
            // immediately: Rust's Command implementation keeps its own
            // exec-error pipe above fd 2 until exec succeeds. Closing that
            // pipe masks the real exec errno and makes a failed melt
            // launch abort with `output.write(&bytes).is_ok()` (SIGABRT).
            // `close_range` is a single async-signal-safe syscall (glibc >=
            // 2.34 wraps it directly); a negative return is deliberately
            // non-fatal, so the worst case is that some fd leaks through.
            libc::close_range(
                3,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC as libc::c_int,
            );
            Ok(())
        });
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn reset_child_signals(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }
}

/// Windows has no fork/exec boundary (no `pre_exec` equivalent) and the
/// Linux dma-buf/sync-fence wedge above does not reproduce there (Windows'
/// handle inheritance is opt-in per-handle, not inherit-by-default), so
/// this is not a port of the Unix fix -- it only suppresses the console
/// window flash a GUI process (`shotcut.exe`) spawning a console
/// subprocess (`melt.exe`) would otherwise show. The other Windows-only
/// concern (killing `melt.exe` if the parent dies unexpectedly) is handled
/// separately in `spawn_melt_draining_stderr` via a Job Object, since that
/// needs the spawned `Child` handle, not just the `Command` being built.
#[cfg(windows)]
fn reset_child_signals(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Spawn `cmd` (which must already have `.stdout(Stdio::piped())` and
/// `.stderr(Stdio::piped())` set) and immediately start a background
/// thread continuously draining the child's stderr into a shared buffer.
///
/// This is the actual fix for the melt stall root-caused here: `melt`
/// writes a running frame/progress line to stdout (and occasional
/// warnings to stderr) as it encodes. The old code captured both via
/// `Stdio::piped()` but only ever read stderr, and only *after* the
/// child had already exited -- classic unread-pipe deadlock. A pipe is a
/// fixed-size OS buffer (64KiB by default on Linux); once melt's
/// progress-line output fills it with nobody draining the read end,
/// melt's own `write(2)` call blocks forever, freezing the whole process
/// (including its writes to the actual output file) at whatever byte
/// offset corresponds to that fixed amount of accumulated stdout text --
/// which is exactly why the stall was 100% deterministic at the same
/// output-file byte offset on every attempt, every fresh respawn, and
/// (confirmed by hand, reproduced with a plain `subprocess.Popen(...,
/// stdout=PIPE, stderr=PIPE)` from a bare Python script with zero Qt/fork
/// involvement) *independent of whether melt was forked from the live Qt
/// process at all*. Every earlier theory tried here (DISPLAY forwarding,
/// signal mask/disposition inheritance, leaked GPU dma-buf fds) was
/// treating a symptom of this same underlying deadlock, not the cause.
/// Fix: stdout is discarded entirely (`Stdio::null()`, set by the
/// caller -- nothing in this codebase ever consumed it) and stderr is
/// drained continuously here rather than buffered up and read once at
/// the end.
/// Windows has no `SIGCHLD`/process-group equivalent, so if this daemon
/// (or the host Shotcut process) dies or is killed unexpectedly, a spawned
/// `melt.exe` would otherwise keep running as an orphan forever -- the
/// closest Windows analogue to what `reset_child_signals`' Unix fd-cleanup
/// protects against, even though the concrete failure mode differs (orphan
/// survival vs. fd inheritance). Fix: put the child in a fresh Job Object
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then deliberately leak the
/// job handle (`std::mem::forget`) rather than close it ourselves --
/// closing it immediately would kill the child right away, whereas leaking
/// it keeps the job (and its kill-on-close semantics) alive for exactly as
/// long as this process runs; Windows closes every handle this process
/// still holds when it exits or is killed, which is the trigger that
/// terminates `melt.exe` too. A failure here (job creation/assignment)
/// is deliberately non-fatal to the export itself -- worst case melt just
/// loses the orphan-cleanup guarantee, same tradeoff `reset_child_signals`
/// makes with `close_range`'s return value on Unix.
#[cfg(windows)]
fn assign_to_kill_on_close_job(child: &Child) {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            windows_sys::Win32::Foundation::CloseHandle(job);
            return;
        }

        let process_handle: HANDLE = child.as_raw_handle() as HANDLE;
        if AssignProcessToJobObject(job, process_handle) == 0 {
            windows_sys::Win32::Foundation::CloseHandle(job);
        }
        // On success, `job` is deliberately never closed here -- see doc
        // comment above (closing it now would kill the child immediately;
        // it must outlive this function, closed only by the OS when this
        // process itself exits).
    }
}

/// Spawn Melt and continuously drain both output streams. Melt writes its
/// frame/progress heartbeat to stdout; keeping that pipe drained is essential
/// because an undrained pipe can block the encoder before it writes the MP4.
/// The heartbeat counter is deliberately advisory: a render is allowed to
/// remain quiet while constructing a complex frame, and completion/explicit
/// `jobs.stop` are the only terminal conditions.
fn spawn_melt_draining_stderr(
    mut cmd: Command,
) -> std::io::Result<(Child, Arc<Mutex<String>>, Arc<std::sync::atomic::AtomicU64>)> {
    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    assign_to_kill_on_close_job(&child);
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let heartbeat = Arc::new(std::sync::atomic::AtomicU64::new(0));
    if let Some(mut pipe) = child.stdout.take() {
        let beat = heartbeat.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        beat.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    }
    if let Some(mut pipe) = child.stderr.take() {
        let buf = stderr_buf.clone();
        let beat = heartbeat.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        beat.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut b) = buf.lock() {
                            b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        }
                    }
                }
            }
        });
    }
    Ok((child, stderr_buf, heartbeat))
}

impl FfiBackend {
    /// # Safety
    /// `main_window` must be a valid, live `MainWindow*` (as obtained from
    /// `MainWindow::singleton()`) for as long as this backend is used --
    /// i.e. for the lifetime of the Qt process this crate is linked into.
    pub unsafe fn new(main_window: *mut c_void) -> Self {
        let project_root = std::env::var("SNAPSHOT_PROJECT_ROOT")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        // Do not call Qt with BlockingQueuedConnection from this constructor:
        // it runs on the detached SAP thread while the GUI is still entering
        // its event loop. Bind-only projects set their target path in the
        // first project_select instead; pre-opened projects already have the
        // correct MainWindow filename from the positional GUI argument.
        Self {
            main_window,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            job_children: HashMap::new(),
            project_root,
            opened: matches!(
                std::env::var("SNAPSHOT_PROJECT_PREOPENED").as_deref(),
                Ok("1") | Ok("true") | Ok("TRUE") | Ok("True")
            ),
        }
    }

    fn undo_redo_depth(&self) -> BackendResult<(usize, usize)> {
        let undo = unsafe { ffi::sap_get_undo_depth(self.main_window) };
        let redo = unsafe { ffi::sap_get_redo_depth(self.main_window) };
        if undo < 0 || redo < 0 {
            return Err(BackendError::NotFound("undo stack unavailable".into()));
        }
        Ok((undo as usize, redo as usize))
    }

    /// Parses the `"t{trackIndex}c{clipIndex}"` clip-id format the C++
    /// side (`sap_ffi.cpp`) mints for every clip it hands back (see
    /// `sap_append_clip`/`sap_move_clip`), so filter.* calls (which take a
    /// `clip_id` rather than a track/clip-index pair per
    /// `01-jsonrpc-spec.md`) can resolve back to the FFI's index-based
    /// calls.
    fn parse_clip_id(clip_id: &str) -> BackendResult<(usize, usize)> {
        let rest = clip_id
            .strip_prefix('t')
            .ok_or_else(|| BackendError::InvalidParams(format!("malformed clip id: {clip_id}")))?;
        let (track_part, clip_part) = rest
            .split_once('c')
            .ok_or_else(|| BackendError::InvalidParams(format!("malformed clip id: {clip_id}")))?;
        let track_index = track_part
            .parse::<usize>()
            .map_err(|_| BackendError::InvalidParams(format!("malformed clip id: {clip_id}")))?;
        let clip_index = clip_part
            .parse::<usize>()
            .map_err(|_| BackendError::InvalidParams(format!("malformed clip id: {clip_id}")))?;
        Ok((track_index, clip_index))
    }

    /// Parses a raw `sap_markers_*` JSON object result (NULL pointer, or a
    /// `{"index":N,"frame":N,"endFrame":N|absent,"text":"...",
    /// "color":"#RRGGBB"}` string) into a `Marker`. `Marker`'s own
    /// `#[serde(rename_all = "camelCase")]` shape matches the C++ side's
    /// JSON exactly, so no intermediate raw struct is needed here (unlike
    /// `parse_playlist_entry`/`SplitClipResult`'s raw structs, which do
    /// need field remapping).
    fn parse_marker(raw: *mut c_char, not_found_msg: &str) -> BackendResult<Marker> {
        if raw.is_null() {
            return Err(BackendError::NotFound(not_found_msg.to_string()));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<Marker>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad marker JSON: {e}")))
    }

    /// Parses one `sap_playlist_*` JSON object result (`{"index":N,
    /// "name":"...","path":"...","durationFrames":N}`) into a `PlaylistEntry`,
    /// with the caller-supplied `source` value (so `playlist.append`/
    /// `insert` echo back the exact source JSON the caller sent, while
    /// `playlist.list`/`get` synthesize `{"path": ...}` from the live
    /// re-read resource, matching MltBackend's own echo-vs-derive split).
    fn parse_playlist_entry(json_str: &str, source: Value) -> BackendResult<PlaylistEntry> {
        let value: Value = serde_json::from_str(json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad playlist-entry JSON: {e}")))?;
        Self::parse_playlist_entry_value(value, source)
    }

    fn parse_playlist_entry_value(value: Value, source: Value) -> BackendResult<PlaylistEntry> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            index: usize,
            name: String,
            duration_frames: i64,
        }
        let raw: Raw = serde_json::from_value(value)
            .map_err(|e| BackendError::InvalidParams(format!("bad playlist-entry JSON: {e}")))?;
        Ok(PlaylistEntry {
            index: raw.index,
            name: raw.name,
            source,
            duration_frames: raw.duration_frames,
        })
    }

    /// Resolves an `edit.appendClip`/`insertClip`/`overwriteClip` `source`
    /// value's tagged union (`{path}` | `{xml}` | `{playlistIndex}`, per
    /// rust-fork/01-jsonrpc-spec.md) to a ready-to-use `CString`, tagged by
    /// which of the `sap_*_clip`/`sap_*_clip_xml` C-ABI pairs it belongs
    /// to. `{playlistIndex}` is resolved here via `sap_playlist_get_xml`
    /// (the live producer's own MLT XML, filters intact) rather than
    /// re-deriving a path from `sap_playlist_get`'s "path" field, which is
    /// only the raw resource string and would silently drop e.g. a title
    /// clip's attached dynamictext/qtext filter.
    fn resolve_clip_source(&mut self, source: &Value) -> BackendResult<ClipSourceResolution> {
        if let Some(path) = source.get("path").and_then(Value::as_str) {
            let c_path = CString::new(path)
                .map_err(|e| BackendError::InvalidParams(format!("invalid source path: {e}")))?;
            return Ok(ClipSourceResolution::Path(c_path));
        }
        if let Some(xml) = source.get("xml").and_then(Value::as_str) {
            let c_xml = CString::new(xml)
                .map_err(|e| BackendError::InvalidParams(format!("invalid source xml: {e}")))?;
            return Ok(ClipSourceResolution::Xml(c_xml));
        }
        if let Some(index) = source.get("playlistIndex").and_then(Value::as_u64) {
            let raw = unsafe { ffi::sap_playlist_get_xml(self.main_window, index as c_int) };
            if raw.is_null() {
                return Err(BackendError::NotFound(format!("playlist index {index}")));
            }
            let xml = unsafe { CStr::from_ptr(raw) }
                .to_string_lossy()
                .into_owned();
            unsafe { ffi::sap_free_string(raw) };
            let c_xml = CString::new(xml).map_err(|e| {
                BackendError::InvalidParams(format!("invalid resolved playlist xml: {e}"))
            })?;
            return Ok(ClipSourceResolution::Xml(c_xml));
        }
        Err(BackendError::InvalidParams(
            "source must be {path: ...} | {xml: ...} | {playlistIndex: ...}".into(),
        ))
    }
}

/// Result of `FfiBackend::resolve_clip_source` -- which `sap_*_clip`
/// (path-opening) vs `sap_*_clip_xml` (ready-made XML) C-ABI sibling to
/// call, per `edit.appendClip`/`insertClip`/`overwriteClip`'s shared
/// `source` union.
enum ClipSourceResolution {
    Path(CString),
    Xml(CString),
}

impl Backend for FfiBackend {
    fn project_select(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        // No multi-project routing in-process: the currently-open project
        // *is* the one project this Qt process has. Any project_id binds.
        //
        // One-time open: Router.Bind sends project.select on every session
        // attach (including 2nd/3rd MCP clients on an already-live project).
        // Only the first call may load from disk; later calls are no-ops
        // that still return current ProjectState.
        if !self.opened {
            // Set first so a failed open below does not retry-loop on every
            // subsequent select (matches plan sketch).
            self.opened = true;
            if let Some(root) = self.project_root.as_ref() {
                let mlt_file_name = std::env::var("SNAPSHOT_PROJECT_MLT_FILENAME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "project.mlt".to_string());
                let mlt_path = root.join(&mlt_file_name);
                if let Some(path_str) = mlt_path.to_str() {
                    if let Ok(c_path) = CString::new(path_str) {
                        let rc = if mlt_path.exists() {
                            unsafe { ffi::sap_open_project(self.main_window, c_path.as_ptr()) }
                        } else {
                            // Bind-only fallback: same as FfiBackend::new.
                            let set_rc = unsafe {
                                ffi::sap_set_project_file(self.main_window, c_path.as_ptr())
                            };
                            // Brand-new folder-type projects: set
                            // MLT.projectFolder before first save so
                            // kSnapflowProjectFolder=1 is written.
                            // SNAPSHOT_PROJECT_TYPE is set by procmgr from
                            // registry.Project.ProjectType.
                            if set_rc == 0 {
                                let ptype = std::env::var("SNAPSHOT_PROJECT_TYPE")
                                    .unwrap_or_else(|_| "folder".to_string());
                                if ptype == "folder" {
                                    if let Some(root_str) = root.to_str() {
                                        if let Ok(c_root) = CString::new(root_str) {
                                            let _ = unsafe {
                                                ffi::sap_set_project_folder(
                                                    self.main_window,
                                                    c_root.as_ptr(),
                                                )
                                            };
                                        }
                                    }
                                } else if ptype == "file" {
                                    let _ = unsafe {
                                        ffi::sap_set_project_folder(
                                            self.main_window,
                                            std::ptr::null(),
                                        )
                                    };
                                }
                            }
                            set_rc
                        };
                        if rc != 0 {
                            return Err(BackendError::InvalidParams("project open failed".into()));
                        }
                    }
                }
            }
        }
        self.project_get_state(project_id)
    }

    fn project_exit(&mut self) -> BackendResult<()> {
        // Intentionally unchanged (idempotent no-op): MCP project.exit is
        // handled entirely by sapproxy.Router.Unbind on the daemon side and
        // never reaches this method while other sessions may remain bound.
        // sap_close_project exists on the C++ side for a later idle-reset
        // path, not for session-scoped close.
        Ok(())
    }

    fn project_get_state(&mut self, project_id: &str) -> BackendResult<ProjectState> {
        let (undo_depth, redo_depth) = self.undo_redo_depth()?;
        let project_type = match unsafe { ffi::sap_is_project_folder(self.main_window) } {
            1 => "folder".to_string(),
            0 => "file".to_string(),
            _ => String::new(),
        };
        Ok(ProjectState {
            project_id: project_id.to_string(),
            dirty: undo_depth > 0,
            undo_depth,
            redo_depth,
            opened: self.opened,
            project_type,
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
                return Err(BackendError::InvalidParams(format!(
                    "bad track kind: {other}"
                )));
            }
        };
        if index < 0 {
            return Err(BackendError::InvalidParams("failed to add track".into()));
        }
        Ok(Track {
            index: index as usize,
            kind: kind.to_string(),
            name: None,
            muted: false,
            hidden: false,
            locked: false,
            blend_mode: crate::backend::default_blend_mode(),
            composite: false,
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

    fn edit_reorder_track(
        &mut self,
        project_id: &str,
        from_index: usize,
        to_index: usize,
    ) -> BackendResult<Vec<Track>> {
        let rc = unsafe {
            ffi::sap_reorder_track(self.main_window, from_index as c_int, to_index as c_int)
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "track {from_index} or {to_index}"
            )));
        }
        self.edit_list_tracks(project_id)
    }

    fn edit_set_track_properties(
        &mut self,
        project_id: &str,
        track_index: usize,
        muted: Option<bool>,
        hidden: Option<bool>,
        locked: Option<bool>,
        blend_mode: Option<String>,
        composite: Option<bool>,
    ) -> BackendResult<Track> {
        // Real wiring for mute/hidden/locked/composite via MultitrackModel
        // and blendMode via ChangeBlendModeCommand (sap_ffi.cpp).
        if let Some(v) = muted {
            let rc = unsafe {
                ffi::sap_set_track_muted(self.main_window, track_index as c_int, v as c_int)
            };
            if rc != 0 {
                return Err(BackendError::NotFound(format!("track {track_index}")));
            }
        }
        if let Some(v) = hidden {
            let rc = unsafe {
                ffi::sap_set_track_hidden(self.main_window, track_index as c_int, v as c_int)
            };
            if rc != 0 {
                return Err(BackendError::NotFound(format!("track {track_index}")));
            }
        }
        if let Some(v) = locked {
            let rc = unsafe {
                ffi::sap_set_track_locked(self.main_window, track_index as c_int, v as c_int)
            };
            if rc != 0 {
                return Err(BackendError::NotFound(format!("track {track_index}")));
            }
        }
        if let Some(v) = composite {
            let rc = unsafe {
                ffi::sap_set_track_composite(self.main_window, track_index as c_int, v as c_int)
            };
            if rc != 0 {
                return Err(BackendError::NotFound(format!("track {track_index}")));
            }
        }
        if let Some(v) = composite {
            let rc = unsafe {
                ffi::sap_set_track_composite(self.main_window, track_index as c_int, v as c_int)
            };
            if rc != 0 {
                return Err(BackendError::NotFound(format!("track {track_index}")));
            }
        }
        if let Some(v) = blend_mode {
            // Real Timeline::ChangeBlendModeCommand via the qtblend/
            // movit.overlay/cairoblend transition lookup in sap_ffi.cpp
            // (duplicated from TrackPropertiesWidget::getTransition() since
            // MultitrackModel's own lookup is private -- see sap_ffi.cpp).
            let c_mode = CString::new(v)
                .map_err(|e| BackendError::InvalidParams(format!("bad blendMode: {e}")))?;
            let rc = unsafe {
                ffi::sap_set_track_blend_mode(
                    self.main_window,
                    track_index as c_int,
                    c_mode.as_ptr(),
                )
            };
            if rc != 0 {
                return Err(BackendError::NotFound(format!(
                    "track {track_index} has no blend transition"
                )));
            }
        }
        // `sap_list_tracks` now reads muted/hidden/locked/composite back from
        // the real MultitrackModel roles (genuine current Qt/MLT state).
        let tracks = self.edit_list_tracks(project_id)?;
        tracks
            .into_iter()
            .find(|t| t.index == track_index)
            .ok_or_else(|| BackendError::NotFound(format!("track {track_index}")))
    }

    fn edit_set_track_height(&mut self, _project_id: &str, height: i64) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_set_track_height(self.main_window, height as c_int) };
        if rc != 0 {
            return Err(BackendError::NotFound("track height unavailable".into()));
        }
        Ok(())
    }

    fn edit_remove_clip(
        &mut self,
        _project_id: &str,
        track_index: usize,
        clip_index: usize,
    ) -> BackendResult<()> {
        let rc = unsafe {
            ffi::sap_remove_clip(self.main_window, track_index as c_int, clip_index as c_int)
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "clip {track_index}/{clip_index}"
            )));
        }
        Ok(())
    }

    fn edit_move_clip(
        &mut self,
        _project_id: &str,
        from_track_index: usize,
        from_clip_index: usize,
        to_track_index: usize,
        to_clip_index: usize,
    ) -> BackendResult<Clip> {
        // Protocol-level edit.moveClip has no ripple param (see
        // server.rs/backend.rs) -- MockBackend/MltBackend's Vec-based move
        // semantics are non-rippling too, so pass ripple=false here to
        // match that behavior rather than shifting downstream clips.
        let raw = unsafe {
            ffi::sap_move_clip(
                self.main_window,
                from_track_index as c_int,
                from_clip_index as c_int,
                to_track_index as c_int,
                to_clip_index as c_int,
                0,
            )
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "moveClip {from_track_index}/{from_clip_index} -> {to_track_index}/{to_clip_index} rejected"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        #[derive(serde::Deserialize)]
        struct MoveClipResult {
            #[serde(rename = "clipId")]
            clip_id: String,
            index: usize,
            #[serde(rename = "inFrame")]
            in_frame: i64,
            #[serde(rename = "outFrame")]
            out_frame: i64,
        }
        let parsed: MoveClipResult = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad moveClip JSON: {e}")))?;
        Ok(Clip {
            clip_id: parsed.clip_id,
            index: parsed.index,
            source: Value::Null,
            in_frame: parsed.in_frame,
            out_frame: parsed.out_frame,
            speed: 1.0,
        })
    }

    fn edit_list_tracks(&mut self, _project_id: &str) -> BackendResult<Vec<Track>> {
        let raw = unsafe { ffi::sap_list_tracks(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::NotFound("track list unavailable".into()));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
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
        // Real wiring: `sap_append_clip`/`sap_append_clip_xml` (sap_ffi.cpp)
        // push the source via the real, undoable Timeline::AppendCommand
        // -- see that file for the full path. Unlike TimelineDock::append()
        // (which only reads the clipboard/"current source"), this takes
        // any of `source`'s three forms directly (see resolve_clip_source).
        let resolved = self.resolve_clip_source(&source)?;
        let raw = match &resolved {
            ClipSourceResolution::Path(c_path) => unsafe {
                ffi::sap_append_clip(self.main_window, track_index as c_int, c_path.as_ptr())
            },
            ClipSourceResolution::Xml(c_xml) => unsafe {
                ffi::sap_append_clip_xml(self.main_window, track_index as c_int, c_xml.as_ptr())
            },
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to append clip from {source} to track {track_index} (invalid track, or source did not resolve to a valid MLT producer)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
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
            speed: 1.0,
        })
    }

    fn edit_insert_clip(
        &mut self,
        _project_id: &str,
        track_index: usize,
        clip_index: usize,
        source: Value,
    ) -> BackendResult<Clip> {
        // Real wiring: `sap_insert_clip`/`sap_insert_clip_xml` (sap_ffi.cpp)
        // push the source via the real, undoable Timeline::InsertCommand
        // -- distinct from sap_append_clip's AppendCommand, this RIPPLES
        // every downstream clip on the track forward, a genuine mid-track
        // splice in one undo step. See resolve_clip_source for the three
        // source forms accepted.
        let resolved = self.resolve_clip_source(&source)?;
        let raw = match &resolved {
            ClipSourceResolution::Path(c_path) => unsafe {
                ffi::sap_insert_clip(
                    self.main_window,
                    track_index as c_int,
                    clip_index as c_int,
                    c_path.as_ptr(),
                )
            },
            ClipSourceResolution::Xml(c_xml) => unsafe {
                ffi::sap_insert_clip_xml(
                    self.main_window,
                    track_index as c_int,
                    clip_index as c_int,
                    c_xml.as_ptr(),
                )
            },
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to insert clip from {source} at {track_index}/{clip_index} (invalid track/clipIndex, locked track, or source did not resolve to a valid MLT producer)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InsertedClip {
            clip_id: String,
            index: usize,
            in_frame: i64,
            out_frame: i64,
        }
        let inserted: InsertedClip = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad insert-clip JSON: {e}")))?;

        Ok(Clip {
            clip_id: inserted.clip_id,
            index: inserted.index,
            source,
            in_frame: inserted.in_frame,
            out_frame: inserted.out_frame,
            speed: 1.0,
        })
    }

    fn edit_overwrite_clip(
        &mut self,
        _project_id: &str,
        track_index: usize,
        clip_index: usize,
        source: Value,
    ) -> BackendResult<Clip> {
        // Real wiring: `sap_overwrite_clip`/`sap_overwrite_clip_xml`
        // (sap_ffi.cpp) push the source via the real, undoable
        // Timeline::OverwriteCommand -- distinct from sap_insert_clip's
        // InsertCommand, this does NOT ripple downstream clips; it drops
        // and replaces whatever occupies clip-slot `clipIndex`. See
        // resolve_clip_source for the three source forms accepted.
        let resolved = self.resolve_clip_source(&source)?;
        let raw = match &resolved {
            ClipSourceResolution::Path(c_path) => unsafe {
                ffi::sap_overwrite_clip(
                    self.main_window,
                    track_index as c_int,
                    clip_index as c_int,
                    c_path.as_ptr(),
                )
            },
            ClipSourceResolution::Xml(c_xml) => unsafe {
                ffi::sap_overwrite_clip_xml(
                    self.main_window,
                    track_index as c_int,
                    clip_index as c_int,
                    c_xml.as_ptr(),
                )
            },
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to overwrite clip at {track_index}/{clip_index} with {source} (invalid track/clipIndex, locked track, or source did not resolve to a valid MLT producer)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OverwrittenClip {
            clip_id: String,
            index: usize,
            in_frame: i64,
            out_frame: i64,
        }
        let overwritten: OverwrittenClip = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad overwrite-clip JSON: {e}")))?;

        Ok(Clip {
            clip_id: overwritten.clip_id,
            index: overwritten.index,
            source,
            in_frame: overwritten.in_frame,
            out_frame: overwritten.out_frame,
            speed: 1.0,
        })
    }

    fn edit_list_clips(
        &mut self,
        _project_id: &str,
        track_index: usize,
    ) -> BackendResult<Vec<Clip>> {
        let raw = unsafe { ffi::sap_list_clips(self.main_window, track_index as c_int) };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!("track {track_index}")));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawClip {
            clip_id: String,
            index: usize,
            path: String,
            in_frame: i64,
            out_frame: i64,
            #[serde(default = "default_clip_speed")]
            speed: f64,
        }
        fn default_clip_speed() -> f64 { 1.0 }
        let raw_clips: Vec<RawClip> = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad clip-list JSON: {e}")))?;
        Ok(raw_clips
            .into_iter()
            .map(|c| Clip {
                clip_id: c.clip_id,
                index: c.index,
                source: json!({"path": c.path}),
                in_frame: c.in_frame,
                out_frame: c.out_frame,
                speed: c.speed,
            })
            .collect())
    }

    fn playback_seek(&mut self, _project_id: &str, frame: i64) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_playback_seek(self.main_window, frame as _) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidParams("playback seek failed".into()))
        }
    }

    fn playback_fast_forward(&mut self, _project_id: &str) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_playback_fast_forward(self.main_window) };
        if rc == 0 { Ok(()) } else { Err(BackendError::InvalidParams("playback fast-forward failed".into())) }
    }

    fn edit_set_clip_speed(&mut self, _project_id: &str, track_index: usize, clip_index: usize, speed: f64) -> BackendResult<Clip> {
        if !speed.is_finite() || speed <= 0.0 { return Err(BackendError::InvalidParams("speed must be finite and greater than zero".into())); }
        let raw = unsafe { ffi::sap_set_clip_speed(self.main_window, track_index as c_int, clip_index as c_int, speed) };
        if raw.is_null() { return Err(BackendError::InvalidParams("set clip speed failed".into())); }
        let text = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<Clip>(&text).map_err(|e| BackendError::InvalidParams(format!("bad set speed JSON: {e}")))
    }

    fn playback_play(&mut self, _project_id: &str, speed: f64) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_playback_play(self.main_window, speed) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidParams("playback.play failed".into()))
        }
    }

    fn playback_pause(&mut self, _project_id: &str, position: Option<i64>) -> BackendResult<()> {
        let pos = position.unwrap_or(-1);
        let rc = unsafe { ffi::sap_playback_pause(self.main_window, pos as _) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidParams("playback.pause failed".into()))
        }
    }

    fn playback_stop(&mut self, _project_id: &str) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_playback_stop(self.main_window) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidParams("playback.stop failed".into()))
        }
    }

    fn playback_get_state(&mut self, _project_id: &str) -> BackendResult<PlaybackState> {
        let raw = unsafe { ffi::sap_playback_get_state(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(
                "playback.getState failed".into(),
            ));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad playback state JSON: {e}")))
    }

    fn project_set_profile(
        &mut self,
        _project_id: &str,
        profile_name: Option<String>,
        width: Option<i32>,
        height: Option<i32>,
        frame_rate_num: Option<i32>,
        frame_rate_den: Option<i32>,
    ) -> BackendResult<ProfileInfo> {
        let name_c = profile_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(CString::new)
            .transpose()
            .map_err(|e| BackendError::InvalidParams(format!("bad profileName: {e}")))?;
        let name_ptr = name_c
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null());
        let w = width.unwrap_or(0);
        let h = height.unwrap_or(0);
        let fps_n = frame_rate_num.unwrap_or(0);
        let fps_d = frame_rate_den.unwrap_or(0);
        if name_c.is_none() && (w <= 0 || h <= 0) {
            return Err(BackendError::InvalidParams(
                "project.setProfile requires profileName or width+height".into(),
            ));
        }
        let rc = unsafe {
            ffi::sap_set_profile(self.main_window, name_ptr, w, h, fps_n, fps_d)
        };
        if rc != 0 {
            return Err(BackendError::InvalidParams(
                "project.setProfile failed".into(),
            ));
        }
        self.project_get_profile(_project_id)
    }

    fn project_get_profile(&mut self, _project_id: &str) -> BackendResult<ProfileInfo> {
        let raw = unsafe { ffi::sap_get_profile(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(
                "project.getProfile failed".into(),
            ));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad profile JSON: {e}")))
    }

    fn notes_get_text(&mut self, _project_id: &str) -> BackendResult<String> {
        let raw = unsafe { ffi::sap_notes_get_text(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(
                "notes.getText failed (invalid handle)".into(),
            ));
        }
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        Ok(text)
    }

    fn notes_set_text(&mut self, _project_id: &str, text: &str) -> BackendResult<()> {
        let c_text = CString::new(text)
            .map_err(|e| BackendError::InvalidParams(format!("bad text: {e}")))?;
        let rc = unsafe { ffi::sap_notes_set_text(self.main_window, c_text.as_ptr()) };
        if rc != 0 {
            return Err(BackendError::InvalidParams(
                "notes.setText failed (invalid handle)".into(),
            ));
        }
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
        source: Value,
        _name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        // `_name` is intentionally ignored -- real playlist entries derive
        // their display name from the live shotcut:caption MLT property or
        // the resource's file basename (see playlistEntryToJson in
        // sap_ffi.cpp), same as MltBackend accepting but not honoring it
        // for probe-derived entries.
        let path = source
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BackendError::InvalidParams("source must be {path: ...}".into()))?;
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("invalid source path: {e}")))?;
        let raw = unsafe { ffi::sap_playlist_append(self.main_window, c_path.as_ptr()) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to append {path} to playlist (did not open as a valid MLT producer)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        Self::parse_playlist_entry(&json_str, source)
    }

    fn playlist_append_image_sequence(
        &mut self,
        _project_id: &str,
        path: &str,
        ttl: Option<i32>,
        begin: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("invalid path: {e}")))?;
        let c_begin = begin
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(CString::new)
            .transpose()
            .map_err(|e| BackendError::InvalidParams(format!("bad begin: {e}")))?;
        let begin_ptr = c_begin
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null());
        let ttl_i = ttl.unwrap_or(1);
        let raw = unsafe {
            ffi::sap_playlist_append_image_sequence(
                self.main_window,
                c_path.as_ptr(),
                ttl_i as c_int,
                begin_ptr,
            )
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to append image sequence from {path}"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        Self::parse_playlist_entry(
            &json_str,
            json!({
                "path": path,
                "imageSequence": true,
                "ttl": ttl_i,
                "begin": begin,
            }),
        )
    }

    fn playlist_list(&mut self, _project_id: &str) -> BackendResult<Vec<PlaylistEntry>> {
        let raw = unsafe { ffi::sap_playlist_list(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::NotFound("playlist unavailable".into()));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        let entries: Vec<Value> = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad playlist-list JSON: {e}")))?;
        entries
            .into_iter()
            .map(|entry| {
                let path = entry
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Self::parse_playlist_entry_value(entry, json!({"path": path}))
            })
            .collect()
    }

    // --- Minimal stubs for the new playlist.* trait methods (task: keep
    // this file's changes to an absolute minimum -- these are explicit
    // NotFound entries, not real Qt/MLT wiring, same honesty policy as
    // playlist_append/file_import above; playlist.addToTimeline has no
    // trait method at all, see backend.rs's comment on that). ---

    fn playlist_insert(
        &mut self,
        _project_id: &str,
        index: usize,
        source: Value,
        _name: Option<String>,
    ) -> BackendResult<PlaylistEntry> {
        let path = source
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BackendError::InvalidParams("source must be {path: ...}".into()))?;
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("invalid source path: {e}")))?;
        let raw =
            unsafe { ffi::sap_playlist_insert(self.main_window, index as c_int, c_path.as_ptr()) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "failed to insert {path} at playlist index {index} (out of range, or not a valid MLT producer)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        Self::parse_playlist_entry(&json_str, source)
    }

    fn playlist_remove(&mut self, _project_id: &str, index: usize) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_playlist_remove(self.main_window, index as c_int) };
        if rc != 0 {
            return Err(BackendError::NotFound(format!("playlist index {index}")));
        }
        Ok(())
    }

    fn playlist_move(
        &mut self,
        _project_id: &str,
        from_index: usize,
        to_index: usize,
    ) -> BackendResult<()> {
        let rc = unsafe {
            ffi::sap_playlist_move(self.main_window, from_index as c_int, to_index as c_int)
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "playlist index {from_index} or {to_index}"
            )));
        }
        Ok(())
    }

    fn playlist_get(
        &mut self,
        _project_id: &str,
        index: usize,
    ) -> BackendResult<PlaylistEntryDetail> {
        let raw = unsafe { ffi::sap_playlist_get(self.main_window, index as c_int) };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!("playlist index {index}")));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        let value: Value = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad playlist-get JSON: {e}")))?;
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let entry = Self::parse_playlist_entry_value(value, json!({"path": path.clone()}))?;
        // Reuse the same real ffprobe-backed helper file.probe uses -- only
        // meaningful for file-backed sources, same honesty policy as
        // MltBackend::playlist_get (a generator/title/blank-spacer entry
        // has no real file to probe, so `probe` is honestly `None`).
        let probe = if path.is_empty() {
            None
        } else {
            crate::media_tools::probe_media(&path).ok()
        };
        Ok(PlaylistEntryDetail {
            index: entry.index,
            name: entry.name,
            source: entry.source,
            duration_frames: entry.duration_frames,
            probe,
        })
    }

    fn file_import(&mut self, project_id: &str, path: &str) -> BackendResult<PlaylistEntry> {
        // Per-bound-project sandbox, ported from the removed MltBackend's
        // identical file_import check (git history:
        // sap-rust/src/mlt_backend.rs) -- a path that resolves outside
        // SNAPSHOT_PROJECT_ROOT is rejected even if it exists and is
        // readable, so one session can never read another session's (or
        // an arbitrary filesystem) files through this call. Skipped
        // entirely when project_root is unset (manual dev launch outside
        // snapshotd).
        let canonical_path = if let Some(project_root) = &self.project_root {
            let canonical_root = fs::canonicalize(project_root).map_err(|e| {
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
                BackendError::InvalidParams(format!(
                    "file.import path {} is not readable: {e}",
                    candidate.display()
                ))
            })?;
            if !canonical_path.starts_with(&canonical_root) {
                return Err(BackendError::InvalidParams(format!(
                    "file.import path {} is outside project root {}",
                    canonical_path.display(),
                    canonical_root.display()
                )));
            }
            canonical_path.to_string_lossy().into_owned()
        } else {
            path.to_string()
        };
        self.playlist_append(project_id, json!({"path": canonical_path}), None)
    }

    fn edit_trim_clip_in(
        &mut self,
        _project_id: &str,
        track_index: usize,
        clip_index: usize,
        new_frame: i64,
        ripple: bool,
    ) -> BackendResult<()> {
        let rc = unsafe {
            ffi::sap_trim_clip_in(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                new_frame as i64,
                ripple as c_int,
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "clip {track_index}/{clip_index} unavailable, or newFrame {new_frame} out of range"
            )));
        }
        Ok(())
    }

    fn edit_trim_clip_out(
        &mut self,
        _project_id: &str,
        track_index: usize,
        clip_index: usize,
        new_frame: i64,
        ripple: bool,
    ) -> BackendResult<()> {
        let rc = unsafe {
            ffi::sap_trim_clip_out(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                new_frame as i64,
                ripple as c_int,
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "clip {track_index}/{clip_index} unavailable, or newFrame {new_frame} out of range"
            )));
        }
        Ok(())
    }

    fn edit_split_clip(
        &mut self,
        _project_id: &str,
        track_index: usize,
        clip_index: usize,
        position: i64,
    ) -> BackendResult<SplitClipResult> {
        let raw = unsafe {
            ffi::sap_split_clip(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                position,
            )
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "split of clip {track_index}/{clip_index} at {position} rejected (invalid clip, or position not inside the clip)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawSplit {
            left_clip_id: String,
            right_clip_id: String,
            left_index: usize,
            right_index: usize,
        }
        let parsed: RawSplit = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad split-clip JSON: {e}")))?;
        Ok(SplitClipResult {
            left_clip_id: parsed.left_clip_id,
            right_clip_id: parsed.right_clip_id,
            left_index: parsed.left_index,
            right_index: parsed.right_index,
        })
    }

    fn transitions_add_crossfade(
        &mut self,
        _project_id: &str,
        track_index: usize,
        between_clips: (usize, usize),
        duration_frames: i64,
    ) -> BackendResult<TransitionInfo> {
        if between_clips.1 != between_clips.0 + 1 {
            return Err(BackendError::InvalidParams(
                "transitions.addCrossfade requires adjacent clip indices".into(),
            ));
        }
        if duration_frames <= 0 {
            return Err(BackendError::InvalidParams(
                "durationFrames must be positive".into(),
            ));
        }
        let raw = unsafe {
            ffi::sap_transitions_add_crossfade(
                self.main_window,
                track_index as c_int,
                between_clips.0 as c_int,
                between_clips.1 as c_int,
                duration_frames,
            )
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "crossfade between clips {}/{} on track {track_index} rejected (locked track, or \
                 durationFrames >= either clip's length)",
                between_clips.0, between_clips.1
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<TransitionInfo>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad crossfade JSON: {e}")))
    }

    fn filter_add(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        mlt_service: &str,
        properties: Value,
    ) -> BackendResult<FilterInfo> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let c_service = CString::new(mlt_service)
            .map_err(|e| BackendError::InvalidParams(format!("bad mltService: {e}")))?;
        let props_json = serde_json::to_string(&properties)
            .map_err(|e| BackendError::InvalidParams(format!("bad properties: {e}")))?;
        let c_props = CString::new(props_json)
            .map_err(|e| BackendError::InvalidParams(format!("bad properties: {e}")))?;
        let raw = unsafe {
            ffi::sap_filter_add(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                c_service.as_ptr(),
                c_props.as_ptr(),
            )
        };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!(
                "failed to attach filter {mlt_service} to clip {clip_id}"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<FilterInfo>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad filter-add JSON: {e}")))
    }

    fn filter_set_property(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        value: Value,
        position: Option<i64>,
    ) -> BackendResult<()> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let c_property = CString::new(property)
            .map_err(|e| BackendError::InvalidParams(format!("bad property name: {e}")))?;
        let value_json = serde_json::to_string(&value)
            .map_err(|e| BackendError::InvalidParams(format!("bad value: {e}")))?;
        let c_value = CString::new(value_json)
            .map_err(|e| BackendError::InvalidParams(format!("bad value: {e}")))?;
        let rc = unsafe {
            ffi::sap_filter_set_property(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                filter_index as c_int,
                c_property.as_ptr(),
                c_value.as_ptr(),
                position.unwrap_or(-1) as c_longlong,
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "filter {filter_index} on clip {clip_id} unavailable"
            )));
        }
        Ok(())
    }

    fn filter_add_keyframe(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        position: i64,
        value: Value,
        interpolation: &str,
    ) -> BackendResult<()> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let c_property = CString::new(property)
            .map_err(|e| BackendError::InvalidParams(format!("bad property name: {e}")))?;
        let value_json = serde_json::to_string(&value)
            .map_err(|e| BackendError::InvalidParams(format!("bad value: {e}")))?;
        let c_value = CString::new(value_json)
            .map_err(|e| BackendError::InvalidParams(format!("bad value: {e}")))?;
        let c_interp = CString::new(interpolation)
            .map_err(|e| BackendError::InvalidParams(format!("bad interpolation: {e}")))?;
        let rc = unsafe {
            ffi::sap_filter_add_keyframe(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                filter_index as c_int,
                c_property.as_ptr(),
                position as c_longlong,
                c_value.as_ptr(),
                c_interp.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "filter {filter_index} on clip {clip_id} unavailable, or bad keyframe value"
            )));
        }
        Ok(())
    }

    fn filter_list(
        &mut self,
        _project_id: &str,
        clip_id: &str,
    ) -> BackendResult<Vec<FilterListEntry>> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let raw = unsafe {
            ffi::sap_filter_list(self.main_window, track_index as c_int, clip_index as c_int)
        };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!(
                "clip {clip_id} unavailable"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawFilterEntry {
            filter_index: usize,
            mlt_service: String,
            #[serde(default)]
            properties: Value,
        }
        let raw_entries: Vec<RawFilterEntry> = serde_json::from_str(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad filter-list JSON: {e}")))?;
        // `properties` is the real, live-read MLT property store for each
        // attached filter (sap_ffi.cpp's filterUserPropertiesToJson), with
        // reserved/internal MLT keys (mlt_type, mlt_service, in, out,
        // _unique_id, ...) filtered out -- not an echo of whatever
        // filter.add/filter.setProperty happened to be called with.
        Ok(raw_entries
            .into_iter()
            .map(|e| FilterListEntry {
                index: e.filter_index,
                mlt_service: e.mlt_service,
                properties: e.properties,
            })
            .collect())
    }

    fn filter_remove(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        filter_index: usize,
    ) -> BackendResult<()> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let rc = unsafe {
            ffi::sap_filter_remove(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                filter_index as c_int,
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "filter {filter_index} on clip {clip_id} unavailable"
            )));
        }
        Ok(())
    }

    fn filter_reorder(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        filter_index: usize,
        new_index: usize,
    ) -> BackendResult<()> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let rc = unsafe {
            ffi::sap_filter_reorder(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                filter_index as c_int,
                new_index as c_int,
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "filter reorder {filter_index}->{new_index} on clip {clip_id} unavailable"
            )));
        }
        Ok(())
    }

    fn filter_list_keyframes(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
    ) -> BackendResult<Vec<KeyframeInfo>> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let c_property = CString::new(property)
            .map_err(|e| BackendError::InvalidParams(format!("bad property name: {e}")))?;
        let raw = unsafe {
            ffi::sap_filter_list_keyframes(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                filter_index as c_int,
                c_property.as_ptr(),
            )
        };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!(
                "filter {filter_index} on clip {clip_id} unavailable"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<Vec<KeyframeInfo>>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad keyframe-list JSON: {e}")))
    }

    fn filter_remove_keyframe(
        &mut self,
        _project_id: &str,
        clip_id: &str,
        filter_index: usize,
        property: &str,
        position: i64,
    ) -> BackendResult<()> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let c_property = CString::new(property)
            .map_err(|e| BackendError::InvalidParams(format!("bad property name: {e}")))?;
        let rc = unsafe {
            ffi::sap_filter_remove_keyframe(
                self.main_window,
                track_index as c_int,
                clip_index as c_int,
                filter_index as c_int,
                c_property.as_ptr(),
                position as c_longlong,
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "no keyframe at position {position} for filter {filter_index} property {property} on clip {clip_id}"
            )));
        }
        Ok(())
    }

    fn clip_length_frames(&mut self, _project_id: &str, clip_id: &str) -> BackendResult<i64> {
        let (track_index, clip_index) = Self::parse_clip_id(clip_id)?;
        let frames = unsafe {
            ffi::sap_clip_length_frames(self.main_window, track_index as c_int, clip_index as c_int)
        };
        if frames < 0 {
            return Err(BackendError::NotFound(format!(
                "clip {clip_id} unavailable"
            )));
        }
        Ok(frames)
    }

    fn generator_create_title(
        &mut self,
        _project_id: &str,
        params: Value,
    ) -> BackendResult<PlaylistEntry> {
        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("simple")
            .to_string();
        let text = params
            .get("text")
            .or_else(|| params.get("html"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BackendError::InvalidParams("generator.createTitle requires text (or html)".into())
            })?
            .to_string();
        let fg = params
            .get("fgColour")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let bg = params
            .get("bgColour")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let c_mode = CString::new(mode)
            .map_err(|e| BackendError::InvalidParams(format!("bad mode: {e}")))?;
        let c_text = CString::new(text)
            .map_err(|e| BackendError::InvalidParams(format!("bad text: {e}")))?;
        let c_fg = fg
            .map(CString::new)
            .transpose()
            .map_err(|e| BackendError::InvalidParams(format!("bad fgColour: {e}")))?;
        let c_bg = bg
            .map(CString::new)
            .transpose()
            .map_err(|e| BackendError::InvalidParams(format!("bad bgColour: {e}")))?;
        let raw = unsafe {
            ffi::sap_generator_create_title(
                self.main_window,
                c_mode.as_ptr(),
                c_text.as_ptr(),
                c_fg.as_ref()
                    .map(|s| s.as_ptr())
                    .unwrap_or(std::ptr::null()),
                c_bg.as_ref()
                    .map(|s| s.as_ptr())
                    .unwrap_or(std::ptr::null()),
            )
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(
                "generator.createTitle failed (no playlist bin?)".into(),
            ));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<PlaylistEntry>(&json_str).map_err(|e| {
            BackendError::InvalidParams(format!("bad generator-create-title JSON: {e}"))
        })
    }

    fn generator_create_color(
        &mut self,
        _project_id: &str,
        params: Value,
    ) -> BackendResult<PlaylistEntry> {
        let hex = params
            .get("hexColor")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BackendError::InvalidParams("generator.createColor requires hexColor".into())
            })?
            .to_string();
        let c_hex = CString::new(hex)
            .map_err(|e| BackendError::InvalidParams(format!("bad hexColor: {e}")))?;
        let raw = unsafe { ffi::sap_generator_create_color(self.main_window, c_hex.as_ptr()) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(
                "generator.createColor failed (no playlist bin?)".into(),
            ));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<PlaylistEntry>(&json_str).map_err(|e| {
            BackendError::InvalidParams(format!("bad generator-create-color JSON: {e}"))
        })
    }

    fn subtitles_add_track(&mut self, _project_id: &str) -> BackendResult<SubtitleTrackInfo> {
        let raw = unsafe { ffi::sap_subtitles_add_track(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::NotFound(
                "subtitles.addTrack unavailable (no clip on the timeline yet?)".into(),
            ));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<SubtitleTrackInfo>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad subtitles-add-track JSON: {e}")))
    }

    fn subtitles_append_item(
        &mut self,
        _project_id: &str,
        track_index: usize,
        start_frame: i64,
        end_frame: i64,
        text: &str,
    ) -> BackendResult<()> {
        let c_text = CString::new(text)
            .map_err(|e| BackendError::InvalidParams(format!("bad text: {e}")))?;
        let rc = unsafe {
            ffi::sap_subtitles_append_item(
                self.main_window,
                track_index as c_int,
                start_frame as c_longlong,
                end_frame as c_longlong,
                c_text.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "subtitle track {track_index} unavailable"
            )));
        }
        Ok(())
    }

    fn subtitles_remove_items(
        &mut self,
        _project_id: &str,
        track_index: usize,
        item_indices: &[usize],
    ) -> BackendResult<()> {
        let indices_json = serde_json::to_string(item_indices)
            .map_err(|e| BackendError::InvalidParams(format!("bad item_indices: {e}")))?;
        let c_indices = CString::new(indices_json)
            .map_err(|e| BackendError::InvalidParams(format!("bad item_indices: {e}")))?;
        let rc = unsafe {
            ffi::sap_subtitles_remove_items(
                self.main_window,
                track_index as c_int,
                c_indices.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(BackendError::InvalidParams(format!(
                "subtitles.removeItems failed for track {track_index} (out-of-range index, or a \
                 non-contiguous index set -- the real SubtitlesModel::removeItems() only supports \
                 removing one contiguous run at a time)"
            )));
        }
        Ok(())
    }

    fn subtitles_import_srt(
        &mut self,
        _project_id: &str,
        path: &str,
        new_track: bool,
    ) -> BackendResult<SubtitleTrackInfo> {
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("bad path: {e}")))?;
        let raw = unsafe {
            ffi::sap_subtitles_import_srt(self.main_window, c_path.as_ptr(), new_track as c_int)
        };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(format!(
                "subtitles.importSrt failed for {path} (unreadable, no cues, or no timeline clip yet)"
            )));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<SubtitleTrackInfo>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad subtitles-import-srt JSON: {e}")))
    }

    fn subtitles_export_srt(
        &mut self,
        _project_id: &str,
        path: &str,
        track_index: usize,
    ) -> BackendResult<String> {
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("bad path: {e}")))?;
        let raw = unsafe {
            ffi::sap_subtitles_export_srt(self.main_window, track_index as c_int, c_path.as_ptr())
        };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!(
                "subtitle track {track_index} unavailable"
            )));
        }
        let out = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        Ok(out)
    }

    fn subtitles_burn_in(&mut self, _project_id: &str, track_index: usize) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_subtitles_burn_in(self.main_window, track_index as c_int) };
        if rc != 0 {
            return Err(BackendError::NotFound(format!(
                "subtitle track {track_index} unavailable"
            )));
        }
        Ok(())
    }

    fn subtitles_set_style(&mut self, _project_id: &str, style: Value) -> BackendResult<()> {
        let json = serde_json::to_string(&style)
            .map_err(|e| BackendError::InvalidParams(format!("bad subtitle style: {e}")))?;
        let c_json = CString::new(json)
            .map_err(|e| BackendError::InvalidParams(format!("bad subtitle style: {e}")))?;
        let rc = unsafe { ffi::sap_subtitles_set_style(self.main_window, c_json.as_ptr()) };
        if rc != 0 {
            return Err(BackendError::NotFound(
                "no tractor-level subtitle filter is attached".into(),
            ));
        }
        Ok(())
    }

    fn file_export(
        &mut self,
        _project_id: &str,
        output_path: &str,
        codec: &str,
        container: &str,
    ) -> BackendResult<String> {
        // Real MLT XML of the actual live project (via the real "Save As"
        // primitive, sap_export_project_xml), written to a scratch dir
        // rather than the project's own file -- exporting must never
        // clobber whatever the user has open.
        let scratch_dir =
            std::env::temp_dir().join(format!("sap-ffi-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch_dir).map_err(|e| {
            BackendError::InvalidParams(format!("failed to create export scratch dir: {e}"))
        })?;
        let mlt_path = scratch_dir.join("project.mlt");
        let c_mlt_path = CString::new(mlt_path.to_string_lossy().into_owned())
            .map_err(|e| BackendError::InvalidParams(format!("bad scratch path: {e}")))?;
        let rc = unsafe { ffi::sap_export_project_xml(self.main_window, c_mlt_path.as_ptr()) };
        if rc != 0 {
            return Err(BackendError::InvalidParams(
                "failed to export the current project to MLT XML (no clips/producer open?)".into(),
            ));
        }

        let resolved_output = {
            let p = std::path::Path::new(output_path);
            let mut resolved = if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(root) = self.project_root.as_ref() {
                // A relative outputPath resolves against the bound
                // project's own root -- the same sandbox root file.import
                // is confined to -- NOT the daemon process's cwd. Found
                // live (phase 14b/15b of the video-generation-e2e-harness
                // plan): a real agent asked for "agent_export.mp4" and the
                // file landed silently next to the snapshotd binary, which
                // the project can neither list nor clean up.
                root.join(output_path)
            } else {
                // Manual dev launch outside the daemon (no
                // SNAPSHOT_PROJECT_ROOT): cwd is the only sensible base.
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(output_path)
            };
            if resolved.extension().is_none() {
                resolved.set_extension(if container.is_empty() {
                    "mp4"
                } else {
                    container
                });
            }
            resolved
        };
        if let Some(parent) = resolved_output.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BackendError::InvalidParams(format!("failed to create export dir: {e}"))
            })?;
        }

        let vcodec = crate::media_tools::normalize_vcodec(codec);
        let melt_bin = crate::media_tools::resolve_melt_binary();
        let qt_platform =
            std::env::var("QT_QPA_PLATFORM").unwrap_or_else(|_| "offscreen".to_string());

        let mut cmd = Command::new(&melt_bin);
        cmd.arg(&mlt_path)
            .arg("-consumer")
            .arg(format!("avformat:{}", resolved_output.display()))
            .arg(format!("vcodec={vcodec}"))
            .arg("acodec=aac")
            .env("QT_QPA_PLATFORM", &qt_platform)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Only forward DISPLAY when the melt child is explicitly NOT running
        // offscreen. avformat file export needs no display of its own --
        // forwarding the parent process's real DISPLAY (set whenever the
        // *editor* itself was launched headed, i.e. daemon.launch with
        // headless=false) hands melt a live, actively-rendering X server
        // that a concurrently-open Shotcut GUI window is also using.
        // Confirmed by hand: exporting this way reproducibly stalls melt
        // indefinitely (ffmpeg's VAAPI/DRM probing threads deadlock in
        // futex_do_wait on that shared display); the identical export with
        // DISPLAY unset completes cleanly in well under two minutes.
        if qt_platform != "offscreen" {
            if let Ok(display) = std::env::var("DISPLAY") {
                cmd.env("DISPLAY", display);
            }
        } else {
            cmd.env_remove("DISPLAY");
        }
        reset_child_signals(&mut cmd);

        let (child, stderr_buf, heartbeat) = spawn_melt_draining_stderr(cmd).map_err(|e| {
            BackendError::InvalidParams(format!(
                "failed to spawn `{melt_bin}`: {e} (is melt on PATH, or MELT_BIN set?)"
            ))
        })?;

        let job_id = uuid::Uuid::new_v4().to_string();
        let evicted = {
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
            // Shares `MltBackend`'s eviction policy/constant rather than
            // duplicating it -- see `prune_finished_jobs`'s doc comment in
            // `mlt_backend.rs` for why this map needs bounding at all
            // (this backend's `jobs`/`job_children` have the exact same
            // unbounded-growth shape MltBackend's did).
            crate::media_tools::prune_finished_jobs(&mut jobs)
        };
        for id in &evicted {
            self.job_children.remove(id);
        }

        // Same kill-handle-plus-polling-thread shape as MltBackend::
        // file_export -- see there for the full rationale (either
        // jobs_stop or the poller can claim the Child; whichever gets
        // there first wins).
        let child_slot = Arc::new(Mutex::new(Some(child)));
        self.job_children.insert(job_id.clone(), child_slot.clone());

        let jobs = self.jobs.clone();
        let job_id_bg = job_id.clone();
        // Melt may remain quiet while constructing a large first frame. Its
        // stdout/stderr are drained continuously by the helper above, so the
        // heartbeat pipes cannot deadlock the encoder. Completion or explicit
        // jobs.stop are the terminal conditions; there is no wall-clock
        // deadline to kill a valid long render.
        std::thread::spawn(move || {
            let mut heartbeat_seen = heartbeat.load(Ordering::Relaxed);
            let outcome = loop {
                let mut guard = child_slot.lock().expect("job child mutex poisoned");
                match guard.as_mut() {
                    None => return, // jobs_stop already took it and set status.
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => {
                            let _finished = guard.take().expect("child present after try_wait");
                            let stderr = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
                            break Ok((status, stderr));
                        }
                        Ok(None) => {
                            // A relaxed atomic read is the complete heartbeat
                            // path: no rendering work, allocation, or timer.
                            let current = heartbeat.load(Ordering::Relaxed);
                            if current != heartbeat_seen {
                                heartbeat_seen = current;
                            }
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
            let mut completed = None;
            if let Some(job) = jobs.get_mut(&job_id_bg) {
                if job.status != "running" {
                    return; // Don't overwrite a client-initiated stop.
                }
                match outcome {
                    Ok((status, stderr)) if status.success() => {
                        if let Some(bad) = crate::media_tools::detect_unrecognised_codec(&stderr) {
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
                completed = Some(job.clone());
            }
            drop(jobs);
            if let Some(completed) = completed {
                publish_export_notification(&completed);
            }
        });

        Ok(job_id)
    }

    fn file_probe(&mut self, path: &str) -> BackendResult<FileProbe> {
        // Pure ffprobe-based probing, zero Qt/MLT dependency -- identical
        // logic to `MltBackend::file_probe`, so reuse it directly rather
        // than duplicating.
        crate::media_tools::probe_media(path)
    }

    fn jobs_get(&mut self, _job_id: &str) -> BackendResult<JobStatus> {
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .get(_job_id)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(format!("job {_job_id}")))
    }

    fn jobs_list(&mut self, _project_id: &str) -> BackendResult<Vec<JobStatus>> {
        // No per-project routing to filter by (see the `FfiBackend` doc
        // comment -- the embedded process has exactly one live project),
        // so this returns every job this backend has ever spawned, unlike
        // MltBackend's project-scoped filter.
        let mut jobs: Vec<JobStatus> = self
            .jobs
            .lock()
            .expect("jobs mutex poisoned")
            .values()
            .cloned()
            .collect();
        jobs.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        Ok(jobs)
    }

    fn jobs_stop(&mut self, _job_id: &str) -> BackendResult<()> {
        let stopped = {
            let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
            let job = jobs
                .get_mut(_job_id)
                .ok_or_else(|| BackendError::NotFound(format!("job {_job_id}")))?;
            if job.status != "running" {
                return Ok(()); // Already terminal -- idempotent success.
            }
            job.status = "stopped".into();
            job.error = Some("stopped by client".into());
            job.clone()
        };
        publish_export_notification(&stopped);
        if let Some(slot) = self.job_children.remove(_job_id) {
            if let Some(mut child) = slot.lock().expect("job child mutex poisoned").take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Ok(())
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
            ffi::sap_get_frame(
                self.main_window,
                frame,
                c_format.as_ptr(),
                &mut out_len as *mut c_int,
            )
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
        frame: i64,
        text: Option<String>,
        color: Option<String>,
    ) -> BackendResult<Marker> {
        // Defaults mirror MltBackend's (backend.rs's Mock/Mlt both use
        // text="" / color="#000000" when unset) so the two backends behave
        // identically for an agent that never passes these optional args.
        let c_text = CString::new(text.unwrap_or_default())
            .map_err(|e| BackendError::InvalidParams(format!("bad text: {e}")))?;
        let c_color = CString::new(color.unwrap_or_else(|| "#000000".to_string()))
            .map_err(|e| BackendError::InvalidParams(format!("bad color: {e}")))?;
        let raw = unsafe {
            ffi::sap_markers_append(self.main_window, frame, c_text.as_ptr(), c_color.as_ptr())
        };
        Self::parse_marker(raw, "markers.append failed")
    }

    fn markers_remove(&mut self, _project_id: &str, marker_index: usize) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_markers_remove(self.main_window, marker_index as c_int) };
        if rc != 0 {
            return Err(BackendError::NotFound(format!("marker {marker_index}")));
        }
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
        // The real `MarkersModel::update()` slot always replaces the full
        // marker (there's no partial setter beyond move()/setColor(), which
        // are their own RPCs) -- so resolve the optional fields against the
        // marker's current state first, then push one full-replace update.
        // `endFrame` is deliberately left untouched here (only
        // `markers.move` changes the range), matching MockBackend/
        // MltBackend's `markers_update`, which never touches `end_frame`.
        let current = self.markers_get(project_id, marker_index)?;
        let resolved_frame = frame.unwrap_or(current.frame);
        let resolved_end = current.end_frame.unwrap_or(current.frame);
        let resolved_text = text.unwrap_or(current.text);
        let resolved_color = color.unwrap_or(current.color);
        let c_text = CString::new(resolved_text)
            .map_err(|e| BackendError::InvalidParams(format!("bad text: {e}")))?;
        let c_color = CString::new(resolved_color)
            .map_err(|e| BackendError::InvalidParams(format!("bad color: {e}")))?;
        let raw = unsafe {
            ffi::sap_markers_update(
                self.main_window,
                marker_index as c_int,
                resolved_frame,
                resolved_end,
                c_text.as_ptr(),
                c_color.as_ptr(),
            )
        };
        Self::parse_marker(raw, &format!("marker {marker_index}"))
    }

    fn markers_move(
        &mut self,
        _project_id: &str,
        marker_index: usize,
        start: i64,
        end: i64,
    ) -> BackendResult<Marker> {
        let raw =
            unsafe { ffi::sap_markers_move(self.main_window, marker_index as c_int, start, end) };
        Self::parse_marker(raw, &format!("marker {marker_index}"))
    }

    fn markers_set_color(
        &mut self,
        _project_id: &str,
        marker_index: usize,
        color: &str,
    ) -> BackendResult<Marker> {
        let c_color = CString::new(color)
            .map_err(|e| BackendError::InvalidParams(format!("bad color: {e}")))?;
        let raw = unsafe {
            ffi::sap_markers_set_color(self.main_window, marker_index as c_int, c_color.as_ptr())
        };
        Self::parse_marker(raw, &format!("marker {marker_index}"))
    }

    fn markers_clear(&mut self, _project_id: &str) -> BackendResult<()> {
        let rc = unsafe { ffi::sap_markers_clear(self.main_window) };
        if rc != 0 {
            return Err(BackendError::NotFound("no active project/timeline".into()));
        }
        Ok(())
    }

    fn markers_list(&mut self, _project_id: &str) -> BackendResult<Vec<Marker>> {
        let raw = unsafe { ffi::sap_markers_list(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::NotFound("no active project/timeline".into()));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<Vec<Marker>>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad markers-list JSON: {e}")))
    }

    fn markers_get(&mut self, _project_id: &str, marker_index: usize) -> BackendResult<Marker> {
        let raw = unsafe { ffi::sap_markers_get(self.main_window, marker_index as c_int) };
        Self::parse_marker(raw, &format!("marker {marker_index}"))
    }

    fn markers_next(&mut self, _project_id: &str, from_frame: i64) -> BackendResult<Option<i64>> {
        let frame = unsafe { ffi::sap_markers_next(self.main_window, from_frame) };
        Ok(if frame < 0 { None } else { Some(frame) })
    }

    fn markers_prev(&mut self, _project_id: &str, from_frame: i64) -> BackendResult<Option<i64>> {
        let frame = unsafe { ffi::sap_markers_prev(self.main_window, from_frame) };
        Ok(if frame < 0 { None } else { Some(frame) })
    }

    fn recent_add(&mut self, _project_id: &str, path: &str) -> BackendResult<()> {
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("bad path: {e}")))?;
        let rc = unsafe { ffi::sap_recent_add(self.main_window, c_path.as_ptr()) };
        if rc != 0 {
            return Err(BackendError::InvalidParams(
                "recent.add failed (invalid handle)".into(),
            ));
        }
        Ok(())
    }

    fn recent_remove(&mut self, _project_id: &str, path: &str) -> BackendResult<String> {
        let c_path = CString::new(path)
            .map_err(|e| BackendError::InvalidParams(format!("bad path: {e}")))?;
        let raw = unsafe { ffi::sap_recent_remove(self.main_window, c_path.as_ptr()) };
        if raw.is_null() {
            return Err(BackendError::NotFound(format!("recent path {path}")));
        }
        let out = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        Ok(out)
    }

    fn recent_list(&mut self, _project_id: &str) -> BackendResult<Vec<String>> {
        let raw = unsafe { ffi::sap_recent_list(self.main_window) };
        if raw.is_null() {
            return Err(BackendError::InvalidParams(
                "recent.list failed (invalid handle)".into(),
            ));
        }
        let json_str = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sap_free_string(raw) };
        serde_json::from_str::<Vec<String>>(&json_str)
            .map_err(|e| BackendError::InvalidParams(format!("bad recent-list JSON: {e}")))
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
    // Mirrors main.rs's (the standalone/snapshotd-driven path)
    // SNAPSHOT_AUDIO_ENABLED gate exactly -- this was previously
    // hardcoded false here, silently disabling the whole audio.*
    // namespace (method_not_found) whenever the server ran embedded
    // inside a real Shotcut process instead of via snapshotd, with no
    // way to opt in. Bug, not a deliberate FFI-path restriction: audio.*
    // is pure filter.add/filter.setProperty plumbing (see server.rs),
    // which is fully wired in FfiBackend.
    let audio_enabled = matches!(
        std::env::var("SNAPSHOT_AUDIO_ENABLED").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("True")
    );
    let config = ServerConfig::new(socket_path.clone(), token, audio_enabled);

    // Wire the external-notification bridge (real Qt-side edits, via
    // sap_ffi.cpp's sap_emit_event) before the runtime starts, so a
    // signal firing immediately after `sap_install_notification_bridge`'s
    // connect() call (main.cpp connects it before spawning this very
    // thread) has somewhere to land rather than racing the mutex init.
    let (notify_tx, notify_rx) = mpsc::unbounded_channel::<RpcNotification>();
    *NOTIFY_BRIDGE_TX
        .lock()
        .expect("notify bridge mutex poisoned") = Some(notify_tx.clone());
    *EXPORT_NOTIFY_TX
        .lock()
        .expect("export notify bridge mutex poisoned") = Some(notify_tx);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sap-rust: failed to start tokio runtime: {e}");
            return;
        }
    };
    eprintln!("sap-rust: SAP server starting on {socket_path}");
    if let Err(e) = rt.block_on(server::serve(config, backend, Some(notify_rx))) {
        eprintln!("sap-rust: server exited with error: {e}");
    }
}

/// Called from C++ (`sap_ffi.cpp`'s `sap_emit_event`, itself connected to
/// the real `MultitrackModel::modified` signal by
/// `sap_install_notification_bridge`) whenever the live Shotcut document
/// changes -- whether that change came from an RPC-driven `Backend` call
/// (`edit.addTrack`, `edit.insertClip`, ...) or a direct human GUI edit in
/// the same process. `json_payload` is a small JSON object with at least a
/// `"type"` field naming the notification method to publish (currently
/// always `"edit.changed"`, matching `sap_ffi.cpp`'s call site).
///
/// Fans out to every SAP client currently bound to any project in this
/// process via `server::serve`'s `external_notify_rx` plumbing -- see that
/// function's doc comment in `server.rs` for why "every project", not one.
/// A malformed payload, a null pointer, or a call before `sap_start_server`
/// has reached the point of populating `NOTIFY_BRIDGE_TX` are all silent
/// no-ops (matches the pre-existing stderr-only stub's own best-effort
/// semantics -- this bridge must never be allowed to panic or block the Qt
/// main thread it runs on).
///
/// # Safety
/// `json_payload`, if non-null, must be a valid NUL-terminated C string for
/// the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn sap_ffi_notify_bridge(json_payload: *const c_char) {
    if json_payload.is_null() {
        return;
    }
    // An RPC-driven call already in flight on the dispatcher will publish
    // its own precisely-reasoned notification (see
    // `SUPPRESS_QT_BRIDGE_NOTIFICATION`'s doc comment) -- skip the
    // generic duplicate here rather than racing it.
    if SUPPRESS_QT_BRIDGE_NOTIFICATION.load(Ordering::SeqCst) {
        return;
    }
    let payload = CStr::from_ptr(json_payload).to_string_lossy().into_owned();
    #[derive(serde::Deserialize)]
    struct EmitPayload {
        #[serde(rename = "type")]
        type_: String,
    }
    let Ok(parsed) = serde_json::from_str::<EmitPayload>(&payload) else {
        return;
    };
    let notification = RpcNotification::new(parsed.type_, json!({"reason": "qtGuiEdit"}));
    if let Ok(guard) = NOTIFY_BRIDGE_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            // Non-blocking by construction (unbounded channel); a send
            // error just means the server task already shut down, a
            // normal shutdown-race outcome, not something to surface.
            let _ = tx.send(notification);
        }
    }
}

/// Standard base64 (RFC 4648, with `=` padding), local to this file so
/// `playback_get_frame`'s wire format needs no external crate dependency.
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
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}
