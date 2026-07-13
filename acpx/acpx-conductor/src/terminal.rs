//! Generic supervised subprocess with captured, byte-limited output --
//! the primitive `acpx-core`'s ACP `terminal/*` method handlers
//! (`acpx-core/src/router.rs`'s `handle_terminal_request`) are built on.
//! Protocol-agnostic deliberately, same crate-boundary reasoning as
//! `BackendProcess`'s `handshake_done`/`agent_capabilities` fields (see
//! their doc comments): this crate owns "spawn a process and capture
//! what it prints, with a byte cap"; `acpx-core::router` owns what an
//! ACP `terminal/create`/`terminal/output`/etc. request means and how to
//! shape one of these into a JSON-RPC reply.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("failed to spawn terminal command: {0}")]
    Spawn(#[from] std::io::Error),
}

/// A command's exit outcome -- exit code, or (Unix only) the signal that
/// killed it. Mirrors the two things a process exit can mean without
/// importing any ACP-specific type name into this crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalExitStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

struct Shared {
    output: Vec<u8>,
    exit_status: Option<TerminalExitStatus>,
}

/// One `terminal/create`d command: the live child process plus its
/// captured combined stdout+stderr (interleaved as it arrives, not
/// separated -- real terminals don't separate them either). Output is
/// captured continuously by two background tasks (one per stream) from
/// the moment of `spawn`, independent of whether/when a caller ever asks
/// for it via [`Self::output`] -- matching the real ACP semantics of
/// `terminal/output` ("returns the current content... without waiting").
pub struct TerminalHandle {
    child: Child,
    shared: Arc<Mutex<Shared>>,
    // Background tasks draining stdout/stderr into `shared`. Joined in
    // `wait_for_exit` -- without this, `wait_for_exit` (which only reaps
    // the child's exit status) can race a caller's very next `output()`
    // call against these tasks still being scheduled to read the child's
    // last buffered bytes, observing a truncated or even completely
    // empty capture despite the process having already exited. Kernel
    // pipe-close on process exit and `Child::wait()`'s own resolution
    // are two independent readiness notifications with no ordering
    // guarantee between them, so this isn't a hypothetical: it
    // reproduced as a real, non-deterministic test failure.
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

impl TerminalHandle {
    /// Spawn `program` with `args`/`env`/`cwd`, capturing combined
    /// stdout+stderr into an in-memory buffer truncated (from the front,
    /// i.e. oldest output dropped first) to stay within
    /// `output_byte_limit` if given. `stdin` is always closed (`/dev/null`
    /// equivalent) -- ACP's terminal model has no provision for an agent
    /// writing to a running command's stdin.
    pub async fn spawn(
        program: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
        output_byte_limit: Option<usize>,
    ) -> Result<Self, TerminalError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("just requested Stdio::piped");
        let stderr = child.stderr.take().expect("just requested Stdio::piped");
        let shared = Arc::new(Mutex::new(Shared {
            output: Vec::new(),
            exit_status: None,
        }));
        let stdout_task = spawn_capture_task(stdout, shared.clone(), output_byte_limit);
        let stderr_task = spawn_capture_task(stderr, shared.clone(), output_byte_limit);
        Ok(Self {
            child,
            shared,
            stdout_task,
            stderr_task,
        })
    }

    /// Current captured output plus the exit status, if the process has
    /// already exited via [`Self::wait_for_exit`] (a bare `terminal/output`
    /// alone, without ever calling `wait_for_exit`, never observes exit --
    /// matching this crate's synchronous-only-when-asked design; the
    /// `acpx-core` caller is responsible for polling `try_wait`-style if
    /// it wants non-blocking exit detection, which this type doesn't
    /// attempt to do on its own in a background task).
    pub async fn output(&self) -> (Vec<u8>, Option<TerminalExitStatus>) {
        let shared = self.shared.lock().await;
        (shared.output.clone(), shared.exit_status.clone())
    }

    /// Block until the command exits, recording its exit status for
    /// every subsequent [`Self::output`] call too.
    pub async fn wait_for_exit(&mut self) -> Result<TerminalExitStatus, TerminalError> {
        let status = self.child.wait().await?;
        // Wait for both capture tasks to observe EOF (i.e. drain
        // whatever the child had already written) before recording exit
        // status, so a caller's subsequent `output()` sees the complete
        // capture rather than whatever happened to be flushed by the
        // time the process itself was reaped -- see the doc comment on
        // `stdout_task`/`stderr_task`.
        let _ = (&mut self.stdout_task).await;
        let _ = (&mut self.stderr_task).await;
        let exit_status = to_exit_status(status);
        self.shared.lock().await.exit_status = Some(exit_status.clone());
        Ok(exit_status)
    }

    /// Kill the command if it hasn't exited yet. Per real ACP semantics,
    /// the terminal itself stays valid afterward (`output`/`wait_for_exit`
    /// still work) -- only `release` invalidates it, which is the
    /// `acpx-core` caller's job (removing this handle from wherever it's
    /// stored), not this type's.
    pub async fn kill(&mut self) -> Result<(), TerminalError> {
        self.child.start_kill()?;
        Ok(())
    }
}

fn to_exit_status(status: std::process::ExitStatus) -> TerminalExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        TerminalExitStatus {
            exit_code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        TerminalExitStatus {
            exit_code: status.code(),
            signal: None,
        }
    }
}

fn spawn_capture_task<R>(
    mut reader: R,
    shared: Arc<Mutex<Shared>>,
    limit: Option<usize>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut shared = shared.lock().await;
                    shared.output.extend_from_slice(&buf[..n]);
                    if let Some(limit) = limit {
                        if shared.output.len() > limit {
                            let excess = shared.output.len() - limit;
                            shared.output.drain(0..excess);
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_output_and_exit_status() {
        let mut handle = TerminalHandle::spawn(
            "sh",
            &["-c".to_string(), "echo hello; exit 3".to_string()],
            &HashMap::new(),
            None,
            None,
        )
        .await
        .expect("spawn");
        let status = handle.wait_for_exit().await.expect("wait_for_exit");
        assert_eq!(status.exit_code, Some(3));
        let (output, exit_status) = handle.output().await;
        assert_eq!(String::from_utf8_lossy(&output).trim_end(), "hello");
        assert_eq!(exit_status, Some(status));
    }

    #[tokio::test]
    async fn output_byte_limit_truncates_from_the_front() {
        let mut handle = TerminalHandle::spawn(
            "sh",
            &["-c".to_string(), "printf '0123456789'".to_string()],
            &HashMap::new(),
            None,
            Some(4),
        )
        .await
        .expect("spawn");
        handle.wait_for_exit().await.expect("wait_for_exit");
        let (output, _) = handle.output().await;
        // Oldest bytes ("012345") dropped, only the last 4 bytes remain.
        assert_eq!(String::from_utf8_lossy(&output), "6789");
    }

    #[tokio::test]
    async fn kill_stops_a_long_running_command() {
        let mut handle = TerminalHandle::spawn(
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            &HashMap::new(),
            None,
            None,
        )
        .await
        .expect("spawn");
        handle.kill().await.expect("kill");
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_for_exit())
                .await
                .expect("kill should make wait_for_exit return quickly, not hang")
                .expect("wait_for_exit");
        assert_ne!(status.exit_code, Some(0));
    }
}
