//! Small panel-side SDP lifecycle client.
//!
//! Project changes originate in the Qt host, but snapshotd owns the process
//! registry. This module is the missing Unix control-socket bridge: it sends
//! GUI register/open/close notifications without routing lifecycle through
//! the project-scoped MCP/SAP connection.

use serde_json::{json, Value};
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{atomic::{AtomicU64, Ordering}, Mutex, OnceLock};
#[cfg(unix)]
use std::time::Duration;

static OPEN_PROJECT: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
static LAST_HEARTBEAT: AtomicU64 = AtomicU64::new(0);

fn state() -> &'static Mutex<Option<(String, String)>> {
    OPEN_PROJECT.get_or_init(|| Mutex::new(None))
}

fn client_id() -> String {
    std::env::var("SNAPSHOTD_GUI_CLIENT_ID")
        .or_else(|_| std::env::var("SNAPFLOW_CLIENT_ID"))
        .unwrap_or_else(|_| format!("panel-gui-{}", std::process::id()))
}

#[cfg(unix)]
fn socket_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SNAPSHOTD_CONTROL_SOCKET") {
        if !path.is_empty() { return Some(PathBuf::from(path)); }
    }
    if let Ok(home) = std::env::var("SNAPSHOTD_HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join("control.sock"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|home| PathBuf::from(home).join(".snapshotd/control.sock"))
}

#[cfg(unix)]
fn call(method: &str, params: Value) -> Result<Value, String> {
    let path = socket_path().ok_or_else(|| "snapshotd control socket is not configured".to_owned())?;
    let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
    stream.write_all(request.to_string().as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(b"\n").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| e.to_string())?;
    let response: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if let Some(error) = response.get("error") { return Err(error.to_string()); }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

// Rust's std doesn't expose a Windows unix-domain-socket client, so the SDP
// control-socket bridge is unix-only for now; GUI lifecycle notifications
// are a best-effort side channel and every caller already treats errors as
// non-fatal, so a stubbed Err here is a safe no-op on Windows.
#[cfg(not(unix))]
fn call(_method: &str, _params: Value) -> Result<Value, String> {
    Err("snapshotd control socket bridge is not supported on this platform".to_owned())
}

pub(crate) fn project_changed(path: Option<String>) {
    let client = client_id();
    std::thread::spawn(move || {
        match path {
            Some(path) if !path.is_empty() => {
                let result = call("daemon.projectOpen", json!({
                    "clientId": client, "projectPath": path, "mode": "gui", "headless": false
                }));
                if let Ok(value) = result {
                    if let (Some(project), Some(instance)) = (value.get("projectId").and_then(Value::as_str), value.get("instanceId").and_then(Value::as_str)) {
                        if let Ok(mut slot) = state().lock() { *slot = Some((project.to_owned(), instance.to_owned())); }
                    }
                }
            }
            _ => {
                let prior = state().lock().ok().and_then(|mut slot| slot.take());
                if let Some((project, _)) = prior {
                    let _ = call("daemon.projectClose", json!({"clientId": client, "projectId": project, "save": true}));
                }
            }
        }
    });
}

pub(crate) fn heartbeat() {
    let client = client_id();
    std::thread::spawn(move || { let _ = call("daemon.clientHeartbeat", json!({"clientId": client})); });
}

pub(crate) fn heartbeat_if_due() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let prior = LAST_HEARTBEAT.load(Ordering::Relaxed);
    if now.saturating_sub(prior) >= 10 && LAST_HEARTBEAT.compare_exchange(prior, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        heartbeat();
    }
}
