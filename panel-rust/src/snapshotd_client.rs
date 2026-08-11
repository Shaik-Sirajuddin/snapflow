//! Async client for snapshotd's newline-delimited JSON-RPC control
//! socket. This is deliberately separate from both ACPX and MCP transports:
//! snapshotd control is a local lifecycle plane, not an agent data plane.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ATTEMPTS: usize = 3;

/// Opt-in diagnostics for Windows/project-switch lifecycle investigations.
/// Keep this disabled by default: paths, instance ids, and RPC timings are
/// only emitted when SNAPFLOW_PROJECT_SWITCH_DEBUG is set to 1/true/yes.
fn project_switch_debug_enabled() -> bool {
    std::env::var("SNAPFLOW_PROJECT_SWITCH_DEBUG")
        .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn project_hint(path: Option<&str>) -> String {
    path.map(|value| {
        let name = Path::new(value)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unnamed>");
        format!("{name} ({} chars)", value.chars().count())
    })
    .unwrap_or_else(|| "<none>".to_owned())
}

fn project_switch_debug(message: impl std::fmt::Display) {
    if project_switch_debug_enabled() {
        eprintln!("panel-rust: project-switch-debug: {message}");
    }
}

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
    #[serde(rename = "sapToken", skip_serializing_if = "Option::is_none")]
    pub sap_token: Option<String>,
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

/// A live daemon inventory subscription. The initial snapshot is returned by
/// `subscribe_projects_stream`; subsequent values are authoritative project
/// arrays delivered from `daemon.projectsChanged` notifications. Dropping
/// this value closes the socket-reading task and therefore unregisters the
/// subscription at the transport boundary.
pub struct ProjectUpdateSubscription {
    pub initial: Value,
    pub updates: tokio::sync::mpsc::UnboundedReceiver<Value>,
    _writer: tokio::io::WriteHalf<DaemonStream>,
    reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for ProjectUpdateSubscription {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

#[derive(Clone, Debug, PartialEq)]
struct LifecycleUpdate {
    project_path: Option<String>,
    reason: String,
    generation: u64,
}

enum RegistrationCommand {
    Wake,
    Stop,
}

static LATEST_LIFECYCLE: OnceLock<Mutex<Option<LifecycleUpdate>>> = OnceLock::new();
static LIFECYCLE_EPOCH: AtomicU64 = AtomicU64::new(1);
// The GUI owns one SAP endpoint for its entire process lifetime. Keep the
// registration and discovery endpoint above any panel/plugin instance so a
// panel reload cannot unregister a still-running GUI process. The daemon's
// lease and PID/start-identity checks clean up the row after an unexpected
// process exit; project changes are sent through the shared handle below.
static PROCESS_REGISTRATION: OnceLock<Arc<SnapshotdRegistration>> = OnceLock::new();
static PROCESS_REGISTRATION_INIT: Mutex<()> = Mutex::new(());

fn latest_lifecycle() -> &'static Mutex<Option<LifecycleUpdate>> {
    LATEST_LIFECYCLE.get_or_init(|| Mutex::new(None))
}

fn scoped_generation(epoch: u64, generation: u64) -> u64 {
    epoch
        .saturating_mul(1u64 << 32)
        .saturating_add(generation.min(u32::MAX as u64))
}

fn accept_latest(slot: &mut Option<LifecycleUpdate>, candidate: LifecycleUpdate) -> bool {
    if slot
        .as_ref()
        .is_some_and(|current| candidate.generation <= current.generation)
    {
        return false;
    }
    *slot = Some(candidate);
    true
}

fn seed_lifecycle(epoch: u64, initial_project_path: Option<String>) -> LifecycleUpdate {
    let mut slot = latest_lifecycle()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = initial_project_path
            .clone()
            .map(|project_path| LifecycleUpdate {
                project_path: Some(project_path),
                reason: "opened".to_owned(),
                generation: scoped_generation(epoch, 0),
            });
    }
    slot.clone().unwrap_or(LifecycleUpdate {
        project_path: initial_project_path,
        reason: "opened".to_owned(),
        generation: scoped_generation(epoch, 0),
    })
}

#[cfg(any(unix, windows))]
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
        if let Err(error) = std::fs::create_dir_all(&apps) {
            eprintln!(
                "panel-rust: snapshotd discovery create_dir_all({}) failed: {error}",
                apps.display()
            );
            return None;
        }
        // AF_UNIX socket paths are limited to roughly 108 bytes on Linux.
        // Keep the filesystem names short because SNAPSHOTD_HOME may itself
        // be a long worktree-scoped runtime path.  The nonce is still carried
        // in the descriptor and registration payload, so the PID filename is
        // only a bounded transport name, not the instance identity.
        let socket = apps.join(format!("{}.sock", std::process::id()));
        let descriptor = apps.join(format!("{}.json", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!(
                    "panel-rust: snapshotd discovery bind({}) failed: {error}",
                    socket.display()
                );
                return None;
            }
        };
        // Discovery carries the SAP endpoint/token metadata and is intended
        // for the same-user daemon only. Restrict the filesystem endpoint to
        // the creating user (the daemon runs under that user's account).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        }
        if let Err(error) = listener.set_nonblocking(true) {
            eprintln!("panel-rust: snapshotd discovery nonblocking setup failed: {error}");
            return None;
        }
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
        let descriptor_bytes = match serde_json::to_vec(&descriptor_value) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!(
                    "panel-rust: snapshotd discovery descriptor serialization failed: {error}"
                );
                return None;
            }
        };
        if let Err(error) = std::fs::write(&descriptor, descriptor_bytes) {
            eprintln!(
                "panel-rust: snapshotd discovery write({}) failed: {error}",
                descriptor.display()
            );
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&descriptor, std::fs::Permissions::from_mode(0o600));
        }
        let (stop, stop_rx) = mpsc::channel();
        let descriptor_for_thread = descriptor.clone();
        let socket_for_thread = socket.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("snapshotd-discovery".into())
            .spawn(move || {
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut line = String::new();
                            if std::io::BufReader::new(&stream)
                                .read_line(&mut line)
                                .is_ok()
                            {
                                if let Ok(request) = serde_json::from_str::<Value>(&line) {
                                    let challenge =
                                        request["params"]["challenge"].as_str().unwrap_or_default();
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
                                            "sapSocketPath": sap_endpoint_from_env(),
                                            "sapEndpoint": sap_endpoint_from_env(),
                                            "sapToken": std::env::var("SNAPSHOT_SAP_TOKEN").ok(),
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
        {
            eprintln!("panel-rust: snapshotd discovery listener thread failed: {error}");
            let _ = std::fs::remove_file(&descriptor);
            let _ = std::fs::remove_file(&socket);
            return None;
        }
        Some(Self {
            stop,
            descriptor,
            socket,
            project_path,
        })
    }

    fn update(&self, project_path: Option<String>) {
        *self
            .project_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = project_path;
    }
}

#[cfg(windows)]
impl DiscoveryEndpoint {
    fn start(home: &Path, nonce: &str, project_path: Option<String>) -> Option<Self> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::windows::named_pipe::ServerOptions;

        let apps = home.join("apps");
        std::fs::create_dir_all(&apps).ok()?;
        let socket = std::env::var_os("SNAPSHOTD_DISCOVERY_PIPE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(format!(
                    "\\\\.\\pipe\\snapflow-discovery-{}",
                    std::process::id()
                ))
        });
        let descriptor = apps.join(format!("{}.json", std::process::id()));
        let initial_project_hint = project_hint(project_path.as_deref());
        let project_path = Arc::new(Mutex::new(project_path));
        let project_for_thread = Arc::clone(&project_path);
        let nonce_for_thread = nonce.to_owned();
        let process_start = process_start_identity();
        project_switch_debug(format_args!(
            "windows discovery starting pid={} project={} home_name={}",
            std::process::id(),
            initial_project_hint,
            home.file_name().and_then(|name| name.to_str()).unwrap_or("<unknown>")
        ));
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
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                else {
                    project_switch_debug("windows discovery runtime creation failed");
                    return;
                };
                // Tokio's Windows named-pipe registration consults the
                // reactor while `ServerOptions::create` is called. Merely
                // owning a runtime is insufficient: this worker thread must
                // enter it before creating the pipe, or Tokio panics with
                // "there is no reactor running" and lifecycle discovery dies.
                let _runtime_guard = runtime.enter();
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    let mut server = match ServerOptions::new()
                        // Discovery is local control-plane traffic; reject
                        // remote clients even when the host default DACL is
                        // broader than the current user.
                        .reject_remote_clients(true)
                        .create(&socket_for_thread)
                    {
                        Ok(server) => server,
                        Err(error) => {
                            project_switch_debug(format_args!("windows discovery pipe create failed: {error}"));
                            break;
                        }
                    };
                    project_switch_debug("windows discovery pipe instance created; waiting for client");
                    let connected = runtime.block_on(async {
                        tokio::time::timeout(Duration::from_millis(50), server.connect()).await
                    });
                    if !matches!(connected, Ok(Ok(()))) {
                        project_switch_debug("windows discovery pipe connect timed out or failed");
                        continue;
                    }
                    project_switch_debug("windows discovery client connected");
                    runtime.block_on(async {
                        let (read, mut write) = tokio::io::split(&mut server);
                        let mut line = String::new();
                        if tokio::time::timeout(Duration::from_secs(2), BufReader::new(read).read_line(&mut line)).await.is_ok() {
                            if let Ok(request) = serde_json::from_str::<Value>(&line) {
                                let challenge = request["params"]["challenge"].as_str().unwrap_or_default();
                                let project = project_for_thread.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
                                project_switch_debug(format_args!("discovery response project={}", project_hint(project.as_deref())));
                                let response = json!({"jsonrpc":"2.0", "id":request["id"], "result": {
                                    "instanceNonce": nonce_for_thread, "pid": std::process::id(), "processStart": process_start,
                                    "projectPath": project,
                                    "sapSocketPath": sap_endpoint_from_env(),
                                    "sapEndpoint": sap_endpoint_from_env(),
                                    "sapToken": std::env::var("SNAPSHOT_SAP_TOKEN").ok(), "challenge": challenge,
                                }});
                                let _ = write.write_all(format!("{response}\n").as_bytes()).await;
                            }
                        }
                    });
                }
                project_switch_debug("windows discovery worker stopped");
                let _ = std::fs::remove_file(descriptor_for_thread);
            })
            .ok()?;
        Some(Self {
            stop,
            descriptor,
            socket,
            project_path,
        })
    }

    fn update(&self, project_path: Option<String>) {
        *self
            .project_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = project_path;
    }
}

#[cfg(any(unix, windows))]
impl Drop for DiscoveryEndpoint {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let _ = std::fs::remove_file(&self.descriptor);
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Owns the non-UI lifecycle loop for one external Snapflow process. The
/// registration and heartbeat never run on the Slint/Qt thread; the returned
/// handle only sends small commands to the worker.
pub struct SnapshotdRegistration {
    commands: Sender<RegistrationCommand>,
    instance_id: Arc<Mutex<Option<String>>>,
    epoch: u64,
    #[cfg(any(unix, windows))]
    discovery: DiscoveryEndpoint,
    #[cfg(unix)]
    project_inventory: Arc<Mutex<Option<Value>>>,
    #[cfg(unix)]
    project_inventory_dirty: Arc<AtomicBool>,
    #[cfg(unix)]
    inventory_stop: Arc<AtomicBool>,
}

impl SnapshotdRegistration {
    /// Return the process-scoped registration. A panel/plugin may be created
    /// more than once during one GUI process, but all instances must share the
    /// same PID/nonce/SAP discovery endpoint and registration worker.
    pub fn start(initial_project_path: Option<String>) -> Option<Arc<Self>> {
        let _init_guard = PROCESS_REGISTRATION_INIT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = PROCESS_REGISTRATION.get() {
            // The Qt host destroys and recreates the Rust panel on File >
            // Close followed by opening another project.  The process-level
            // registration must survive that recreation, but the new panel's
            // initial identity still has to advance the shared lifecycle.
            // `start()` used to return the existing handle without publishing
            // this path, leaving snapshotd with an open row whose last reason
            // was still `closed` and whose project_path was empty.
            if let Some(project_path) = initial_project_path {
                let local_generation = latest_lifecycle()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                    .map(|current| {
                        current
                            .generation
                            .saturating_sub(existing.epoch.saturating_mul(1u64 << 32))
                            .min(u32::MAX as u64)
                            .saturating_add(1)
                    })
                    .unwrap_or(1);
                existing.update(Some(project_path), "opened", local_generation);
            }
            return Some(existing.clone());
        }

        let registration = Arc::new(Self::start_inner(initial_project_path)?);
        let _ = PROCESS_REGISTRATION.set(registration);
        PROCESS_REGISTRATION.get().cloned()
    }

    fn start_inner(initial_project_path: Option<String>) -> Option<Self> {
        // Daemon-owned Snapflow children already have authoritative
        // process-instance rows in snapshotd. They must not also register as
        // external GUI owners, or a cold project.open can observe two owners
        // and fail to drain the headless child deterministically.
        let managed = is_daemon_managed(std::env::var("SNAPSHOTD_MANAGED").ok().as_deref());
        let managed_instance_id = std::env::var("SNAPSHOTD_INSTANCE_ID").ok();
        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = initial_project_path;
            return None;
        }
        #[cfg(any(unix, windows))]
        {
            let epoch = LIFECYCLE_EPOCH.fetch_add(1, Ordering::SeqCst);
            let seeded = seed_lifecycle(epoch, initial_project_path.clone());
            let home = std::env::var_os("SNAPSHOTD_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".snapshotd"))
                })
                .or_else(|| {
                    #[cfg(windows)]
                    {
                        std::env::var_os("USERPROFILE")
                            .map(|home| PathBuf::from(home).join(".snapshotd"))
                    }
                    #[cfg(not(windows))]
                    {
                        None
                    }
                })
                .or_else(|| {
                    eprintln!("panel-rust: snapshotd registration has no SNAPSHOTD_HOME or HOME");
                    None
                })?;
            let client = SnapshotdControlClient::from_default_runtime()?;
            let nonce = uuid::Uuid::new_v4().to_string();
            let discovery =
                match DiscoveryEndpoint::start(&home, &nonce, seeded.project_path.clone()) {
                    Some(discovery) => discovery,
                    None => {
                        eprintln!(
                            "panel-rust: snapshotd discovery endpoint failed (home={})",
                            home.display()
                        );
                        return None;
                    }
                };
            let (commands, receiver) = mpsc::channel();
            project_switch_debug(format_args!(
                "registration starting managed={} initial_project={}",
                managed,
                project_hint(seeded.project_path.as_deref())
            ));
            let instance_id = Arc::new(Mutex::new(None));
            let project_inventory = Arc::new(Mutex::new(None));
            let project_inventory_dirty = Arc::new(AtomicBool::new(false));
            let inventory_stop = Arc::new(AtomicBool::new(false));
            {
                let inventory_client = client.clone();
                let inventory_snapshot = project_inventory.clone();
                let inventory_dirty = project_inventory_dirty.clone();
                let stop = inventory_stop.clone();
                std::thread::Builder::new()
                    .name("snapshotd-project-inventory".into())
                    .spawn(move || {
                        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        else {
                            return;
                        };
                        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                            let subscription =
                                runtime.block_on(inventory_client.subscribe_projects_stream());
                            let Ok(mut subscription) = subscription else {
                                std::thread::sleep(Duration::from_secs(1));
                                continue;
                            };
                            let initial = subscription
                                .initial
                                .get("projects")
                                .cloned()
                                .unwrap_or(subscription.initial.clone());
                            *inventory_snapshot
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(initial);
                            inventory_dirty.store(true, Ordering::SeqCst);
                            while !stop.load(Ordering::SeqCst) {
                                let next = runtime.block_on(async {
                                    tokio::time::timeout(
                                        Duration::from_secs(1),
                                        subscription.updates.recv(),
                                    )
                                    .await
                                });
                                match next {
                                    Ok(Some(value)) => {
                                        *inventory_snapshot
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                            Some(value);
                                        inventory_dirty.store(true, Ordering::SeqCst);
                                    }
                                    Ok(None) | Err(_) => break,
                                }
                            }
                        }
                    })
                    .ok();
            }
            let published_id = Arc::clone(&instance_id);
            std::thread::Builder::new()
                .name("snapshotd-registration".into())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            eprintln!("panel-rust: snapshotd registration runtime failed: {error}");
                            return;
                        }
                    };
                    let mut registration = ExternalInstanceRegistration {
                        instance_nonce: nonce.clone(),
                        pid: std::process::id(),
                        process_start: process_start_identity(),
                        project_path: seeded.project_path.clone(),
                        sap_socket_path: sap_endpoint_from_env(),
                        sap_token: std::env::var("SNAPSHOT_SAP_TOKEN").ok(),
                        capabilities: Some(json!({"lifecycle": true, "mcpContexts": true})),
                    };
                    let mut current_project_path = seeded.project_path.clone();
                    let mut current_reason = seeded.reason.clone();
                    let mut current_generation = seeded.generation;
                    let mut managed_pending = managed;
                    let mut pending_update = false;
                    let mut last_heartbeat = Instant::now();
                    let mut current_id = if managed {
                        managed_instance_id.clone()
                    } else {
                        runtime.block_on(async {
                            register_and_extract_id(&client, &registration).await
                        })
                    };
                    if let Some(id) = current_id.as_ref() {
                        *published_id
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.clone());
                        if managed {
                            managed_pending = runtime
                                .block_on(client.instance_project_changed(
                                    "managed",
                                    id,
                                    None,
                                    current_project_path.as_deref(),
                                    &current_reason,
                                    current_generation,
                                ))
                                .is_err();
                        }
                    }
                    loop {
                        match receiver.recv_timeout(Duration::from_millis(250)) {
                            Ok(RegistrationCommand::Wake) => {
                                project_switch_debug("registration wake received");
                                let latest = latest_lifecycle()
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .clone();
                                let Some(latest) = latest else { continue };
                                if latest.generation <= current_generation {
                                    continue;
                                }
                                current_project_path = latest.project_path;
                                current_reason = latest.reason;
                                current_generation = latest.generation;
                                project_switch_debug(format_args!(
                                    "lifecycle update generation={} reason={} project={}",
                                    current_generation,
                                    current_reason,
                                    project_hint(current_project_path.as_deref())
                                ));
                                // A daemon may have expired or reconciled the
                                // external lease while the GUI stayed alive.
                                // Project selection is an immediate recovery
                                // edge; do not wait for the 10-second heartbeat
                                // interval before registering this process again.
                                if current_id.is_none() && !managed {
                                    registration.project_path = current_project_path.clone();
                                    current_id = runtime.block_on(async {
                                        register_and_extract_id(&client, &registration).await
                                    });
                                    if let Some(id) = current_id.as_ref() {
                                        *published_id
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                            Some(id.clone());
                                    }
                                }
                                if let Some(id) = current_id.as_deref() {
                                    if managed {
                                        managed_pending = runtime
                                            .block_on(client.instance_project_changed(
                                                "managed",
                                                id,
                                                None,
                                                current_project_path.as_deref(),
                                                &current_reason,
                                                current_generation,
                                            ))
                                            .is_err();
                                    } else {
                                        pending_update = runtime
                                            .block_on(client.update_open_project(
                                                id,
                                                current_project_path.as_deref(),
                                                &current_reason,
                                                current_generation,
                                            ))
                                            .is_err();
                                    }
                                }
                            }
                            Ok(RegistrationCommand::Stop)
                            | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                if !managed {
                                    if let Some(id) = current_id.take() {
                                        let _ = runtime
                                            .block_on(client.unregister_external_instance(&id));
                                    }
                                }
                                return;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if let Some(id) = current_id.as_deref() {
                                    if managed && managed_pending {
                                        managed_pending = runtime
                                            .block_on(client.instance_project_changed(
                                                "managed",
                                                id,
                                                None,
                                                current_project_path.as_deref(),
                                                &current_reason,
                                                current_generation,
                                            ))
                                            .is_err();
                                    }
                                    if !managed && pending_update {
                                        pending_update = runtime
                                            .block_on(client.update_open_project(
                                                id,
                                                current_project_path.as_deref(),
                                                &current_reason,
                                                current_generation,
                                            ))
                                            .is_err();
                                    }
                                    if last_heartbeat.elapsed() < Duration::from_secs(10) {
                                        continue;
                                    }
                                    last_heartbeat = Instant::now();
                                    if runtime.block_on(client.heartbeat(id)).is_err() {
                                        registration.project_path = current_project_path.clone();
                                        current_id = if managed {
                                            managed_instance_id.clone()
                                        } else {
                                            runtime.block_on(async {
                                                register_and_extract_id(&client, &registration)
                                                    .await
                                            })
                                        };
                                        if let Some(id) = current_id.as_ref() {
                                            *published_id
                                                .lock()
                                                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                                Some(id.clone());
                                            if managed {
                                                managed_pending = runtime
                                                    .block_on(client.instance_project_changed(
                                                        "managed",
                                                        id,
                                                        None,
                                                        current_project_path.as_deref(),
                                                        &current_reason,
                                                        current_generation,
                                                    ))
                                                    .is_err();
                                            } else {
                                                pending_update = runtime
                                                    .block_on(client.update_open_project(
                                                        id,
                                                        current_project_path.as_deref(),
                                                        &current_reason,
                                                        current_generation,
                                                    ))
                                                    .is_err();
                                            }
                                        }
                                    }
                                } else {
                                    registration.project_path = current_project_path.clone();
                                    current_id = if managed {
                                        managed_instance_id.clone()
                                    } else {
                                        runtime.block_on(async {
                                            register_and_extract_id(&client, &registration).await
                                        })
                                    };
                                    if let Some(id) = current_id.as_ref() {
                                        *published_id
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                            Some(id.clone());
                                        if managed {
                                            managed_pending = runtime
                                                .block_on(client.instance_project_changed(
                                                    "managed",
                                                    id,
                                                    None,
                                                    current_project_path.as_deref(),
                                                    &current_reason,
                                                    current_generation,
                                                ))
                                                .is_err();
                                        } else {
                                            pending_update = runtime
                                                .block_on(client.update_open_project(
                                                    id,
                                                    current_project_path.as_deref(),
                                                    &current_reason,
                                                    current_generation,
                                                ))
                                                .is_err();
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
                .ok()?;
            Some(Self {
                commands,
                instance_id,
                epoch,
                discovery,
                #[cfg(unix)]
                project_inventory,
                #[cfg(unix)]
                project_inventory_dirty,
                #[cfg(unix)]
                inventory_stop,
            })
        }
    }

    pub fn update(&self, project_path: Option<String>, reason: impl Into<String>, generation: u64) {
        let candidate = LifecycleUpdate {
            project_path,
            reason: reason.into(),
            generation: scoped_generation(self.epoch, generation),
        };
        let accepted = {
            let mut latest = latest_lifecycle()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            accept_latest(&mut latest, candidate.clone())
        };
        if !accepted {
            project_switch_debug(format_args!("lifecycle update ignored generation={} project={}", candidate.generation, project_hint(candidate.project_path.as_deref())));
            return;
        }
        project_switch_debug(format_args!("lifecycle update queued generation={} reason={} project={}", candidate.generation, candidate.reason, project_hint(candidate.project_path.as_deref())));
        #[cfg(any(unix, windows))]
        self.discovery.update(candidate.project_path);
        if self.commands.send(RegistrationCommand::Wake).is_err() {
            project_switch_debug("registration wake send failed");
        }
    }

    pub fn instance_id(&self) -> Option<String> {
        self.instance_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Return the newest push inventory exactly once. The reader thread owns
    /// all socket I/O; this accessor is a lock-only UI snapshot operation.
    #[cfg(unix)]
    pub fn take_project_inventory(&self) -> Option<Value> {
        if !self.project_inventory_dirty.swap(false, Ordering::SeqCst) {
            return None;
        }
        self.project_inventory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Consume only the notification edge when the caller uses the existing
    /// bounded list-projects fallback to materialize the typed model.
    #[cfg(unix)]
    pub fn take_project_inventory_notification(&self) -> bool {
        self.take_project_inventory().is_some()
    }
}

/// Daemon-launched GUI children already have a managed process-instance row.
/// Only a native/self-launched GUI should create an external-instance lease.
fn is_daemon_managed(marker: Option<&str>) -> bool {
    marker == Some("1")
}

impl Drop for SnapshotdRegistration {
    fn drop(&mut self) {
        #[cfg(unix)]
        self.inventory_stop.store(true, Ordering::SeqCst);
        let _ = self.commands.send(RegistrationCommand::Stop);
    }
}

async fn register_and_extract_id(
    client: &SnapshotdControlClient,
    registration: &ExternalInstanceRegistration,
) -> Option<String> {
    let mut last_error = None;
    for attempt in 0..10 {
        match client.register_external_instance(registration).await {
            Ok(value) => {
                return value
                    .get("instance")?
                    .get("instanceId")?
                    .as_str()
                    .map(str::to_owned);
            }
            Err(error) => {
                if attempt == 0 || attempt == 9 {
                    eprintln!(
                        "panel-rust: snapshotd external registration attempt {} failed: {error}",
                        attempt + 1
                    );
                }
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let _ = last_error;
    None
}

fn process_start_identity() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        if let Some(start) = stat.split_whitespace().nth(21) {
            return start.to_owned();
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let handle = unsafe { GetCurrentProcess() };
        let mut creation = std::mem::MaybeUninit::uninit();
        let mut exit = std::mem::MaybeUninit::uninit();
        let mut kernel = std::mem::MaybeUninit::uninit();
        let mut user = std::mem::MaybeUninit::uninit();
        let ok = unsafe {
            GetProcessTimes(
                handle,
                creation.as_mut_ptr(),
                exit.as_mut_ptr(),
                kernel.as_mut_ptr(),
                user.as_mut_ptr(),
            )
        };
        if ok != 0 {
            let creation = unsafe { creation.assume_init() };
            let value =
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
            return value.to_string();
        }
    }
    format!("{}", std::process::id())
}

/// Return the SAP endpoint explicitly supplied by the SAP host.
///
/// This intentionally does not derive a value from the discovery endpoint
/// (`~/.snapshotd/apps/<pid>.sock`).  That socket only answers `app.discover`
/// and speaks SDP discovery JSON; it is not a SAP server.  A real direct GUI
/// owner must have Qt/Shotcut's embedded `sap_start_server` publish one of
/// these variables before registration.  Daemon-launched children receive
/// them from snapshotd's process manager.
fn sap_endpoint_from_env() -> Option<String> {
    select_sap_endpoint(
        std::env::var("SNAPSHOT_SAP_ENDPOINT").ok().as_deref(),
        std::env::var("SNAPSHOT_SAP_SOCKET").ok().as_deref(),
    )
}

fn select_sap_endpoint(endpoint: Option<&str>, socket: Option<&str>) -> Option<String> {
    endpoint
        .filter(|value| !value.is_empty())
        .or_else(|| socket.filter(|value| !value.is_empty()))
        .map(str::to_owned)
}

impl SnapshotdControlClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            next_id: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[cfg(windows)]
    pub fn from_default_runtime() -> Option<Self> {
        let endpoint = std::env::var_os("SNAPSHOTD_CONTROL_ENDPOINT")
            .or_else(|| std::env::var_os("SNAPSHOTD_CONTROL_SOCKET"))
            .map(PathBuf::from)
            .or_else(|| {
                let scope = std::env::var("USERNAME")
                    .or_else(|_| std::env::var("USER"))
                    .unwrap_or_else(|_| "default".into());
                let scope: String = scope
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect();
                Some(PathBuf::from(format!(
                    "\\\\.\\pipe\\snapflow-{scope}-control"
                )))
            })?;
        Some(Self::new(endpoint))
    }

    #[cfg(not(windows))]
    pub fn from_default_runtime() -> Option<Self> {
        if let Some(endpoint) = std::env::var_os("SNAPSHOTD_CONTROL_ENDPOINT")
            .or_else(|| std::env::var_os("SNAPSHOTD_CONTROL_SOCKET"))
        {
            return Some(Self::new(PathBuf::from(endpoint)));
        }
        let home = std::env::var_os("SNAPSHOTD_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".snapshotd"))
            })?;
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
        self.call(
            "daemon.registerExternalInstance",
            serde_json::to_value(registration)
                .map_err(|e| SnapshotdClientError::Malformed(e.to_string()))?,
        )
        .await
    }

    pub async fn update_open_project(
        &self,
        instance_id: &str,
        project_path: Option<&str>,
        reason: &str,
        generation: u64,
    ) -> Result<Value, SnapshotdClientError> {
        let started = Instant::now();
        project_switch_debug(format_args!("RPC updateOpenProject begin generation={} project={}", generation, project_hint(project_path)));
        let result = self.call(
            "daemon.updateOpenProject",
            json!({
                "instanceId": instance_id,
                "projectPath": project_path,
                "reason": reason,
                "generation": generation,
            }),
        )
        .await;
        project_switch_debug(format_args!("RPC updateOpenProject end elapsed_ms={} result={}", started.elapsed().as_millis(), if result.is_ok() { "ok" } else { "error" }));
        result
    }

    pub async fn instance_project_changed(
        &self,
        owner: &str,
        instance_id: &str,
        instance_nonce: Option<&str>,
        project_path: Option<&str>,
        reason: &str,
        generation: u64,
    ) -> Result<Value, SnapshotdClientError> {
        let started = Instant::now();
        project_switch_debug(format_args!("RPC instanceProjectChanged begin owner={} generation={} reason={} project={}", owner, generation, reason, project_hint(project_path)));
        let result = self.call(
            "daemon.instanceProjectChanged",
            json!({
                "owner": owner, "instanceId": instance_id, "instanceNonce": instance_nonce,
                "pid": std::process::id(), "processStart": process_start_identity(),
                "projectPath": project_path, "reason": reason, "generation": generation,
            }),
        )
        .await;
        project_switch_debug(format_args!("RPC instanceProjectChanged end elapsed_ms={} result={}", started.elapsed().as_millis(), if result.is_ok() { "ok" } else { "error" }));
        result
    }

    pub async fn heartbeat(&self, instance_id: &str) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.heartbeat", json!({"instanceId": instance_id}))
            .await
    }

    pub async fn unregister_external_instance(
        &self,
        instance_id: &str,
    ) -> Result<Value, SnapshotdClientError> {
        self.call(
            "daemon.unregisterExternalInstance",
            json!({"instanceId": instance_id}),
        )
        .await
    }

    pub async fn register_mcp_context(
        &self,
        registration: &McpContextRegistration,
    ) -> Result<Value, SnapshotdClientError> {
        self.call(
            "daemon.registerMcpContext",
            serde_json::to_value(registration)
                .map_err(|e| SnapshotdClientError::Malformed(e.to_string()))?,
        )
        .await
    }

    pub async fn set_mcp_project_target(
        &self,
        context_token: &str,
        project_id: &str,
    ) -> Result<Value, SnapshotdClientError> {
        self.call(
            "daemon.setMcpProjectTarget",
            json!({"contextToken": context_token, "projectId": project_id}),
        )
        .await
    }

    pub async fn unregister_mcp_context(
        &self,
        context_token: &str,
    ) -> Result<Value, SnapshotdClientError> {
        self.call(
            "daemon.unregisterMcpContext",
            json!({"contextToken": context_token}),
        )
        .await
    }

    pub async fn list_projects(&self) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.listProjects", json!({})).await
    }

    /// Open (or attach to) a project via the control socket. Reaches the
    /// same ForwardSAP `project.select` / Router.Bind path as the MCP
    /// tool of that name -- first select loads .mlt; later attaches reuse
    /// the pooled process/connection without reloading.
    pub async fn open_project(&self, project_id: &str) -> Result<Value, SnapshotdClientError> {
        self.call("project.open", json!({"projectId": project_id}))
            .await
    }

    /// Open or attach via filesystem path (folder or .mlt).
    pub async fn open_project_path(&self, path: &str) -> Result<Value, SnapshotdClientError> {
        self.call("project.open", json!({"path": path})).await
    }

    /// Release this control-socket session's project binding. Other
    /// sessions bound to the same project are unaffected (Router.Unbind
    /// only). Same semantics as MCP `project.close`.
    pub async fn close_project(&self) -> Result<Value, SnapshotdClientError> {
        self.call("project.close", json!({})).await
    }

    pub async fn subscribe_projects(&self) -> Result<Value, SnapshotdClientError> {
        self.call("daemon.subscribeProjects", json!({})).await
    }

    /// Open a persistent JSONL control connection for project inventory
    /// updates. Ordinary `call` requests intentionally remain one-shot; this
    /// method is the opt-in long-lived transport used by panel inventory
    /// consumers.
    #[cfg(any(unix, windows))]
    pub async fn subscribe_projects_stream(
        &self,
    ) -> Result<ProjectUpdateSubscription, SnapshotdClientError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request =
            json!({"jsonrpc": "2.0", "id": id, "method": "daemon.subscribeProjects", "params": {}});
        let stream = connect_stream(&self.socket_path).await?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut line = serde_json::to_vec(&request)
            .map_err(|error| SnapshotdClientError::Malformed(error.to_string()))?;
        line.push(b'\n');
        tokio::time::timeout(REQUEST_TIMEOUT, write_half.write_all(&line))
            .await
            .map_err(|_| SnapshotdClientError::Timeout)?
            .map_err(|error| SnapshotdClientError::Unavailable(error.to_string()))?;

        let mut reader = BufReader::new(read_half);
        let mut response = String::new();
        tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut response))
            .await
            .map_err(|_| SnapshotdClientError::Timeout)?
            .map_err(|error| SnapshotdClientError::Unavailable(error.to_string()))?;
        let value: Value = serde_json::from_str(&response)
            .map_err(|error| SnapshotdClientError::Malformed(error.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(SnapshotdClientError::Rpc(error.to_string()));
        }
        let initial = value
            .get("result")
            .cloned()
            .ok_or_else(|| SnapshotdClientError::Malformed("response has no result".into()))?;
        let (updates, receiver) = tokio::sync::mpsc::unbounded_channel();
        let reader_task = tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                let read = match reader.read_line(&mut line).await {
                    Ok(read) => read,
                    Err(_) => break,
                };
                if read == 0 {
                    break;
                }
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("method").and_then(Value::as_str) == Some("daemon.projectsChanged") {
                    if updates
                        .send(frame.get("params").cloned().unwrap_or(Value::Null))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Ok(ProjectUpdateSubscription {
            initial,
            updates: receiver,
            _writer: write_half,
            reader_task,
        })
    }

    /// Resolve the daemon project id from the canonical MLT path. This keeps
    /// the panel's durable identity path-based while still supplying the
    /// daemon's opaque project id for MCP context registration.
    pub async fn project_id_for_path(
        &self,
        project_path: &Path,
    ) -> Result<Option<String>, SnapshotdClientError> {
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
        let Some(rows) = projects.as_array() else {
            return Ok(None);
        };
        let candidates: Vec<(String, PathBuf)> = rows
            .iter()
            .filter_map(|row| {
                let id = row
                    .get("id")
                    .or_else(|| row.get("ID"))
                    .and_then(Value::as_str)?;
                let root = row
                    .get("rootDir")
                    .or_else(|| row.get("RootDir"))
                    .and_then(Value::as_str)?;
                let file = row
                    .get("mltFileName")
                    .or_else(|| row.get("MltFileName"))
                    .and_then(Value::as_str)?;
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

    async fn call_once(&self, request: &Value) -> Result<Value, SnapshotdClientError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stream = connect_stream(&self.socket_path).await?;
        let (read_half, mut write_half) = tokio::io::split(stream);
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

    #[cfg(not(any(unix, windows)))]
    async fn call_once(&self, _request: &Value) -> Result<Value, SnapshotdClientError> {
        Err(SnapshotdClientError::Unsupported)
    }
}

#[cfg(unix)]
type DaemonStream = tokio::net::UnixStream;
#[cfg(windows)]
type DaemonStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(any(unix, windows))]
async fn connect_stream(endpoint: &Path) -> Result<DaemonStream, SnapshotdClientError> {
    #[cfg(unix)]
    let connect = tokio::net::UnixStream::connect(endpoint);
    #[cfg(windows)]
    let connect = async {
        let endpoint = endpoint.to_owned();
        tokio::task::spawn_blocking(move || {
            tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint)
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
    };
    tokio::time::timeout(REQUEST_TIMEOUT, connect)
        .await
        .map_err(|_| SnapshotdClientError::Timeout)?
        .map_err(|error| SnapshotdClientError::Unavailable(error.to_string()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[test]
    fn managed_children_do_not_register_as_external_instances() {
        assert!(is_daemon_managed(Some("1")));
        assert!(!is_daemon_managed(None));
        assert!(!is_daemon_managed(Some("0")));
    }

    #[test]
    fn lifecycle_coalescing_rejects_stale_close_after_newer_open() {
        let mut latest = Some(LifecycleUpdate {
            project_path: Some("new.mlt".into()),
            reason: "opened".into(),
            generation: 10,
        });
        assert!(!accept_latest(
            &mut latest,
            LifecycleUpdate {
                project_path: None,
                reason: "closed".into(),
                generation: 9,
            }
        ));
        assert!(!accept_latest(
            &mut latest,
            LifecycleUpdate {
                project_path: None,
                reason: "closed".into(),
                generation: 10,
            }
        ));
        assert_eq!(latest.unwrap().project_path.as_deref(), Some("new.mlt"));
    }

    #[test]
    fn lifecycle_epoch_keeps_recreated_panel_newer_than_old_events() {
        assert!(scoped_generation(2, 1) > scoped_generation(1, u64::MAX));
    }

    #[test]
    fn sap_endpoint_selection_never_falls_back_to_discovery_socket() {
        // The app discovery socket is deliberately not an input to this
        // helper.  Only an endpoint explicitly published by the SAP host can
        // be registered as `sapSocketPath`.
        assert_eq!(select_sap_endpoint(None, None), None);
        assert_eq!(
            select_sap_endpoint(Some("/run/project.sock"), Some("/tmp/legacy.sock")),
            Some("/run/project.sock".to_owned())
        );
        assert_eq!(
            select_sap_endpoint(None, Some("/tmp/legacy.sock")),
            Some("/tmp/legacy.sock".to_owned())
        );
        assert_eq!(select_sap_endpoint(Some(""), Some("")), None);
    }

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
                    Some("daemon.registerExternalInstance") => {
                        json!({"instance": {"instanceId": "instance-1"}, "heartbeatEvery": "10s", "leaseDuration": "30s"})
                    }
                    Some("daemon.setMcpProjectTarget") => {
                        json!({"contextToken": "ctx", "targetProjectId": "project-b"})
                    }
                    Some("daemon.unregisterMcpContext") => json!({}),
                    Some("daemon.subscribeProjects") => {
                        json!({"mode": "push", "pollAfter": "5s", "projects": []})
                    }
                    other => panic!("unexpected method: {other:?}"),
                };
                write
                    .write_all(
                        json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
                            .to_string()
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                write.write_all(b"\n").await.unwrap();
            }
        });
        let client = SnapshotdControlClient::new(&socket);
        let registration = ExternalInstanceRegistration {
            instance_nonce: "nonce".into(),
            pid: 1,
            process_start: "start".into(),
            project_path: None,
            sap_socket_path: None,
            sap_token: None,
            capabilities: None,
        };
        let registered = client
            .register_external_instance(&registration)
            .await
            .unwrap();
        assert_eq!(registered["instance"]["instanceId"], "instance-1");
        let context = client
            .set_mcp_project_target("ctx", "project-b")
            .await
            .unwrap();
        assert_eq!(context["targetProjectId"], "project-b");
        client.unregister_mcp_context("ctx").await.unwrap();
        let subscription = client.subscribe_projects().await.unwrap();
        assert_eq!(subscription["mode"], "push");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn persistent_project_subscription_receives_inventory_notification() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "daemon.subscribeProjects");
            write
                .write_all(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"mode": "push", "projects": []}
                    })
                    .to_string()
                    .as_bytes(),
                )
                .await
                .unwrap();
            write.write_all(b"\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            write
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"method\":\"daemon.projectsChanged\",\"params\":[{\"id\":\"project-a\"}]}\n",
                )
                .await
                .unwrap();
            write.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let client = SnapshotdControlClient::new(&socket);
        let mut subscription = client.subscribe_projects_stream().await.unwrap();
        assert_eq!(subscription.initial["mode"], "push");
        let update = tokio::time::timeout(Duration::from_secs(1), subscription.updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update[0]["id"], "project-a");
        drop(subscription);
        server.await.unwrap();
    }
}
