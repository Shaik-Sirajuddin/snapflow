//! Spawn/stop one backend ACP agent process and frame newline-delimited
//! JSON-RPC over its stdio.

use crate::framing::{FramedReader, FramedWriter};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("failed to spawn backend process: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("backend process has no stdin/stdout pipes")]
    MissingPipes,
}

/// What to run for one backend agent, and how -- resolved from either a
/// hardcoded Phase 1 spec or (from Phase 4 on) a registry entry's
/// `distribution` method. Native/unmanaged mode (Phase 1/2 default per
/// `02-architecture.md`) means `env` stays empty: the process inherits the
/// ambient environment as-is, no acpx-injected provider/key config.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Optional process working directory for custom backend definitions.
    pub cwd: Option<std::path::PathBuf>,
    /// Env vars to set/override on top of the inherited ambient
    /// environment. Empty in native/unmanaged mode.
    pub env: HashMap<String, String>,
}

impl SpawnSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            env: HashMap::new(),
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

/// A supervised backend agent process with framed stdio JSON-RPC access.
pub struct BackendProcess {
    child: Child,
    /// Shared, independently-lockable -- **not** covered by whatever lock
    /// a caller holds on the surrounding `Arc<Mutex<BackendProcess>>`
    /// itself (`acpx_conductor::supervisor::SharedBackendProcess`). This
    /// is deliberate, not an accident of convenience: `acpx-core::router`
    /// needs to be able to write a real ACP `session/cancel` *notification*
    /// onto this same process's stdin *while* a `session/prompt` call
    /// against this exact process is still mid-flight, holding the outer
    /// per-process lock for the whole duration of its blocking
    /// `read_matching_response` loop (see that function's own doc
    /// comment for why one child process's stdio can never support two
    /// truly interleaved request/response pairs -- that constraint is
    /// real and unavoidable, but a fire-and-forget *write* with no
    /// matching read isn't a request/response pair at all, so it isn't
    /// bound by it). `Supervisor` keeps its own independent clone of this
    /// exact `Arc` (via `Self::writer_handle`, captured at spawn time,
    /// *before* the fresh `BackendProcess` is ever wrapped in its own
    /// outer `Arc<Mutex<..>>` and handed out), so a caller can obtain it
    /// without ever touching the outer per-process lock at all -- see
    /// `acpx_conductor::supervisor::Supervisor::cancel_writer`.
    pub writer: Arc<Mutex<FramedWriter>>,
    /// `None` once [`Self::start_demux`] has taken it to hand to the
    /// process's background reader task -- see that method's doc comment.
    /// `Some` for the entire handshake/authenticate phase (those still
    /// read directly off it, unchanged) and for the whole lifetime of a
    /// process that never enables process-reader-demux at all (the
    /// opt-in default: `ACPX_PROCESS_READER_DEMUX` unset).
    reader: Option<FramedReader>,
    /// `Some` once [`Self::start_demux`] has spawned this process's
    /// background reader task -- the pending-request table callers
    /// register a response id against instead of holding this
    /// `BackendProcess`'s own outer lock across a full write+read turn.
    /// `None` for the entire lifetime of a process that never enables
    /// process-reader-demux (`ACPX_PROCESS_READER_DEMUX` unset, the
    /// default) -- every dispatch path keeps reading directly off
    /// `reader` via [`Self::reader_mut`] exactly as before this field
    /// existed. See `memory/acpx/tasks/zed_integration.yaml` task 7 and
    /// `memory/acpx/gen/acpx-concurrency-config-execution.meta.json`
    /// phase 1.
    pub pending: Option<Arc<crate::demux::PendingRequests>>,
    /// Whether the ACP `initialize` handshake has already been performed
    /// against this process instance. Deliberately just a generic done/not
    /// flag owned here (not ACP-specific logic -- this crate stays
    /// protocol-agnostic per `03-crate-and-folder-layout.md`'s crate
    /// split, `acpx-core::router` owns what "initialize" actually means)
    /// so callers holding this process's own lock can check-and-set it
    /// atomically without a second, separate piece of bookkeeping keyed
    /// off process identity: this flag's lifetime is exactly this
    /// `BackendProcess` instance's lifetime, so a crash + respawn (a
    /// brand new instance) naturally starts back at `false`.
    pub handshake_done: bool,
    /// The real ACP `initialize` response's `result` object, captured the
    /// first time [`Self::handshake_done`] flips to `true` -- i.e. the
    /// backend's actual `agentCapabilities`/`authMethods`/negotiated
    /// `protocolVersion`, not acpx's assumptions about them. `None` until
    /// the handshake has actually run once. Reset to `None` on every
    /// fresh spawn alongside `handshake_done`, for the same crash+respawn
    /// reason. Protocol-agnostic storage only (an opaque JSON blob) --
    /// same rationale as `handshake_done` itself: this crate doesn't know
    /// or care what "agentCapabilities" means, `acpx-core::router` does.
    pub agent_capabilities: Option<serde_json::Value>,
    /// Live `terminal/create`d commands for this process, keyed by the
    /// terminal id acpx-core mints and hands back to the backend. Lives
    /// here (not in acpx-core) for the same reason `handshake_done`/
    /// `agent_capabilities` do: it's a piece of per-process state a
    /// caller holding this process's own lock needs to check-and-mutate
    /// atomically. Never reset on respawn (unlike `handshake_done`) --
    /// a crash+respawn is a brand new `BackendProcess` instance with a
    /// fresh, empty map, and any terminal ids the backend held from the
    /// old instance are simply gone, matching a real terminal's lifetime
    /// being tied to the process that created it.
    pub terminals: HashMap<String, crate::terminal::TerminalHandle>,
    /// Whether a real ACP `authenticate` request has already succeeded
    /// against this process instance. Only meaningful when the backend's
    /// `initialize` response (`agent_capabilities`) advertised a
    /// non-empty `authMethods` -- `acpx-core::router::ensure_backend_
    /// initialized` is the sole reader/writer of this flag, deciding
    /// from it whether `authenticate` still needs to be attempted (or
    /// re-attempted, if it previously failed) before any session/*
    /// call reaches this backend. `false` until a real `authenticate`
    /// round trip returns a non-error result. Reset to `false` on every
    /// fresh spawn alongside `handshake_done`/`agent_capabilities`, same
    /// crash+respawn reasoning as those two fields.
    pub authenticated: bool,
}

impl BackendProcess {
    /// Spawn a backend process per `spec`, wiring newline-delimited
    /// JSON-RPC framing over its stdio. stderr is inherited (not captured)
    /// so backend diagnostics surface directly in acpx's own logs for now;
    /// revisit if that gets noisy.
    pub async fn spawn(spec: &SpawnSpec) -> Result<Self, ProcessError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or(ProcessError::MissingPipes)?;
        let stdout = child.stdout.take().ok_or(ProcessError::MissingPipes)?;

        Ok(Self {
            child,
            reader: Some(FramedReader::new(stdout)),
            writer: Arc::new(Mutex::new(FramedWriter::new(stdin))),
            pending: None,
            handshake_done: false,
            agent_capabilities: None,
            terminals: HashMap::new(),
            authenticated: false,
        })
    }

    /// Clone of this process's shared writer handle -- see
    /// [`Self::writer`]'s doc comment for why this exists and who's
    /// meant to call it (`Supervisor`, once, at spawn time).
    pub fn writer_handle(&self) -> Arc<Mutex<FramedWriter>> {
        Arc::clone(&self.writer)
    }

    /// Mutable access to the raw reader for direct, single-shot reads
    /// (the `initialize`/`authenticate` handshake, and the idle
    /// scavenger's non-blocking drain) that happen before -- or entirely
    /// without -- process-reader-demux ever starting for this process.
    /// Panics if called after [`Self::start_demux`] has taken the reader;
    /// every call site that could run after demux starts must go through
    /// the pending-request table (`self.pending`) instead.
    pub fn reader_mut(&mut self) -> &mut FramedReader {
        self.reader.as_mut().expect(
            "BackendProcess::reader_mut called after start_demux() took the reader; \
             use self.pending's registered oneshot instead",
        )
    }

    /// Move the raw reader out to a new background task that owns it for
    /// the rest of this process's lifetime, matching backend responses to
    /// callers via a pending-request table (`self.pending`) instead of
    /// requiring the caller to hold this `BackendProcess`'s own outer
    /// lock across an entire write+read turn -- see
    /// `memory/acpx/tasks/zed_integration.yaml` task 7. Idempotent is
    /// *not* guaranteed: call only when `self.pending.is_none()` (callers
    /// check that themselves so they can skip re-spawning a consumer for
    /// an already-demuxed process, mirroring the existing idle-scavenger
    /// dedup pattern in `acpx-core::router::spawn_idle_scavenger_if_new`).
    /// Must only be called once the `initialize`/`authenticate` handshake
    /// has already completed via [`Self::reader_mut`] -- the handshake's
    /// own fixed-id reads are not routed through the pending table.
    pub fn start_demux(&mut self) -> mpsc::UnboundedReceiver<crate::demux::UnmatchedFrame> {
        let reader = self
            .reader
            .take()
            .expect("start_demux called twice, or before the reader was ever set");
        let pending = Arc::new(crate::demux::PendingRequests::new());
        let (unmatched_tx, unmatched_rx) = mpsc::unbounded_channel();
        crate::demux::spawn_reader_task(reader, Arc::clone(&pending), unmatched_tx);
        self.pending = Some(pending);
        unmatched_rx
    }

    /// Returns the process's exit status if it has already exited
    /// (non-blocking check). Tokio caches the reaped status internally, so
    /// repeated calls after the process has exited keep returning the same
    /// value rather than erroring on a second wait.
    pub fn try_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().unwrap_or_default()
    }

    /// True if the process has exited (non-blocking check).
    pub fn has_exited(&mut self) -> bool {
        self.try_exit_status().is_some()
    }

    /// Operating-system process id while the child is still running.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn kill(&mut self) -> Result<(), std::io::Error> {
        self.child.start_kill()?;
        let _ = self.child.wait().await;
        Ok(())
    }
}
