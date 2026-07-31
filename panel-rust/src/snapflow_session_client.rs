//! TCP WebSocket client for snapflowd's authenticated session-derived-state
//! API. This is intentionally separate from `snapshotd_client` (the local
//! lifecycle socket) and `acpx-client` (the ACPX gateway protocol).

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Condvar, Mutex},
};

const SESSION_IDLE_POLL: Duration = Duration::from_millis(250);
const SESSION_UPDATE_POLL: Duration = Duration::from_millis(500);
const SESSION_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_MAX_BACKOFF: Duration = Duration::from_secs(5);
const SESSION_QUEUE_LIMIT: usize = 256;
use thiserror::Error;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    MaybeTlsStream, WebSocketStream,
};

#[derive(Debug, Error)]
pub enum SnapflowSessionClientError {
    #[error("snapflowd session websocket connection failed: {0}")]
    Connect(String),
    #[error("snapflowd session websocket protocol error: {0}")]
    Protocol(String),
    #[error("snapflowd session websocket closed")]
    Closed,
    #[error("snapflowd session operation timed out")]
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSnapshot {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "acpSessionId")]
    pub acp_session_id: Option<String>,
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    #[serde(rename = "projectPath")]
    pub project_path: Option<String>,
    #[serde(rename = "connectionStatus")]
    pub connection_status: String,
    pub revision: u64,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub snapflow_session_id: Option<String>,
    pub acp_session_id: Option<String>,
}

impl SessionSnapshot {
    pub fn session_ref(&self) -> SessionRef {
        SessionRef {
            snapflow_session_id: Some(self.session_id.clone()),
            acp_session_id: self.acp_session_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpdate {
    pub client_instance_id: Option<String>,
    pub snapshot: SessionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEndpoint {
    pub websocket_url: String,
    pub service_token: String,
}

impl SessionEndpoint {
    /// Resolve the endpoint without touching the UI thread's network path.
    /// Environment variables are useful for remote/TLS deployments; the
    /// local daemon's owner-only mcp_config.json is the default discovery
    /// source and works on Windows as well as Unix.
    pub fn discover() -> Option<Self> {
        let token = std::env::var("SNAPFLOWD_SESSION_SERVICE_TOKEN").ok();
        let explicit_url = std::env::var("SNAPFLOWD_SESSION_WS_URL").ok();
        if let (Some(websocket_url), Some(service_token)) = (explicit_url, token.clone()) {
            if !websocket_url.is_empty() && !service_token.is_empty() {
                return Some(Self {
                    websocket_url,
                    service_token,
                });
            }
        }

        let home = std::env::var_os("SNAPSHOTD_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|value| PathBuf::from(value).join(".snapshotd"))
            })
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|value| PathBuf::from(value).join(".snapshotd"))
            })?;
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join("mcp_config.json")).ok()?).ok()?;
        let service_token = token.or_else(|| {
            config
                .get("sessionServiceToken")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })?;
        let addr = crate::agent_bridge::snapshotd_mcp_addr().or_else(|| {
            config
                .get("bindAddr")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })?;
        let addr = if addr.starts_with("0.0.0.0:") || addr.starts_with("[::]:") {
            addr.replacen("0.0.0.0", "127.0.0.1", 1)
                .replacen("[::]", "127.0.0.1", 1)
        } else {
            addr
        };
        (!service_token.is_empty() && !addr.is_empty()).then(|| Self {
            websocket_url: format!("ws://{addr}/session/ws"),
            service_token,
        })
    }
}

#[derive(Debug, Serialize)]
struct WsRequest<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(rename = "requestId")]
    request_id: &'a str,
    #[serde(rename = "sessionIds", skip_serializing_if = "Option::is_none")]
    session_ids: Option<&'a [String]>,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(rename = "acpSessionId", skip_serializing_if = "Option::is_none")]
    acp_session_id: Option<&'a str>,
    #[serde(rename = "contextToken", skip_serializing_if = "Option::is_none")]
    context_token: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "clientInstanceId")]
    client_instance_id: Option<String>,
    snapshot: Option<SessionSnapshot>,
    snapshots: Option<Vec<SessionSnapshot>>,
    error: Option<String>,
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct SnapflowSessionClient {
    socket: Socket,
    next_request_id: u64,
    pub client_instance_id: Option<String>,
    pending_updates: VecDeque<SessionUpdate>,
}

/// Owns one reconnecting subscription worker. The UI only changes the
/// desired session-id set and drains a bounded queue; all TCP/WebSocket I/O
/// remains off the Slint thread.
pub struct SessionSubscription {
    desired: Arc<Mutex<Vec<String>>>,
    desired_generation: Arc<AtomicU64>,
    updates: Arc<Mutex<VecDeque<SessionUpdate>>>,
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SessionSubscription {
    pub fn start(endpoint: SessionEndpoint) -> Self {
        let desired = Arc::new(Mutex::new(Vec::new()));
        let desired_generation = Arc::new(AtomicU64::new(0));
        let updates = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let worker_desired = Arc::clone(&desired);
        let worker_generation = Arc::clone(&desired_generation);
        let worker_updates = Arc::clone(&updates);
        let worker_stop = Arc::clone(&stop);
        let worker_wake = Arc::clone(&wake);
        let worker = std::thread::Builder::new()
            .name("snapflow-session-status".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!("panel-rust: session status runtime failed: {error}");
                        return;
                    }
                };
                let _guard = runtime.enter();
                let mut backoff = Duration::from_millis(250);
                while !worker_stop.load(Ordering::Acquire) {
                    let ids = worker_desired
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if ids.is_empty() {
                        if Self::wait_or_stop(&worker_stop, &worker_wake, SESSION_IDLE_POLL) {
                            break;
                        }
                        continue;
                    }
                    let generation = worker_generation.load(Ordering::Acquire);
                    // Construct Tokio timer futures inside the runtime. The
                    // timer registration itself needs an active reactor;
                    // constructing `timeout(...)` outside `block_on` makes
                    // the worker panic before it can subscribe.
                    let connected = runtime.block_on(async {
                        SnapflowSessionClient::connect(
                            &endpoint.websocket_url,
                            &endpoint.service_token,
                        )
                        .await
                    });
                    let Ok(mut client) = connected else {
                        if Self::wait_or_stop(&worker_stop, &worker_wake, backoff) {
                            break;
                        }
                        backoff = (backoff * 2).min(SESSION_MAX_BACKOFF);
                        continue;
                    };
                    backoff = Duration::from_millis(250);
                    if let Ok(Ok(initial)) = runtime.block_on(tokio::time::timeout(
                        Duration::from_secs(3),
                        client.subscribe(
                            ids.into_iter()
                                .map(|acp_session_id| SessionRef {
                                    snapflow_session_id: None,
                                    acp_session_id: Some(acp_session_id),
                                })
                                .collect(),
                        ),
                    )) {
                        let mut queue = worker_updates.lock().unwrap_or_else(|e| e.into_inner());
                        for snapshot in initial {
                            queue.push_back(SessionUpdate {
                                client_instance_id: client.client_instance_id.clone(),
                                snapshot,
                            });
                        }
                        while queue.len() > SESSION_QUEUE_LIMIT {
                            queue.pop_front();
                        }
                    } else {
                        if Self::wait_or_stop(&worker_stop, &worker_wake, backoff) {
                            break;
                        }
                        backoff = (backoff * 2).min(SESSION_MAX_BACKOFF);
                        continue;
                    };
                    loop {
                        if worker_stop.load(Ordering::Acquire)
                            || worker_generation.load(Ordering::Acquire) != generation
                        {
                            break;
                        }
                        match runtime.block_on(client.next_update(SESSION_UPDATE_POLL)) {
                            Ok(update) => {
                                let mut queue =
                                    worker_updates.lock().unwrap_or_else(|e| e.into_inner());
                                queue.push_back(update);
                                while queue.len() > SESSION_QUEUE_LIMIT {
                                    queue.pop_front();
                                }
                            }
                            Err(SnapflowSessionClientError::Timeout) => {}
                            Err(_) => break,
                        }
                    }
                }
            })
            .ok();
        Self {
            desired,
            desired_generation,
            updates,
            stop,
            wake,
            worker,
        }
    }

    fn wait_or_stop(stop: &AtomicBool, wake: &(Mutex<()>, Condvar), duration: Duration) -> bool {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let guard = wake.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = wake.1.wait_timeout(guard, duration);
        stop.load(Ordering::Acquire)
    }

    pub fn set_sessions(&self, mut session_ids: Vec<String>) {
        session_ids.retain(|id| !id.is_empty());
        session_ids.sort();
        session_ids.dedup();
        let mut desired = self.desired.lock().unwrap_or_else(|e| e.into_inner());
        if *desired != session_ids {
            *desired = session_ids;
            self.desired_generation.fetch_add(1, Ordering::AcqRel);
            self.wake.1.notify_all();
        }
    }

    pub fn drain(&self) -> Vec<SessionUpdate> {
        self.updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Return the bounded queue handle for non-UI producers such as SQLite
    /// cache hydration. The caller must preserve the same 256-item bound.
    pub fn updates_handle(&self) -> Arc<Mutex<VecDeque<SessionUpdate>>> {
        Arc::clone(&self.updates)
    }
}

impl Drop for SessionSubscription {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl SnapflowSessionClient {
    pub async fn connect(
        url: &str,
        service_token: &str,
    ) -> Result<Self, SnapflowSessionClientError> {
        let mut request = url
            .into_client_request()
            .map_err(|error| SnapflowSessionClientError::Connect(error.to_string()))?;
        let authorization = HeaderValue::from_str(&format!("Bearer {service_token}"))
            .map_err(|error| SnapflowSessionClientError::Connect(error.to_string()))?;
        request.headers_mut().insert("Authorization", authorization);
        let connected = tokio::time::timeout(SESSION_CONNECT_TIMEOUT, connect_async(request))
            .await
            .map_err(|_| SnapflowSessionClientError::Timeout)?
            .map_err(|error| SnapflowSessionClientError::Connect(error.to_string()))?;
        let (socket, _) = connected;
        Ok(Self {
            socket,
            next_request_id: 1,
            client_instance_id: None,
            pending_updates: VecDeque::new(),
        })
    }

    pub async fn subscribe(
        &mut self,
        session_refs: Vec<SessionRef>,
    ) -> Result<Vec<SessionSnapshot>, SnapflowSessionClientError> {
        let request_id = self.next_id();
        let session_ids: Vec<String> = session_refs
            .into_iter()
            .filter_map(|reference| reference.snapflow_session_id.or(reference.acp_session_id))
            .collect();
        self.send(WsRequest {
            kind: "session.subscribe",
            request_id: &request_id,
            session_ids: Some(&session_ids),
            session_id: None,
            acp_session_id: None,
            context_token: None,
        })
        .await?;
        loop {
            let response = self.receive_timed().await?;
            if response.request_id.as_deref() != Some(request_id.as_str()) {
                if let Some(snapshot) = response.snapshot {
                    self.pending_updates.push_back(SessionUpdate {
                        client_instance_id: self.client_instance_id.clone(),
                        snapshot,
                    });
                }
                continue;
            }
            if let Some(error) = response.error {
                return Err(SnapflowSessionClientError::Protocol(error));
            }
            self.client_instance_id = response.client_instance_id;
            return Ok(response.snapshots.unwrap_or_default());
        }
    }

    pub async fn resync(
        &mut self,
        session_id: &str,
        acp_session_id: Option<&str>,
    ) -> Result<SessionSnapshot, SnapflowSessionClientError> {
        let request_id = self.next_id();
        self.send(WsRequest {
            kind: "session.resync",
            request_id: &request_id,
            session_ids: None,
            session_id: Some(session_id),
            acp_session_id,
            context_token: None,
        })
        .await?;
        loop {
            let response = self.receive_timed().await?;
            if response.request_id.as_deref() != Some(request_id.as_str()) {
                if let Some(snapshot) = response.snapshot {
                    self.pending_updates.push_back(SessionUpdate {
                        client_instance_id: self.client_instance_id.clone(),
                        snapshot,
                    });
                }
                continue;
            }
            if let Some(error) = response.error {
                return Err(SnapflowSessionClientError::Protocol(error));
            }
            return response.snapshot.ok_or_else(|| {
                SnapflowSessionClientError::Protocol("missing session snapshot".into())
            });
        }
    }

    pub async fn register_context(
        &mut self,
        context_token: &str,
        acp_session_id: &str,
    ) -> Result<(), SnapflowSessionClientError> {
        let request_id = self.next_id();
        self.send(WsRequest {
            kind: "session.context.register",
            request_id: &request_id,
            session_ids: None,
            session_id: None,
            acp_session_id: Some(acp_session_id),
            context_token: Some(context_token),
        })
        .await?;
        loop {
            let response = self.receive_timed().await?;
            if response.request_id.as_deref() != Some(request_id.as_str()) {
                if let Some(snapshot) = response.snapshot {
                    self.pending_updates.push_back(SessionUpdate {
                        client_instance_id: self.client_instance_id.clone(),
                        snapshot,
                    });
                }
                continue;
            }
            if let Some(error) = response.error {
                return Err(SnapflowSessionClientError::Protocol(error));
            }
            return Ok(());
        }
    }

    pub async fn next_update(
        &mut self,
        timeout: Duration,
    ) -> Result<SessionUpdate, SnapflowSessionClientError> {
        if let Some(update) = self.pending_updates.pop_front() {
            return Ok(update);
        }
        let response = tokio::time::timeout(timeout, self.receive())
            .await
            .map_err(|_| SnapflowSessionClientError::Timeout)??;
        if let Some(error) = response.error {
            return Err(SnapflowSessionClientError::Protocol(error));
        }
        let snapshot = response.snapshot.ok_or_else(|| {
            SnapflowSessionClientError::Protocol(format!(
                "{} did not contain a snapshot",
                response.kind
            ))
        })?;
        Ok(SessionUpdate {
            client_instance_id: self.client_instance_id.clone(),
            snapshot,
        })
    }

    pub async fn disconnect(mut self) -> Result<(), SnapflowSessionClientError> {
        self.socket
            .close(None)
            .await
            .map_err(|error| SnapflowSessionClientError::Protocol(error.to_string()))
    }

    fn next_id(&mut self) -> String {
        let id = self.next_request_id.to_string();
        self.next_request_id += 1;
        id
    }

    async fn send(&mut self, request: WsRequest<'_>) -> Result<(), SnapflowSessionClientError> {
        let payload = serde_json::to_string(&request)
            .map_err(|error| SnapflowSessionClientError::Protocol(error.to_string()))?;
        self.socket
            .send(Message::Text(payload))
            .await
            .map_err(|error| SnapflowSessionClientError::Protocol(error.to_string()))
    }

    async fn receive(&mut self) -> Result<Response, SnapflowSessionClientError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text)
                        .map_err(|error| SnapflowSessionClientError::Protocol(error.to_string()))
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) | None => return Err(SnapflowSessionClientError::Closed),
                Some(Ok(_)) => {
                    return Err(SnapflowSessionClientError::Protocol(
                        "expected text websocket message".into(),
                    ))
                }
                Some(Err(error)) => return Err(SnapflowSessionClientError::Protocol(error.to_string())),
            }
        }
    }

    async fn receive_timed(&mut self) -> Result<Response, SnapflowSessionClientError> {
        tokio::time::timeout(Duration::from_secs(3), self.receive())
            .await
            .map_err(|_| SnapflowSessionClientError::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn client_subscribes_and_receives_replacement_snapshot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = socket.next().await.unwrap().unwrap();
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "session.subscribed",
                        "requestId": "1",
                        "clientInstanceId": "panel-instance",
                        "snapshots": [{
                            "sessionId": "snap-1",
                            "acpSessionId": "acp-1",
                            "connectionStatus": "connected",
                            "revision": 1,
                            "createdAt": "",
                            "expiresAt": ""
                        }]
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "session.update",
                        "sessionId": "snap-1",
                        "snapshot": {
                            "sessionId": "snap-1",
                            "acpSessionId": "acp-1",
                            "projectPath": "/projects/demo.mlt",
                            "connectionStatus": "connected",
                            "revision": 2,
                            "createdAt": "",
                            "expiresAt": ""
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let mut client =
            SnapflowSessionClient::connect(&format!("ws://{address}/session/ws"), "secret")
                .await
                .unwrap();
        let initial = client
            .subscribe(vec![SessionRef {
                snapflow_session_id: None,
                acp_session_id: Some("acp-1".to_owned()),
            }])
            .await
            .unwrap();
        assert_eq!(client.client_instance_id.as_deref(), Some("panel-instance"));
        assert_eq!(initial[0].session_id, "snap-1");
        let update = client.next_update(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            update.snapshot.project_path.as_deref(),
            Some("/projects/demo.mlt")
        );
        assert_eq!(update.snapshot.revision, 2);
        server.await.unwrap();
    }
}
