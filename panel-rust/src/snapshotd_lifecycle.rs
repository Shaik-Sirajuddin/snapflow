//! Small panel-side SDP lifecycle client.
//!
//! Project changes originate in the Qt host, but snapshotd owns the process
//! registry. This module is the missing Unix control-socket bridge: it sends
//! GUI register/open/close notifications without routing lifecycle through
//! the project-scoped MCP/SAP connection.

use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};

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

fn call(method: &str, params: Value) -> Result<Value, String> {
    let client = crate::snapshotd_client::SnapshotdControlClient::from_default_runtime()
        .ok_or_else(|| "snapshotd control endpoint is not configured".to_owned())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(client.call(method, params))
        .map_err(|e| e.to_string())
}

pub(crate) fn project_changed(path: Option<String>) {
    let client = client_id();
    std::thread::spawn(move || match path {
        Some(path) if !path.is_empty() => {
            let result = call(
                "daemon.projectOpen",
                json!({
                    "clientId": client, "projectPath": path, "mode": "gui", "headless": false
                }),
            );
            if let Ok(value) = result {
                if let (Some(project), Some(instance)) = (
                    value.get("projectId").and_then(Value::as_str),
                    value.get("instanceId").and_then(Value::as_str),
                ) {
                    if let Ok(mut slot) = state().lock() {
                        *slot = Some((project.to_owned(), instance.to_owned()));
                    }
                }
            }
        }
        _ => {
            let prior = state().lock().ok().and_then(|mut slot| slot.take());
            if let Some((project, _)) = prior {
                let _ = call(
                    "daemon.projectClose",
                    json!({"clientId": client, "projectId": project, "save": true}),
                );
            }
        }
    });
}

pub(crate) fn heartbeat() {
    let client = client_id();
    std::thread::spawn(move || {
        let _ = call("daemon.clientHeartbeat", json!({"clientId": client}));
    });
}

pub(crate) fn heartbeat_if_due() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let prior = LAST_HEARTBEAT.load(Ordering::Relaxed);
    if now.saturating_sub(prior) >= 10
        && LAST_HEARTBEAT
            .compare_exchange(prior, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        heartbeat();
    }
}
