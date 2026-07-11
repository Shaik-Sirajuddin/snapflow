//! LSP-style `Content-Length` framing over any `AsyncRead`/`AsyncWrite`, per
//! 01-jsonrpc-spec.md's "Transport" section. Chosen over newline-delimited
//! JSON specifically because SAP payloads can contain arbitrary file paths /
//! text (subtitle content, notes) that could otherwise require fragile
//! escaping if a bare newline were the frame delimiter.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed Content-Length header")]
    BadHeader,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("peer closed connection")]
    Eof,
}

/// Reads one `Content-Length`-framed JSON message from `reader`.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Value, FramingError> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(FramingError::Eof);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // blank line: end of headers
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let n: usize = rest.trim().parse().map_err(|_| FramingError::BadHeader)?;
            content_length = Some(n);
        }
        // Unknown headers are ignored, matching LSP framing tolerance.
    }
    let len = content_length.ok_or(FramingError::BadHeader)?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let value: Value = serde_json::from_slice(&buf)?;
    Ok(value)
}

/// Writes one `Content-Length`-framed JSON message to `writer`.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<(), FramingError> {
    let body = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}
