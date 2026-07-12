//! Newline-delimited JSON-RPC framing over a backend process's stdio, per
//! ACP's stdio transport convention (one JSON value per line).

use acpx_proto::jsonrpc::{Request, Response};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("backend closed stdout (eof)")]
    Eof,
}

/// Reads one newline-delimited JSON-RPC message at a time from a backend
/// process's stdout. Returns raw `Value`s -- callers decide whether a given
/// line is a `Response` or an agent-initiated notification/request (e.g.
/// `session/update`), since both flow over the same stream.
pub struct FramedReader {
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl FramedReader {
    pub fn new(stdout: ChildStdout) -> Self {
        Self {
            lines: BufReader::new(stdout).lines(),
        }
    }

    pub async fn read_value(&mut self) -> Result<Value, FramingError> {
        loop {
            let line = self.lines.next_line().await?.ok_or(FramingError::Eof)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // backends may emit blank keep-alive lines
            }
            return Ok(serde_json::from_str(trimmed)?);
        }
    }

    pub async fn read_response(&mut self) -> Result<Response, FramingError> {
        let value = self.read_value().await?;
        Ok(serde_json::from_value(value)?)
    }
}

pub struct FramedWriter {
    stdin: ChildStdin,
}

impl FramedWriter {
    pub fn new(stdin: ChildStdin) -> Self {
        Self { stdin }
    }

    pub async fn write_value(&mut self, value: &Value) -> Result<(), FramingError> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn write_request(&mut self, request: &Request) -> Result<(), FramingError> {
        let value = serde_json::to_value(request)?;
        self.write_value(&value).await
    }
}
