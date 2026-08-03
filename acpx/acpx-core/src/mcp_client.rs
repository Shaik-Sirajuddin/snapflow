//! Real MCP (Model Context Protocol) client capability -- settings-only.
//!
//! acpx has otherwise never itself been an MCP client: `mcp_servers.rs`'s
//! `McpServerStore` only ever stores/merges opaque server *config*, which
//! is handed to the **backend agent** at `session/new` -- the agent is
//! the one that actually connects and speaks MCP during a real session.
//! This module exists purely so the Settings UI can preview a configured
//! server's real advertised tools *before* any chat session exists to
//! piggyback the connection on (see `router.rs`'s `mcp_servers/tools_
//! fetch` RPC and `Router::spawn_mcp_tools_fetch`'s doc comments for the
//! full request flow). It has no effect on how a real session's backend
//! agent talks to MCP servers.
//!
//! Deliberately hand-rolled, no MCP SDK dependency -- matches this
//! workspace's existing convention (see `panel-rust/src/bin/
//! snapflowd_mcp.rs`'s own doc comment, the real MCP server this module's
//! tests probe against). Stdio framing reuses `acpx_conductor::framing`
//! (newline-delimited JSON-RPC, the same shape ACP's own stdio transport
//! and MCP's stdio transport both use) rather than `acpx_conductor::
//! process::BackendProcess` -- that type is a *supervised, long-lived*
//! backend-agent process (crash/restart backoff, terminal tracking,
//! demux, npx-cache warming); a `tools/list` preview is a one-shot probe
//! that spawns, asks two questions, and exits, so reusing just the
//! framing primitives over a plain `tokio::process::Command` is a better
//! fit than pulling in all of that machinery.

use acpx_conductor::framing::{FramedReader, FramedWriter};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One tool an MCP server advertised via a real `tools/list` response.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Last-known state of one server's background `tools/list` probe, as
/// tracked by [`McpToolCatalogCache`]. `mcp_servers/tools_fetch` writes
/// `Fetching` immediately (before the detached task even starts, so a
/// racing poller never sees a stale/absent entry as "nothing in
/// flight") then overwrites it with `Ready`/`Error` once the real probe
/// finishes; `mcp_servers/list` reads whatever is currently here to
/// build each entry's `tools` field.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolCatalogState {
    Fetching,
    Ready { tools: Vec<McpToolInfo> },
    Error { message: String },
}

/// Cheap-clone (`Arc<Mutex<..>>`-backed) per-server cache of the last
/// `tools/list` probe result, mirroring `crate::oauth::OAuthTokenCache`'s
/// exact shape and rationale: the real fetch runs in a `tokio::spawn`ed
/// detached task (see `Router::spawn_mcp_tools_fetch`) well past the
/// `mcp_servers/tools_fetch` RPC call's own `&Router` borrow, so the task
/// needs its own owned handle into this state rather than a reference
/// back into a `Router` that may no longer be being dispatched against by
/// the time the probe completes.
#[derive(Debug, Default, Clone)]
pub struct McpToolCatalogCache {
    entries: Arc<Mutex<HashMap<String, ToolCatalogState>>>,
}

impl McpToolCatalogCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, server_name: &str) -> Option<ToolCatalogState> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(server_name)
            .cloned()
    }

    pub fn insert(&self, server_name: impl Into<String>, state: ToolCatalogState) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(server_name.into(), state);
    }

    /// `true` if a fetch for this server is already in flight -- lets the
    /// RPC handler skip spawning a duplicate probe when the UI polls
    /// faster than a slow server responds.
    pub fn is_fetching(&self, server_name: &str) -> bool {
        matches!(self.get(server_name), Some(ToolCatalogState::Fetching))
    }

    pub fn remove(&self, server_name: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(server_name);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    #[error("failed to spawn MCP server process: {0}")]
    Spawn(std::io::Error),
    #[error("MCP server process has no stdin/stdout pipes")]
    MissingPipes,
    #[error("MCP server stdio framing error: {0}")]
    Framing(#[from] acpx_conductor::framing::FramingError),
    #[error("MCP server returned a JSON-RPC error for {method}: {message}")]
    RpcError { method: &'static str, message: String },
    #[error("timed out waiting for the MCP server's {0} response")]
    Timeout(&'static str),
    #[error("MCP server HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MCP server HTTP response for {method} was not valid JSON: {detail}")]
    MalformedHttpResponse { method: &'static str, detail: String },
}

/// What to spawn for a stdio-transport MCP server -- deliberately the
/// same three fields as `acpx_conductor::process::SpawnSpec` (command/
/// args/env), just not that exact type: this crate doesn't depend on
/// `acpx-conductor`'s own `SpawnSpec` staying probe-shaped forever (it's
/// the supervised-backend-agent type, evolving for that purpose).
#[derive(Debug, Clone)]
pub struct StdioSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// What to call for an http-transport MCP server. `headers` carries any
/// static or (already-resolved) OAuth `Authorization` header the caller
/// wants attached -- same header-injection contract `Router::inject_
/// oauth_headers` already uses for session/new, reused here rather than
/// duplicated.
#[derive(Debug, Clone)]
pub struct HttpSpec {
    pub url: String,
    pub headers: HashMap<String, String>,
}

pub enum ProbeSpec {
    Stdio(StdioSpec),
    Http(HttpSpec),
}

/// Extracts a [`ProbeSpec`] from a raw MCP server config entry (the same
/// opaque `serde_json::Value` shape `McpServerStore` holds -- see that
/// module's doc comment for why acpx never re-types these fields).
/// Dispatches on `entry["type"]` ("stdio" vs "http"); returns `None` for
/// an entry with a missing/unrecognized type or missing required fields
/// (`command` for stdio, `url` for http) rather than erroring, since a
/// malformed/incomplete entry simply isn't probeable yet -- the caller
/// (`Router::spawn_mcp_tools_fetch`) turns that into an `Error` catalog
/// state with a clear message.
///
/// Callers wanting a live OAuth bearer token attached to an http entry's
/// headers should pass `entry` through `Router::inject_oauth_headers`
/// first -- this function only reads whatever static `headers` object is
/// already on the entry, exactly like the stdio side only reads whatever
/// static `env` map is already there.
pub fn probe_spec_from_entry(entry: &serde_json::Value) -> Option<ProbeSpec> {
    let transport = entry.get("type").and_then(|t| t.as_str())?;
    match transport {
        "stdio" => {
            let command = entry.get("command").and_then(|c| c.as_str())?.to_string();
            let args = entry
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let env = entry
                .get("env")
                .and_then(|e| e.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Some(ProbeSpec::Stdio(StdioSpec { command, args, env }))
        }
        "http" => {
            let url = entry.get("url").and_then(|u| u.as_str())?.to_string();
            let headers = entry
                .get("headers")
                .and_then(|h| h.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Some(ProbeSpec::Http(HttpSpec { url, headers }))
        }
        _ => None,
    }
}

/// Connects to one MCP server (spawn-and-speak for stdio, two sequential
/// JSON-RPC POSTs for http), performs the real `initialize` handshake,
/// then `tools/list`, and returns the real advertised tools. Bounded by
/// `timeout` end to end (covers connect + both round trips) -- the whole
/// probe is a fire-and-forget background task kicked off by `Router::
/// spawn_mcp_tools_fetch`, never run while holding the router's global
/// lock, so a hung/slow server here never blocks anything else on the
/// gateway (see that function's doc comment for the full reasoning).
pub async fn probe(
    http_client: &reqwest::Client,
    spec: ProbeSpec,
    timeout: Duration,
) -> Result<Vec<McpToolInfo>, McpClientError> {
    match spec {
        ProbeSpec::Stdio(stdio) => probe_stdio(stdio, timeout).await,
        ProbeSpec::Http(http) => probe_http(http_client, &http, timeout).await,
    }
}

pub async fn probe_stdio(
    spec: StdioSpec,
    timeout: Duration,
) -> Result<Vec<McpToolInfo>, McpClientError> {
    tokio::time::timeout(timeout, probe_stdio_inner(spec))
        .await
        .map_err(|_| McpClientError::Timeout("tools/list"))?
}

async fn probe_stdio_inner(spec: StdioSpec) -> Result<Vec<McpToolInfo>, McpClientError> {
    let mut command = tokio::process::Command::new(&spec.command);
    command
        .args(&spec.args)
        .envs(&spec.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // A probe that outlives its own timeout (the `tokio::time::
        // timeout` above already bounds the *await*, but a killed future
        // doesn't imply a killed child) must not leak the subprocess --
        // `kill_on_drop` guarantees the child dies when this function
        // returns/is dropped either way.
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(McpClientError::Spawn)?;
    let stdin = child.stdin.take().ok_or(McpClientError::MissingPipes)?;
    let stdout = child.stdout.take().ok_or(McpClientError::MissingPipes)?;
    let mut writer = FramedWriter::new(stdin);
    let mut reader = FramedReader::new(stdout);

    writer
        .write_value(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "acpx", "version": env!("CARGO_PKG_VERSION") },
            }
        }))
        .await?;
    let init_response = reader.read_value().await?;
    if let Some(error) = init_response.get("error") {
        return Err(McpClientError::RpcError {
            method: "initialize",
            message: error.to_string(),
        });
    }

    // Notification (no "id") -- per MCP's lifecycle, sent after a
    // successful initialize and before any other request; no response is
    // expected or read for it.
    writer
        .write_value(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
        .await?;

    writer
        .write_value(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        }))
        .await?;
    let tools_response = reader.read_value().await?;

    let _ = child.kill().await;
    parse_tools_list_result(&tools_response, "tools/list")
}

pub async fn probe_http(
    client: &reqwest::Client,
    spec: &HttpSpec,
    timeout: Duration,
) -> Result<Vec<McpToolInfo>, McpClientError> {
    tokio::time::timeout(timeout, probe_http_inner(client, spec))
        .await
        .map_err(|_| McpClientError::Timeout("tools/list"))?
}

async fn probe_http_inner(
    client: &reqwest::Client,
    spec: &HttpSpec,
) -> Result<Vec<McpToolInfo>, McpClientError> {
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "acpx", "version": env!("CARGO_PKG_VERSION") },
        }
    });
    let init_response = post_json_rpc(client, spec, &init_body, "initialize").await?;
    if let Some(error) = init_response.get("error") {
        return Err(McpClientError::RpcError {
            method: "initialize",
            message: error.to_string(),
        });
    }

    let tools_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {},
    });
    let tools_response = post_json_rpc(client, spec, &tools_body, "tools/list").await?;
    parse_tools_list_result(&tools_response, "tools/list")
}

/// One JSON-RPC request per POST. `Accept: application/json` asks a
/// spec-compliant server for the plain (non-SSE) half of the MCP
/// streamable-HTTP transport convention, but real-world servers do not
/// all honor that header -- some always reply `Content-Type: text/
/// event-stream` with the JSON-RPC payload wrapped in a single `data:
/// {...}` frame even for a one-shot POST. Reading the body as text first
/// (rather than handing the response straight to `reqwest::Response::
/// json`, which fails with an opaque "error decoding response body" the
/// moment the bytes aren't bare JSON) lets this function unwrap that one
/// common SSE-framing case before giving up, and -- when the body truly
/// isn't JSON-RPC (an HTTP error page, empty body, garbage) -- report the
/// response status and a truncated snippet instead of reqwest's generic
/// decode-error string, which carries none of that context.
async fn post_json_rpc(
    client: &reqwest::Client,
    spec: &HttpSpec,
    body: &serde_json::Value,
    method: &'static str,
) -> Result<serde_json::Value, McpClientError> {
    let mut request = client
        .post(&spec.url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(body);
    for (key, value) in &spec.headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
        return Ok(value);
    }
    if let Some(value) = extract_sse_json_data(&text) {
        return Ok(value);
    }

    let mut snippet: String = text.chars().take(200).collect();
    if snippet.len() < text.len() {
        snippet.push('\u{2026}');
    }
    if snippet.is_empty() {
        snippet.push_str("<empty body>");
    }
    Err(McpClientError::MalformedHttpResponse {
        method,
        detail: format!("HTTP status {status}, body: {snippet}"),
    })
}

/// Pulls the JSON-RPC payload out of a single-event `text/event-stream`
/// body (`data: {...}` lines, blank-line-terminated, per the SSE format
/// the MCP streamable-HTTP transport layers JSON-RPC over). Only the
/// first event's `data:` payload(s) are used -- a one-shot `initialize`/
/// `tools/list` probe expects exactly one JSON-RPC response, not a
/// stream of them. Returns `None` (letting the caller fall through to
/// its own malformed-response error) when no line looks like an SSE data
/// frame at all, so this never masks a genuinely non-JSON, non-SSE body.
fn extract_sse_json_data(text: &str) -> Option<serde_json::Value> {
    let mut data_lines = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        } else if !data_lines.is_empty() && line.trim().is_empty() {
            // Blank line ends the first event -- stop collecting so a
            // later, unrelated event in the same body can't get merged
            // in.
            break;
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    serde_json::from_str(&data_lines.join("\n")).ok()
}

fn parse_tools_list_result(
    value: &serde_json::Value,
    method: &'static str,
) -> Result<Vec<McpToolInfo>, McpClientError> {
    if let Some(error) = value.get("error") {
        return Err(McpClientError::RpcError {
            method,
            message: error.to_string(),
        });
    }
    let tools = value
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(|tools| tools.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(tools
        .into_iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_string();
            let description = tool
                .get("description")
                .and_then(|d| d.as_str())
                .map(str::to_string);
            Some(McpToolInfo { name, description })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_spec_from_entry_extracts_stdio_fields() {
        let entry = serde_json::json!({
            "name": "fs",
            "type": "stdio",
            "command": "snapflowd-mcp",
            "args": ["--skills-dir", "/tmp/skills"],
            "env": {"RUST_LOG": "info"},
        });
        match probe_spec_from_entry(&entry).expect("stdio entry should yield a spec") {
            ProbeSpec::Stdio(spec) => {
                assert_eq!(spec.command, "snapflowd-mcp");
                assert_eq!(spec.args, vec!["--skills-dir", "/tmp/skills"]);
                assert_eq!(spec.env.get("RUST_LOG").map(String::as_str), Some("info"));
            }
            ProbeSpec::Http(_) => panic!("expected a stdio spec"),
        }
    }

    #[test]
    fn probe_spec_from_entry_extracts_http_fields_with_headers() {
        let entry = serde_json::json!({
            "name": "remote",
            "type": "http",
            "url": "https://mcp.example.com/rpc",
            "headers": {"Authorization": "Bearer abc123"},
        });
        match probe_spec_from_entry(&entry).expect("http entry should yield a spec") {
            ProbeSpec::Http(spec) => {
                assert_eq!(spec.url, "https://mcp.example.com/rpc");
                assert_eq!(
                    spec.headers.get("Authorization").map(String::as_str),
                    Some("Bearer abc123")
                );
            }
            ProbeSpec::Stdio(_) => panic!("expected an http spec"),
        }
    }

    #[test]
    fn probe_spec_from_entry_returns_none_for_incomplete_or_unknown_entries() {
        assert!(probe_spec_from_entry(&serde_json::json!({"name": "x"})).is_none());
        assert!(probe_spec_from_entry(&serde_json::json!({"name": "x", "type": "stdio"})).is_none());
        assert!(probe_spec_from_entry(&serde_json::json!({"name": "x", "type": "http"})).is_none());
        assert!(probe_spec_from_entry(&serde_json::json!({"name": "x", "type": "carrier-pigeon"})).is_none());
    }

    /// Real subprocess, real stdio framing, real `initialize`/`tools/
    /// list` JSON-RPC -- a tiny hand-rolled shell MCP responder standing
    /// in for a real server (same "real process, not a mock" bar
    /// `oauth.rs`'s own tests hold themselves to).
    #[tokio::test]
    async fn probe_stdio_returns_real_tools_from_a_real_subprocess() {
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("stub_mcp_server.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"stub","version":"0.0.0"}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"tools/list"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echoes input"},{"name":"ping"}]}}\n' "$id"
  fi
  # notifications (no "id") get no reply, matching a well-behaved server.
done
"#,
        )
        .expect("write stub MCP server script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x");
        }

        let tools = probe_stdio(
            StdioSpec {
                command: "sh".to_string(),
                args: vec![script_path.to_string_lossy().into_owned()],
                env: HashMap::new(),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("probe_stdio should succeed against a real well-behaved server");

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("Echoes input"));
        assert_eq!(tools[1].name, "ping");
        assert_eq!(tools[1].description, None);
    }

    #[tokio::test]
    async fn probe_stdio_surfaces_a_real_rpc_error() {
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("erroring_mcp_server.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"tools/list"'; then
    printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"tools/list not supported"}}\n' "$id"
  fi
done
"#,
        )
        .expect("write erroring MCP server script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x");
        }

        let result = probe_stdio(
            StdioSpec {
                command: "sh".to_string(),
                args: vec![script_path.to_string_lossy().into_owned()],
                env: HashMap::new(),
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(
            result,
            Err(McpClientError::RpcError { method: "tools/list", .. })
        ));
    }

    #[tokio::test]
    async fn probe_stdio_times_out_against_a_hung_process() {
        // `sleep` never writes anything to stdout -- a deliberately hung
        // "server" that outlives the probe's own timeout.
        let result = probe_stdio(
            StdioSpec {
                command: "sleep".to_string(),
                args: vec!["10".to_string()],
                env: HashMap::new(),
            },
            Duration::from_millis(200),
        )
        .await;
        assert!(matches!(result, Err(McpClientError::Timeout("tools/list"))));
    }

    /// Real HTTP server (`tokio::net::TcpListener`, same hand-rolled-
    /// response style `oauth.rs`'s own `start_loopback_listener` test
    /// uses), real JSON-RPC POSTs, real `Authorization` header check.
    #[tokio::test]
    async fn probe_http_returns_real_tools_from_a_real_http_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub http mcp server");
        let port = listener.local_addr().expect("local_addr").port();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let has_auth_header = request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-token");
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    let body: serde_json::Value =
                        serde_json::from_str(body_str).unwrap_or(serde_json::Value::Null);
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

                    let resp_body = if !has_auth_header {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "missing bearer token"}})
                    } else if method == "initialize" {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2024-11-05", "capabilities": {}}})
                    } else if method == "tools/list" {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "read_file", "description": "Reads a file"}]}})
                    } else {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}})
                    };
                    let body_bytes = serde_json::to_vec(&resp_body).unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body_bytes.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&body_bytes).await;
                });
            }
        });

        let client = reqwest::Client::new();
        let tools = probe_http(
            &client,
            &HttpSpec {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer test-token".to_string(),
                )]),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("probe_http should succeed with the right auth header");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[tokio::test]
    async fn probe_http_surfaces_auth_failure_without_the_header() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub http mcp server");
        let port = listener.local_addr().expect("local_addr").port();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    let body: serde_json::Value =
                        serde_json::from_str(body_str).unwrap_or(serde_json::Value::Null);
                    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));
                    let resp_body = serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "missing bearer token"}});
                    let body_bytes = serde_json::to_vec(&resp_body).unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body_bytes.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&body_bytes).await;
                });
            }
        });

        let client = reqwest::Client::new();
        let result = probe_http(
            &client,
            &HttpSpec {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: HashMap::new(),
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(
            result,
            Err(McpClientError::RpcError { method: "initialize", .. })
        ));
    }

    #[test]
    fn extract_sse_json_data_unwraps_a_single_event() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let value = extract_sse_json_data(body).expect("should extract the data payload");
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["tools"], serde_json::json!([]));
    }

    #[test]
    fn extract_sse_json_data_joins_multi_line_data_frames() {
        // SSE allows a single event's payload to be split across several
        // `data:` lines, joined with `\n` before parsing.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"tools\":[]}}\n\n";
        let value = extract_sse_json_data(body).expect("should join and parse multi-line data");
        assert_eq!(value["id"], 1);
    }

    #[test]
    fn extract_sse_json_data_returns_none_for_non_sse_bodies() {
        assert!(extract_sse_json_data("<html>not json, not sse</html>").is_none());
        assert!(extract_sse_json_data("").is_none());
    }

    /// A real-world MCP HTTP server that ignores the client's `Accept:
    /// application/json` request and always answers with `Content-Type:
    /// text/event-stream`, wrapping the JSON-RPC response in a `data:`
    /// frame -- the case this fix's `extract_sse_json_data` fallback
    /// exists for. Before this fix, `probe_http` would fail every such
    /// server with reqwest's opaque "error decoding response body".
    #[tokio::test]
    async fn probe_http_succeeds_against_a_real_server_that_answers_with_sse() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub http mcp server");
        let port = listener.local_addr().expect("local_addr").port();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    let body: serde_json::Value =
                        serde_json::from_str(body_str).unwrap_or(serde_json::Value::Null);
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));

                    let resp_body = if method == "initialize" {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2024-11-05", "capabilities": {}}})
                    } else if method == "tools/list" {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "read_file", "description": "Reads a file"}]}})
                    } else {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}})
                    };
                    let sse_body = format!("event: message\ndata: {}\n\n", resp_body);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        sse_body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(sse_body.as_bytes()).await;
                });
            }
        });

        let client = reqwest::Client::new();
        let tools = probe_http(
            &client,
            &HttpSpec {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: HashMap::new(),
            },
            Duration::from_secs(5),
        )
        .await
        .expect("probe_http should succeed against an SSE-only server");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    /// A server that returns a genuinely non-JSON-RPC body (e.g. a
    /// reverse-proxy error page) must still fail, but with a message
    /// carrying the HTTP status and a body snippet -- not reqwest's
    /// opaque "error decoding response body" that gives the user no way
    /// to tell what actually went wrong.
    #[tokio::test]
    async fn probe_http_surfaces_a_clear_error_for_a_truly_malformed_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub http mcp server");
        let port = listener.local_addr().expect("local_addr").port();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let body = "<html><body>502 Bad Gateway</body></html>";
                    let response = format!(
                        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        let client = reqwest::Client::new();
        let result = probe_http(
            &client,
            &HttpSpec {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: HashMap::new(),
            },
            Duration::from_secs(5),
        )
        .await;

        match result {
            Err(McpClientError::MalformedHttpResponse { method: "initialize", detail }) => {
                assert!(detail.contains("502"), "expected status in detail: {detail}");
                assert!(
                    detail.contains("Bad Gateway"),
                    "expected body snippet in detail: {detail}"
                );
            }
            other => panic!("expected a clear MalformedHttpResponse error, got {other:?}"),
        }
    }
}
