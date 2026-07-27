//! Async client for snapshotd's newline-delimited Unix JSON-RPC control
//! socket. This is deliberately separate from both ACPX and MCP transports:
//! snapshotd control is a local lifecycle plane, not an agent data plane.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
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

enum RegistrationCommand {
    Update {
        project_path: Option<String>,
        reason: String,
        generation: u64,
    },
    Stop,
}

#[cfg(unix)]
struct DiscoveryEndpoint {
    stop: Sender<()>,
    descriptor: PathBuf,
    socket: PathBuf,
    project_path: Arc<Mutex<Option<String>>>,
}

#[cfg(unix)]
impl DiscoveryEndpoint {
    fn start(home: &Path, nonce: &str, project_path: Option<String>) -> Option<Self> {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;
        let apps = home.join("apps");
        std::fs::create_dir_all(&apps).ok()?;
        let socket = apps.join(format!("{}-{}.sock", std::process::id(), nonce));
        let descriptor = apps.join(format!("{}-{}.json", std::process::id(), nonce));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).ok()?;
        listener.set_nonblocking(true).ok()?;
        let project_path = Arc::new(Mutex::new(project_path));
        let project_for_thread = Arc::clone(&project_path);
        let nonce_for_thread = nonce.to_owned();
        let process_start = process_start_identity();
        let descriptor_value = json!({
            "endpoint": socket,
            "pid": std::process::id(),
            "processStart": process_start,
            "instanceNonce": nonce,
            "protocolVersion": 1,
        });
        std::fs::write(&descriptor, serde_json::to_vec(&descriptor_value).ok()?).ok()?;
        let (stop, stop_rx) = mpsc::channel();
        let descriptor_for_thread = descriptor.clone();
        let socket_for_thread = socket.clone();
        std::thread::Builder::new()
            .name("snapshotd-discovery".into())
            .spawn(move || {
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut line = String::new();
                            if std::io::BufReader::new(&stream).read_line(&mut line).is_ok() {
                                if let Ok(request) = serde_json::from_str::<Value>(&line) {
                                    let challenge = request["params"]["challenge"]
                                        .as_str()
                                        .unwrap_or_default();
                                    let project = project_for_thread
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .clone();
                                    let response = json!({
                                        "jsonrpc": "2.0", "id": request["id"], "result": {
                                            "instanceNonce": nonce_for_thread,
                                            "pid": std::process::id(),
                                            "processStart": process_start,
                                            "projectPath": project,
                                            "challenge": challenge,
                                        }
                                    });
                                    let _ = writeln!(stream, "{response}");
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => break,
                    }
                }
                let _ = std::fs::remove_file(descriptor_for_thread);
                let _ = std::fs::remove_file(socket_for_thread);
            })
            .ok()?;
        Some(Self { stop, descriptor, socket, project_path })
    }

    fn update(&self, project_path: Option<String>) {
        *self
            .project_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = project_path;
    }
}

#[cfg(unix)]
impl Drop for DiscoveryEndpoint {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let _ = std::fs::remove_file(&self.descriptor);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Owns the non-UI lifecycle loop for one external Snapflow process. The
/// registration and heartbeat never run on the Slint/Qt thread; the returned
/// handle only sends small commands to the worker.
pub struct SnapshotdRegistration {
    commands: Sender<RegistrationCommand>,
    instance_id: Arc<Mutex<Option<String>>>,
    #[cfg(unix)]
    discovery: DiscoveryEndpoint,
}

impl SnapshotdRegistration {
    pub fn start(initial_project_path: Option<String>) -> Option<Self> {
        #[cfg(not(unix))]
        {
            let _ = initial_project_path;
            return None;
        }
        #[cfg(unix)]
        {
            let home = std::env::var_os("SNAPSHOTD_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".snapshotd")))?;
            let client = SnapshotdControlClient::new(home.join("control.sock"));
            let nonce = uuid::Uuid::new_v4().to_string();
            let discovery = DiscoveryEndpoint::start(&home, &nonce, initial_project_path.clone())?;
            let (commands, receiver) = mpsc::channel();
            let instance_id = Arc::new(Mutex::new(None));
            let published_id = Arc::clone(&instance_id);
            std::thread::Builder::new()
                .name("snapshotd-registration".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            eprintln!("panel-rust: snapshotd registration runtime failed: {error}");
                            return;
                        }
                    };
                    let registration = ExternalInstanceRegistration {
                        instance_nonce: nonce.clone(),
                        pid: std::process::id(),
                        process_start: process_start_identity(),
                        project_path: initial_project_path.clone(),
                        sap_socket_path: None,
                        capabilities: Some(json!({"lifecycle": true, "mcpContexts": true})),
                    };
                    let mut current_id = runtime.block_on(async {
                        register_and_extract_id(&client, &registration).await
                    });
                    if let Some(id) = current_id.as_ref() {
                        *published_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.clone());
                    }
                    loop {
                        match receiver.recv_timeout(Duration::from_secs(10)) {
                            Ok(RegistrationCommand::Update { project_path, reason, generation }) => {
                                if let Some(id) = current_id.as_deref() {
                                    let _ = runtime.block_on(client.update_open_project(id, project_path.as_deref(), &reason, generation));
                                }
                            }
                            Ok(RegistrationCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                if let Some(id) = current_id.take() {
                                    let _ = runtime.block_on(client.unregister_external_instance(&id));
                                }
                                return;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if let Some(id) = current_id.as_deref() {
                                    if runtime.block_on(client.heartbeat(id)).is_err() {
                                        current_id = runtime.block_on(async { register_and_extract_id(&client, &registration).await });
                                        if let Some(id) = current_id.as_ref() {
                                            *published_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.clone());
                                        }
                                    }
                                } else {
                                    current_id = runtime.block_on(async { register_and_extract_id(&client, &registration).await });
                                    if let Some(id) = current_id.as_ref() {
                                        *published_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.clone());
                                    }
                                }
                            }
                        }
                    }
                })
                .ok()?;
            Some(Self { commands, instance_id, discovery })
        }
    }

    pub fn update(&self, project_path: Option<String>, reason: impl Into<String>, generation: u64) {
        #[cfg(unix)]
        self.discovery.update(project_path.clone());
        let _ = self.commands.send(RegistrationCommand::Update { project_path, reason: reason.into(), generation });
    }

    pub fn instance_id(&self) -> Option<String> {
        self.instance_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

impl Drop for SnapshotdRegistration {
    fn drop(&mut self) {
        let _ = self.commands.send(RegistrationCommand::Stop);
    }
}

async fn register_and_extract_id(
    client: &SnapshotdControlClient,
    registration: &ExternalInstanceRegistration,
) -> Option<String> {
    let value = client.register_external_instance(registration).await.ok()?;
    value.get("instance")?.get("instanceId")?.as_str().map(str::to_owned)
}

fn process_start_identity() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        if let Some(start) = stat.split_whitespace().nth(21) {
            return start.to_owned();
        }
    }
    format!("{}", std::process::id())
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

    pub fn from_default_runtime() -> Option<Self> {
        let home = std::env::var_os("SNAPSHOTD_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".snapshotd")))?;
        Some(Self::new(home.join("control.sock")))
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

    pub async fn unregister_mcp_context(&self, context_token: &str) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.unregisterMcpContext", json!({"contextToken": context_token})).await
    }

    pub async fn list_projects(&self) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.listProjects", json!({})).await
    }

    pub async fn subscribe_projects(&self) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.subscribeProjects", json!({})).await
    }

    /// Resolve the daemon project id from the canonical MLT path. This keeps
    /// the panel's durable identity path-based while still supplying the
    /// daemon's opaque project id for MCP context registration.
    pub async fn project_id_for_path(&self, project_path: &Path) -> Result<Option<String>, SnapshotdClientError> {
        let projects = self.list_projects().await?;
        let project_path = project_path.to_path_buf();
        let wanted = tokio::task::spawn_blocking(move || {
            std::fs::canonicalize(&project_path)
                .unwrap_or(project_path)
                .to_string_lossy()
                .into_owned()
        })
        .await
        .map_err(|error| SnapshotdClientError::Malformed(format!("project path task: {error}")))?;
        let Some(rows) = projects.as_array() else { return Ok(None) };
        let candidates: Vec<(String, PathBuf)> = rows
            .iter()
            .filter_map(|row| {
                let id = row.get("id").or_else(|| row.get("ID")).and_then(Value::as_str)?;
                let root = row.get("rootDir").or_else(|| row.get("RootDir")).and_then(Value::as_str)?;
                let file = row.get("mltFileName").or_else(|| row.get("MltFileName")).and_then(Value::as_str)?;
                Some((id.to_owned(), Path::new(root).join(file)))
            })
            .collect();
        let matched = tokio::task::spawn_blocking(move || {
            candidates.into_iter().find_map(|(id, candidate)| {
                let candidate = std::fs::canonicalize(&candidate).unwrap_or(candidate);
                (candidate.to_string_lossy() == wanted).then_some(id)
            })
        })
        .await
        .map_err(|error| SnapshotdClientError::Malformed(format!("project match task: {error}")))?;
        Ok(matched)
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
            for _ in 0..4 {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match request["method"].as_str() {
                    Some("daemon.registerExternalInstance") => json!({"instance": {"instanceId": "instance-1"}, "heartbeatEvery": "10s", "leaseDuration": "30s"}),
                    Some("daemon.setMcpProjectTarget") => json!({"contextToken": "ctx", "targetProjectId": "project-b"}),
                    Some("daemon.unregisterMcpContext") => json!({}),
                    Some("daemon.subscribeProjects") => json!({"mode": "poll", "pollAfter": "5s", "projects": []}),
                    other => panic!("unexpected method: {other:?}"),
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
        client.unregister_mcp_context("ctx").await.unwrap();
        let subscription = client.subscribe_projects().await.unwrap();
        assert_eq!(subscription["mode"], "poll");
        server.await.unwrap();
    }
}
