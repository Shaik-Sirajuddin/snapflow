//! Spawn/stop one backend ACP agent process and frame newline-delimited
//! JSON-RPC over its stdio.

use crate::framing::{FramedReader, FramedWriter};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::{Child, Command};

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
    /// Env vars to set/override on top of the inherited ambient
    /// environment. Empty in native/unmanaged mode.
    pub env: HashMap<String, String>,
}

impl SpawnSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: HashMap::new(),
        }
    }
}

/// A supervised backend agent process with framed stdio JSON-RPC access.
pub struct BackendProcess {
    child: Child,
    pub reader: FramedReader,
    pub writer: FramedWriter,
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

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or(ProcessError::MissingPipes)?;
        let stdout = child.stdout.take().ok_or(ProcessError::MissingPipes)?;

        Ok(Self {
            child,
            reader: FramedReader::new(stdout),
            writer: FramedWriter::new(stdin),
            handshake_done: false,
            agent_capabilities: None,
            terminals: HashMap::new(),
            authenticated: false,
        })
    }

    /// Returns the process's exit status if it has already exited
    /// (non-blocking check). Tokio caches the reaped status internally, so
    /// repeated calls after the process has exited keep returning the same
    /// value rather than erroring on a second wait.
    pub fn try_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        match self.child.try_wait() {
            Ok(status) => status,
            Err(_) => None,
        }
    }

    /// True if the process has exited (non-blocking check).
    pub fn has_exited(&mut self) -> bool {
        self.try_exit_status().is_some()
    }

    pub async fn kill(&mut self) -> Result<(), std::io::Error> {
        self.child.start_kill()?;
        let _ = self.child.wait().await;
        Ok(())
    }
}
