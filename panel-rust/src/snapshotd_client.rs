//! Async client for snapshotd's newline-delimited Unix JSON-RPC control
//! socket. This is deliberately separate from both ACPX and MCP transports:
//! snapshotd control is a local lifecycle plane, not an agent data plane.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Error)]
pub enum SnapshotdClientError {
    #[error("snapshotd control socket is unavailable: {0}")]
    Unavailable(String),
    #[error("snapshotd request timed out")]
    Timeout,
    #[error("snapshotd returned an error: {0}")]
    Rpc(String),
    #[error("snapshotd returned malformed data: {0}")]
    Malformed(String),
    #[error("snapshotd client is unsupported on this platform")]
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExternalInstanceRegistration {
    #[serde(rename = "instanceNonce")]
    pub instance_nonce: String,
    pub pid: u32,
    #[serde(rename = "processStart")]
    pub process_start: String,
    #[serde(rename = "projectPath", skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(rename = "sapSocketPath", skip_serializing_if = "Option::is_none")]
    pub sap_socket_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExternalInstanceReply {
    pub instance: Value,
    #[serde(rename = "heartbeatEvery")]
    pub heartbeat_every: Duration,
    #[serde(rename = "leaseDuration")]
    pub lease_duration: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpContextRegistration {
    #[serde(rename = "contextToken")]
    pub context_token: String,
    #[serde(rename = "acpSessionId")]
    pub acp_session_id: String,
    #[serde(rename = "chatProjectId")]
    pub chat_project_id: String,
    #[serde(rename = "defaultTargetProjectId")]
    pub default_target_project_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotdControlClient {
    socket_path: PathBuf,
    next_id: std::sync::Arc<AtomicU64>,
}

impl SnapshotdControlClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            next_id: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, SnapshotdClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.call_once(&request).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let retryable = matches!(
                        error,
                        SnapshotdClientError::Unavailable(_) | SnapshotdClientError::Timeout
                    );
                    last_error = Some(error);
                    if !retryable || attempt + 1 == MAX_ATTEMPTS {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| SnapshotdClientError::Unavailable("no attempt".into())))
    }

    pub async fn register_external_instance(
        &self,
        registration: &ExternalInstanceRegistration,
    ) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.registerExternalInstance", serde_json::to_value(registration).map_err(|e| SnapshotdClientError::Malformed(e.to_string()))?).await
    }

    pub async fn update_open_project(
        &self,
        instance_id: &str,
        project_path: Option<&str>,
        reason: &str,
        generation: u64,
    ) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.updateOpenProject", json!({
            "instanceId": instance_id,
            "projectPath": project_path,
            "reason": reason,
            "generation": generation,
        })).await
    }

    pub async fn heartbeat(&self, instance_id: &str) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.heartbeat", json!({"instanceId": instance_id})).await
    }

    pub async fn unregister_external_instance(&self, instance_id: &str) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.unregisterExternalInstance", json!({"instanceId": instance_id})).await
    }

    pub async fn register_mcp_context(&self, registration: &McpContextRegistration) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.registerMcpContext", serde_json::to_value(registration).map_err(|e| SnapshotdClientError::Malformed(e.to_string()))?).await
    }

    pub async fn set_mcp_project_target(&self, context_token: &str, project_id: &str) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.setMcpProjectTarget", json!({"contextToken": context_token, "projectId": project_id})).await
    }

    #[cfg(unix)]
    async fn call_once(&self, request: &Value) -> Result<Value, SnapshotdClientError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let stream = tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(&self.socket_path))
            .await
            .map_err(|_| SnapshotdClientError::Timeout)?
            .map_err(|e| SnapshotdClientError::Unavailable(e.to_string()))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut line = serde_json::to_vec(request)
            .map_err(|e| SnapshotdClientError::Malformed(e.to_string()))?;
        line.push(b'\n');
        tokio::time::timeout(REQUEST_TIMEOUT, write_half.write_all(&line))
            .await
            .map_err(|_| SnapshotdClientError::Timeout)?
            .map_err(|e| SnapshotdClientError::Unavailable(e.to_string()))?;
        let mut reader = BufReader::new(read_half);
        let mut response = String::new();
        tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut response))
            .await
            .map_err(|_| SnapshotdClientError::Timeout)?
            .map_err(|e| SnapshotdClientError::Unavailable(e.to_string()))?;
        let value: Value = serde_json::from_str(&response)
            .map_err(|e| SnapshotdClientError::Malformed(e.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(SnapshotdClientError::Rpc(error.to_string()));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| SnapshotdClientError::Malformed("response has no result".into()))
    }

    #[cfg(not(unix))]
    async fn call_once(&self, _request: &Value) -> Result<Value, SnapshotdClientError> {
        Err(SnapshotdClientError::Unsupported)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn frames_registration_and_context_calls_over_unix_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = if request["method"] == "daemon.registerExternalInstance" {
                    json!({"instance": {"instanceId": "instance-1"}, "heartbeatEvery": "10s", "leaseDuration": "30s"})
                } else {
                    json!({"contextToken": "ctx", "targetProjectId": "project-b"})
                };
                write.write_all(json!({"jsonrpc":"2.0", "id":request["id"], "result":result}).to_string().as_bytes()).await.unwrap();
                write.write_all(b"\n").await.unwrap();
            }
        });
        let client = SnapshotdControlClient::new(&socket);
        let registration = ExternalInstanceRegistration {
            instance_nonce: "nonce".into(), pid: 1, process_start: "start".into(),
            project_path: None, sap_socket_path: None, capabilities: None,
        };
        let registered = client.register_external_instance(&registration).await.unwrap();
        assert_eq!(registered["instance"]["instanceId"], "instance-1");
        let context = client.set_mcp_project_target("ctx", "project-b").await.unwrap();
        assert_eq!(context["targetProjectId"], "project-b");
        server.await.unwrap();
    }
}
