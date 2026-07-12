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
        })
    }

    /// True if the process has exited (non-blocking check).
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub async fn kill(&mut self) -> Result<(), std::io::Error> {
        self.child.start_kill()?;
        let _ = self.child.wait().await;
        Ok(())
    }
}
