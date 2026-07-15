//! Bridges `rui-acp-client`'s async, per-thread ACP connections into
//! `panel-rust`'s single-OS-thread Slint world.
//!
//! Threading model (see `lib.rs` module docs): Slint/Qt must stay on one
//! OS thread. This module owns a background multi-thread tokio runtime
//! whose worker threads run entirely on their own, and *never* touch
//! Slint state directly. The only channel back to the UI thread is
//! `Mutex<VecDeque<BridgeEvent>>`, drained by [`AgentBridge::poll`] --
//! called periodically from a Qt timer via `panel_rust_poll`.
//!
//! ## JSON persistence (jsonl cache) and live reload
//!
//! Backed by [`crate::jsonl_store::JsonlStore`] -- one `<thread_id>.jsonl`
//! file per thread under the cache dir resolved by
//! [`resolve_cache_dir`].
//!
//! - **Cold start (renders smoothly from disk):** each thread's history
//!   is seeded from its jsonl file *before* the live agent connection is
//!   even spawned (see the `new_with_agent_cmd_and_cache_dir` loop
//!   below), so the very first render (`panel_rust_create` ->
//!   `bridge.history(0)`) shows cached scrollback immediately, with zero
//!   dependency on a subprocess round trip having completed. (The
//!   subprocess handshake/`open_session` call itself *does* happen
//!   synchronously within `AgentBridge::new` -- see that constructor's
//!   comment for why: it closes a real race where an immediate
//!   follow-up `send_prompt` could otherwise silently be dropped. That
//!   blocking is bounded and one-time at panel-creation, and is
//!   independent of -- does not gate -- the cache-seeded render above.)
//! - **No conflict when json content varies:** the seeded messages are
//!   plain `Vec<ChatMessage>` appended in file order, whatever mix of
//!   `MessageKind`s they happen to contain -- there is no schema
//!   reconciliation step, so a cache file from a longer or differently
//!   shaped prior run loads exactly as written, and the UI thread only
//!   ever reads a fully-formed snapshot through the same
//!   `Mutex<Vec<ChatMessage>>` the live path appends to (never a
//!   torn/partial write -- see `ThreadSlot::history`).
//! - **Async live reload:** as the bound agent streams new messages in
//!   (on a background runtime thread), each is pushed onto that same
//!   `history` mutex *and* appended to the jsonl file, in that order.
//!   Because appends never truncate or reorder what's already there, a
//!   live message arriving after a cache-seeded render composes cleanly
//!   on top of it -- the UI thread (via `poll` + `history`) never
//!   observes a state that mixes half of one write with half of another.
//! - **Trailer refresh:** on each `AgentEvent::TurnEnded`, the trailer is
//!   rewritten (`JsonlStore::overwrite`, with the full in-memory history
//!   as of that turn boundary) so the cache file's metadata (session id,
//!   message count) reflects true state -- deliberately not on every
//!   streamed message chunk, to avoid rewriting the whole file on every
//!   token.
//! - **Not implemented (deliberate scope boundary):** the full
//!   `session/list`-diff resync sequence from
//!   `chat-panel-acp-rust-sdk.md` Decision 2. `rui-mock-agent` (the only
//!   agent available to test against in this repo) does not persist
//!   sessions server-side across process restarts, so treating a fresh
//!   agent connection as source-of-truth on cold start would erase the
//!   jsonl cache instead of protecting it. jsonl is source-of-truth for
//!   pre-restart scrollback; the live agent connection is source-of-truth
//!   for anything from this run forward. Revisit once a real ACP agent
//!   with durable server-side session storage exists to validate
//!   against.

use crate::jsonl_store::{JsonlStore, ThreadTrailer};
use crate::protocol_types::{
    AgentEvent, AgentRequestEvent, ChatMessage, ConfigOptionInfo, SessionModesEvent,
    TerminalOutputEvent,
};
use crate::gateway_actor::{spawn_acpx_thread_with_gateway, AcpxThreadHandle};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(thiserror::Error, Debug)]
pub enum BridgeError {
    #[error("failed to start background async runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("jsonl cache error: {0}")]
    Cache(#[source] crate::jsonl_store::CacheError),
    #[error("acpx gateway provisioning failed: {0}")]
    Gateway(String),
}

/// One agent-bridge event, tagged with which UI thread index it belongs
/// to. `panel-rust`'s `PanelSingleton::apply_bridge_events` matches on
/// `event` for thread-status transitions and, for `Message`, re-reads
/// `AgentBridge::history` rather than trusting text carried here --
/// single source of truth is the mutex-guarded history, not the event.
pub struct BridgeEvent {
    pub thread_index: usize,
    pub event: AgentEvent,
}

/// One UI thread's state: its live agent handle, its jsonl-backed
/// scrollback (seeded at cold start, appended to live), and the ACP
/// session id once `open_session` resolves (used to fill the trailer).
struct ThreadSlot {
    thread_id: String,
    handle: Arc<AcpxThreadHandle>,
    history: Mutex<Vec<ChatMessage>>,
    acp_session_id: Mutex<Option<String>>,
    /// Live interactive requests (`session/request_permission`,
    /// `fs/read_text_file`, `fs/write_text_file`, `terminal/create`)
    /// awaiting a UI decision -- populated by
    /// `AgentEvent::PermissionRequest` in the forwarder loops below,
    /// drained by [`AgentBridge::respond_to_request`] once the user
    /// (or a future auto-decision path) answers. In practice never
    /// holds more than one entry at a time -- a well-behaved backend's
    /// own `session/prompt` call blocks on the relay's reply before
    /// sending a second such request -- but a `Vec` rather than an
    /// `Option` costs nothing and doesn't assume that invariant holds
    /// for every possible backend.
    pending_requests: Mutex<Vec<AgentRequestEvent>>,
    /// Latest live output snapshot per terminal id, keyed by
    /// `terminal_id` -- populated from `AgentEvent::TerminalOutput`
    /// (the gateway's `acpx/terminal_output` push, see
    /// `acpx_core::router::spawn_terminal_output_stream`'s doc comment).
   /// Always the current whole-buffer snapshot, never appended-to --
   /// matches that event's own "replace, don't append" contract.
   terminal_buffers: Mutex<HashMap<String, TerminalBuffer>>,
    /// Insertion-ordered list of every terminal id ever seen on this
    /// thread (first-seen order) -- `HashMap` iteration order is
    /// unspecified, but the UI needs a stable order to render terminal
    /// cards in (and to pick "the active/most-recent one" without
    /// depending on hash iteration). Appended to exactly once per new
    /// terminal id, in [`store_terminal_output`].
    terminal_order: Mutex<Vec<String>>,
    /// Most recently advertised `modes`/`configOptions` for this thread
    /// -- see [`AgentEvent::SessionModes`]/[`AgentEvent::
    /// CurrentModeChanged`]/[`AgentEvent::ConfigOptions`]'s doc
    /// comments. `None`/empty means the backend hasn't advertised any
    /// (either it genuinely has none, or `session/new`/`session/load`
    /// hasn't resolved yet) -- the settings-sheet mode/config selector
    /// (Coverage Matrix's `session/set_mode`, `session/set_config_
    /// option` row) is capability-gated on this being non-empty, not
    /// shown as a dead/always-present control.
    session_modes: Mutex<Option<SessionModesEvent>>,
    config_options: Mutex<Vec<ConfigOptionInfo>>,
}

/// One terminal's current known state, as last observed via
/// `AgentEvent::TerminalOutput`. See [`ThreadSlot::terminal_buffers`].
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalBuffer {
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<(Option<i32>, Option<i32>)>,
}

/// Owns the background runtime, the per-thread agent connections, the
/// jsonl cache, and the event queue the UI thread drains via `poll`.
pub struct AgentBridge {
    runtime: tokio::runtime::Runtime,
    slots: Vec<Arc<ThreadSlot>>,
    events: Arc<Mutex<VecDeque<BridgeEvent>>>,
    gateway_urls: std::collections::HashMap<String, String>,
    // Phase 2 (chat-panel-production-ui/execution-plan.md): "one shared
    // acpx_client::Gateway held by AgentBridge" -- one real connection
    // per distinct gateway URL (== per provider, today), reused by
    // every thread bound to that provider instead of each thread
    // opening its own. Keyed by base_url (not provider) so a future
    // multi-URL-per-provider scenario stays representable without a
    // schema change, even though provider and URL are 1:1 today.
    gateways: std::collections::HashMap<String, Arc<acpx_client::Gateway>>,
    #[allow(dead_code)] // kept alive for its Drop / for future direct use
    store: Option<JsonlStore>,
    // Client-local PTY terminals -- v1 keeps this to at most one per
    // thread (keyed by thread `idx`), matching the settings-sheet's own
    // "one bound choice per scope" simplicity; a future increment could
    // key by a client-generated terminal id instead to support more
    // than one per thread. Distinct from `ThreadSlot::terminal_buffers`
    // (agent-created, read-only, gateway-relayed) -- these are real
    // client-spawned shell processes (`local_terminal::LocalTerminal`),
    // never touch the gateway at all.
    // `RefCell`, not a plain field, so every accessor below can stay
    // `&self` -- matches every other per-thread read accessor in this
    // impl block (`history`/`active_terminals`/`terminal_buffer`/etc.),
    // which `PanelSingleton`'s own `&self` refresh methods
    // (`refresh_terminals_for` and friends) rely on being able to call
    // without needing `&mut self.bridge` threaded through.
    local_terminals: std::cell::RefCell<std::collections::HashMap<usize, crate::local_terminal::LocalTerminal>>,
}

/// A point-in-time read of a client-local terminal's VT100 screen state
/// (`AgentBridge::local_terminal_snapshot`) -- what `models::to_local_
/// terminal_item` turns into the Slint-facing `LocalTerminalItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTerminalSnapshot {
    pub screen_text: String,
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub has_exited: bool,
}

/// Turns a UI thread display name into a filesystem-safe, stable jsonl
/// cache key -- lowercased, non-alphanumerics collapsed to `-`. Stable
/// across runs as long as `THREAD_NAMES` (in `lib.rs`) doesn't change,
/// which is the v1 fixed-thread-list assumption documented there.
/// One `AgentEvent::TerminalOutput`'s worth of update, applied to
/// `slot`'s live terminal-buffer map -- shared by both forwarder loops
/// (initial-construction and `add_thread`) so the "replace this
/// terminal's snapshot" semantics stay in exactly one place.
/// `handle.open_session(cwd)` if `profile` is `None`, else
/// `handle.open_session_with_profile(cwd, profile)` -- one helper so
/// [`AgentBridge::add_thread_with_profile`]'s two call sites (fresh-open
/// and resume-failed-fallback) don't duplicate the branch.
async fn open_session_maybe_profiled(
    handle: &AcpxThreadHandle,
    cwd: PathBuf,
    profile: Option<&str>,
) -> Result<String, crate::gateway_actor::AcpxThreadError> {
    match profile {
        Some(profile) => handle.open_session_with_profile(cwd, profile).await,
        None => handle.open_session(cwd).await,
    }
}

fn store_terminal_output(slot: &ThreadSlot, ev: &TerminalOutputEvent) {
    let is_new = !slot
        .terminal_buffers
        .lock()
        .expect("terminal_buffers mutex poisoned")
        .contains_key(&ev.terminal_id);
    if is_new {
        slot.terminal_order
            .lock()
            .expect("terminal_order mutex poisoned")
            .push(ev.terminal_id.clone());
    }
    slot.terminal_buffers
        .lock()
        .expect("terminal_buffers mutex poisoned")
        .insert(
            ev.terminal_id.clone(),
            TerminalBuffer {
                output: ev.output.clone(),
                truncated: ev.truncated,
                exit_status: ev.exit_status,
            },
        );
}

/// Applies one [`AgentEvent::SessionModes`]/[`AgentEvent::
/// CurrentModeChanged`]/[`AgentEvent::ConfigOptions`] event to `slot`'s
/// own capability state -- shared by both forwarder loops, same role
/// [`store_terminal_output`] plays for terminal buffers.
fn store_capability_event(slot: &ThreadSlot, ev: &AgentEvent) {
    match ev {
        AgentEvent::SessionModes(modes) => {
            *slot.session_modes.lock().expect("session_modes mutex poisoned") =
                Some(modes.clone());
        }
        AgentEvent::CurrentModeChanged(mode_id) => {
            if let Some(modes) = slot
                .session_modes
                .lock()
                .expect("session_modes mutex poisoned")
                .as_mut()
            {
                modes.current_mode_id = mode_id.clone();
            }
        }
        AgentEvent::ConfigOptions(options) => {
            *slot
                .config_options
                .lock()
                .expect("config_options mutex poisoned") = options.clone();
        }
        _ => {}
    }
}

fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for ch in name.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Which acpx-gateway-backed provider a UI thread is bound to. v1's fixed
/// four-thread list (`THREAD_NAMES` in `lib.rs`) alternates codex/claude
/// by index, so both providers get real, concurrent, isolated coverage
/// rather than only ever exercising one -- the multi-provider
/// verification requirement from `chat-panel-acpx-gateway-integration.md`
/// Phase 3 bullet 5 applies to the *real* running panel, not only its
/// test suite.
pub fn provider_for_index(idx: usize) -> &'static str {
    if idx % 2 == 0 {
        "codex"
    } else {
        "claude"
    }
}

/// Resolves the dev-checkout `acpx-server` binary path: `RUI_ACPX_SERVER_BIN`
/// env override, else a path relative to this crate's own
/// `CARGO_MANIFEST_DIR`, matching the same convention
/// `resolve_agent_command`'s successor (`provision_gateway` below)
/// uses for the backend it spawns *inside* that gateway.
fn resolve_acpx_server_bin_from(
    override_bin: Option<&str>,
    current_exe: Option<&Path>,
    manifest_dir: &Path,
) -> PathBuf {
    if let Some(bin) = override_bin.filter(|bin| !bin.is_empty()) {
        return PathBuf::from(bin);
    }
    if let Some(parent) = current_exe.and_then(Path::parent) {
        for candidate in [
            parent.join("acpx-server"),
            parent.join("../libexec/acpx-server"),
        ] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    manifest_dir.join("../acpx/target/debug/acpx-server")
}

fn resolve_acpx_server_bin() -> PathBuf {
    resolve_acpx_server_bin_from(
        std::env::var("RUI_ACPX_SERVER_BIN").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolves the mock backend agent binary the locally-spawned gateway
/// should proxy to: `RUI_ACP_AGENT_CMD` env override (a real
/// ACP-compliant agent binary/command), else the dev-checkout
/// `rui-mock-agent` binary this crate itself builds (`src/bin/
/// mock_agent.rs`, ported directly from the former `rui-acp-client`
/// crate's own `[[bin]]` of the same name -- Phase 2, chat-panel-
/// production-ui/execution-plan.md) -- the acpx-gateway's own default
/// backend for dev/test.
fn resolve_backend_agent_command() -> String {
    if let Ok(cmd) = std::env::var("RUI_ACP_AGENT_CMD") {
        return cmd;
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug/rui-mock-agent")
        .to_string_lossy()
        .into_owned()
}

/// Real (not just "is the TCP port open") liveness probe: issues an
/// actual `session/list` JSON-RPC call over a hand-rolled HTTP/1.1
/// request (no async runtime available yet at this point in
/// construction, and pulling in `reqwest`'s blocking client just for a
/// one-shot startup probe isn't worth the extra compiled dependency) and
/// checks the response actually looks like a JSON-RPC envelope.
///
/// **Real bug this closes, found empirically, not assumed:** the naive
/// version of this check (a bare `TcpStream::connect` with no HTTP
/// request at all) was tried first and immediately produced a false
/// positive against this dev machine's own unrelated service already
/// listening on the fixed default port 8791 -- `panel-rust` happily
/// "reused" it as if it were the claude acpx-gateway, then every
/// `session/new` against it failed (`405 Method Not Allowed`, a
/// completely different HTTP server). A bare port-open check can never
/// distinguish "our gateway" from "any other service that happens to be
/// listening here" on a shared dev machine; an actual protocol-shaped
/// round trip can.
///
/// Single connect-and-probe attempt -- factored out from
/// [`probe_acpx_gateway`] so that function can retry a couple times
/// under real system load (see its own doc comment's "known limitation"
/// note) without duplicating this request-building logic.
fn probe_acpx_gateway_once(port: u16, expected_agent: Option<&str>) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1500)));
    let request = if let Some(expected_agent) = expected_agent {
        let _ = expected_agent;
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
    } else {
        let body = r#"{"jsonrpc":"2.0","id":0,"method":"session/list","params":{}}"#;
        format!(
            "POST /rpc HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let Ok(text) = String::from_utf8(response) else {
        return false;
    };
    let Some((headers, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let Some(status_line) = headers.lines().next() else {
        return false;
    };
    let status = status_line.split_whitespace().nth(1);
    if status != Some("200") {
        return false;
    }
    let Ok(envelope): Result<serde_json::Value, _> = serde_json::from_str(body) else {
        return false;
    };
    if envelope.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0")
        || envelope.get("error").is_some()
    {
        if expected_agent.is_none() {
            return false;
        }
    }
    if let Some(expected_agent) = expected_agent {
        envelope.get("status").and_then(|s| s.as_str()) == Some("ok")
            && envelope.get("agentId").and_then(|id| id.as_str()) == Some(expected_agent)
    } else {
        envelope
            .get("result")
            .and_then(|r| r.get("sessions"))
            .and_then(|s| s.as_array())
            .is_some()
    }
}

/// See [`probe_acpx_gateway_once`]. Retries up to 3 times (small,
/// fixed backoff) before concluding "not a real acpx-server" -- **known
/// limitation found empirically**: a single 200ms-connect/500ms-read
/// attempt produced a false negative during this crate's own headless
/// smoke test, spawning a redundant second gateway instead of reusing
/// an already-live one, when the host machine was under heavy
/// concurrent CPU load (Shotcut's own MLT filter-metadata loading
/// competing with unrelated build/test processes on the same box). The
/// redundant spawn was itself harmless (a second, independent, correctly
/// working gateway -- no crash, no cross-provider mixup), but it defeats
/// the "relaunch reattaches to the existing gateway" property this
/// function exists for. Retrying trades a little startup latency in the
/// already-rare "something is listening but isn't answering yet" case
/// for a much higher chance of correctly reusing a live gateway.
#[cfg(test)]
fn probe_acpx_gateway(port: u16) -> bool {
    probe_acpx_gateway_for_agent(port, None)
}

fn probe_acpx_gateway_for_agent(port: u16, expected_agent: Option<&str>) -> bool {
    for attempt in 0..3 {
        if probe_acpx_gateway_once(port, expected_agent) {
            return true;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    false
}

/// Binds an ephemeral TCP port synchronously, then immediately drops the
/// listener so `acpx-server` can bind the same port itself moments later
/// -- same "probe a free port, hand the number to the real process"
/// trick this workspace's own `rui-acpx-client`/`acpx-server` test suites
/// use, reused here so a colliding fixed default port (see
/// `probe_acpx_gateway`'s doc comment) never blocks startup.
fn reserve_port(port: u16) -> io::Result<File> {
    let path = std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock"));
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn reserve_ephemeral_port() -> Option<(u16, File)> {
    for _ in 0..32 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);
        if let Ok(lock) = reserve_port(port) {
            return Some((port, lock));
        }
    }
    None
}

/// Resolves and, if necessary, spawns `provider`'s acpx gateway,
/// returning the base URL to actually dial:
///
/// 1. `RUI_ACPX_<PROVIDER>_URL` env override (real-deployment path -- an
///    already-running acpx-server this process should just dial,
///    trusted as-is with no liveness probe, matching
///    `RUI_ACP_AGENT_CMD`'s established override-precedence convention).
/// 2. Else, a fixed per-provider loopback default port (8790 codex /
///    8791 claude) is probed with [`probe_acpx_gateway`] -- if a real
///    acpx-server is already answering there (an operator-started one,
///    *or this same panel process's own gateway surviving a prior
///    thread's earlier call in this same construction loop, or -- the
///    concrete "closing and relaunching reattaches" case -- a gateway
///    left running by a now-closed prior panel process*), it's reused
///    unchanged.
/// 3. Else, spawns a fresh `acpx-server` child -- on the fixed default
///    port if nothing at all is listening there yet, or on a freshly
///    probed ephemeral port if something *is* listening but didn't pass
///    the acpx-shaped check (an unrelated service already owns the
///    default port on this machine).
///
/// Spawned with `RUI_MOCK_AGENT_PERSONA=provider` so its backend tags
/// replies for the multi-provider isolation checks.
///
/// **Deliberately not tied to this process's lifetime**: the spawned
/// `acpx-server` (and, transitively, its own backend subprocess) is placed
/// in a separate process group, so it is reparented to init and keeps
/// running if this process (the panel / the whole host application) is
/// killed by PID rather than by process-group signal. This is exactly the
/// "window close does not imply session close" contract: the gateway
/// process, and therefore every session it holds open, survives the panel
/// window/process going away. See
/// `gen/plans/chat-panel/chat-panel-acpx-gateway-integration.md` Phase 3
/// bullet 8's verification requirement -- `Command::spawn` here with no
/// special detachment call is the entire mechanism, not an oversight.
fn provision_gateway(provider: &str, cache_dir: Option<&PathBuf>) -> Result<String, String> {
    let env_key = format!("RUI_ACPX_{}_URL", provider.to_uppercase());
    if let Ok(url) = std::env::var(&env_key) {
        return Ok(url);
    }

    let default_port: u16 = if provider == "codex" { 8790 } else { 8791 };
    if probe_acpx_gateway_for_agent(default_port, Some(provider)) {
        return Ok(format!("http://127.0.0.1:{default_port}"));
    }

    // Nothing acpx-shaped answering the default port -- decide which
    // port to actually spawn on. If the default port is genuinely free
    // (no TCP listener at all, not just "didn't answer our probe"),
    // spawn there directly (keeps the common case's URL predictable);
    // otherwise it's occupied by some unrelated service, so probe for a
    // real free ephemeral port instead of fighting over the default one.
    let (port, lock) = if std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], default_port)),
        std::time::Duration::from_millis(100),
    )
    .is_err()
    {
        match reserve_port(default_port) {
            Ok(lock) => (default_port, lock),
            Err(_) => reserve_ephemeral_port()
                .ok_or_else(|| "could not reserve a loopback port".to_string())?,
        }
    } else {
        reserve_ephemeral_port().ok_or_else(|| "could not reserve a loopback port".to_string())?
    };

    spawn_gateway_process(provider, port, lock, cache_dir)?;
    Ok(format!("http://127.0.0.1:{port}"))
}

/// The actual `Command::spawn` -- split from [`provision_gateway`] so the
/// port-selection policy above stays readable. See that function's doc
/// comment for the full reuse/fallback contract this is one step of.
fn spawn_gateway_process(
    provider: &str,
    port: u16,
    lock: File,
    cache_dir: Option<&PathBuf>,
) -> Result<(), String> {
    let mut cmd = std::process::Command::new(resolve_acpx_server_bin());
    cmd.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
        .env("ACPX_BACKEND_CMD", resolve_backend_agent_command())
        .env("ACPX_DEFAULT_AGENT_ID", provider)
        .env("RUI_MOCK_AGENT_PERSONA", provider)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Persist session/transcript metadata to sqlite so a `session/load`
    // after this whole panel process (and even this gateway process, if
    // it's ever restarted by an operator) relaunches can still rehydrate
    // -- the concrete mechanism behind "closing and relaunching the app
    // auto-reloads session instances ... resuming continues the session
    // from acpx-server" (Phase 3 bullet 6). Placed alongside the jsonl
    // cache dir when one is configured, else a per-provider tempdir so a
    // no-persistence dev run still gets a working (if ephemeral) db
    // rather than silently disabling rehydration.
    let db_path = match cache_dir {
        Some(dir) => dir.join(format!("acpx-{provider}.sqlite3")),
        None => std::env::temp_dir().join(format!(
            "rui-acpx-{provider}-{}.sqlite3",
            std::process::id()
        )),
    };
    cmd.env("ACPX_DB_PATH", &db_path);
    let mut child = cmd.spawn().map_err(|e| {
        let _ =
            std::fs::remove_file(std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock")));
        format!("failed to spawn acpx-server for {provider} on port {port}: {e}")
    })?;
    for _ in 0..50 {
        if probe_acpx_gateway_for_agent(port, Some(provider)) {
            std::thread::spawn(move || {
                let mut child = child;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(500)),
                    }
                }
                drop(lock);
                let _ = std::fs::remove_file(
                    std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock")),
                );
            });
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("failed checking acpx-server startup: {e}"))?
        {
            let _ = std::fs::remove_file(
                std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock")),
            );
            return Err(format!(
                "acpx-server exited during startup for {provider} on port {port}: {status}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock")));
    Err(format!(
        "acpx-server did not become ready for {provider} on port {port}"
    ))
}

/// Resolves the jsonl cache directory: explicit override first, then the
/// platform state directory, with a dev-checkout fallback for local builds.
fn resolve_cache_dir_from(
    override_dir: Option<&str>,
    xdg_state_home: Option<&str>,
    local_app_data: Option<&str>,
    home: Option<&str>,
    manifest_dir: &Path,
) -> PathBuf {
    if let Some(dir) = override_dir.filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = xdg_state_home.filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("shotcut/rui-thread-cache");
    }
    if let Some(dir) = local_app_data.filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("Shotcut/rui-thread-cache");
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        return PathBuf::from(home).join(".local/state/shotcut/rui-thread-cache");
    }
    manifest_dir.join("../.rui-thread-cache")
}

pub fn resolve_cache_dir() -> PathBuf {
    resolve_cache_dir_from(
        std::env::var("RUI_ACP_CACHE_DIR").ok().as_deref(),
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("LOCALAPPDATA").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Opaque staleness token -- not a real RFC3339 timestamp (no chrono
/// dependency pulled in just for this), only ever compared for equality
/// against itself by a future resync check, per the module doc's
/// documented scope boundary.
fn now_token() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// The `cwd` argument ACP's `session/new` wants -- this crate has no
/// concept of a project directory of its own (the chat panel isn't
/// editing files directly), so the process's own working directory is
/// as reasonable a default as any, with `.` as a last-resort fallback if
/// that's somehow unavailable.
fn cwd_for_session() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn persist_thread_snapshot(store: Option<&JsonlStore>, slot: &ThreadSlot, updated_at: String) {
    let Some(store) = store else {
        return;
    };
    let history = slot.history.lock().expect("history mutex poisoned").clone();
    let session_id = slot
        .acp_session_id
        .lock()
        .expect("acp_session_id mutex poisoned")
        .clone()
        .unwrap_or_default();
    let trailer = ThreadTrailer {
        acp_session_id: session_id,
        title: Some(slot.thread_id.clone()),
        updated_at: Some(updated_at),
        message_count: history.len(),
    };
    if let Err(e) = store.overwrite(&slot.thread_id, &history, &trailer) {
        eprintln!(
            "panel-rust: jsonl trailer overwrite failed for {}: {e}",
            slot.thread_id
        );
    }
}

fn replay_matches_cached_position(
    history: &[ChatMessage],
    cached_index: &mut usize,
    message: &ChatMessage,
) -> bool {
    // A gateway replay contains backend-originated updates, while the
    // local jsonl transcript also contains the user's prompt. Match the
    // replay as an ordered subsequence rather than requiring both streams
    // to have identical event boundaries. Advancing only forward preserves
    // repeated identical messages at distinct transcript positions.
    if let Some(relative) = history[*cached_index..]
        .iter()
        .position(|cached| cached == message)
    {
        *cached_index += relative + 1;
        true
    } else {
        false
    }
}

impl AgentBridge {
    /// Production constructor: every thread's acpx gateway URL resolved
    /// (env-override-or-local-autospawn, see [`provision_gateway`]) +
    /// real (dev-checkout) cache dir.
    pub fn new(thread_names: &[&str]) -> Result<Self, BridgeError> {
        let cache_dir = resolve_cache_dir();
        let cache_dir_for_resolver = cache_dir.clone();
        Self::new_with_gateway_resolver_and_cache_dir(
            thread_names,
            move |provider| {
                provision_gateway(provider, Some(&cache_dir_for_resolver))
                    .map_err(BridgeError::Gateway)
            },
            Some(cache_dir),
        )
    }

    /// Test/override constructor: every thread dials the single given
    /// gateway base URL (both "codex" and "claude" providers alike --
    /// tests that specifically need two distinct gateways use
    /// [`Self::new_with_gateway_resolver_and_cache_dir`] directly with a
    /// resolver closure of their own), no jsonl persistence (in-memory
    /// history only) -- what the existing Rust test suite used before
    /// this module had a cache dir parameter at all, kept working with
    /// the same call shape (one URL in, not an agent command) after the
    /// acpx cutover.
    pub fn new_with_gateway_url(
        thread_names: &[&str],
        base_url: String,
    ) -> Result<Self, BridgeError> {
        Self::new_with_gateway_resolver_and_cache_dir(
            thread_names,
            move |_provider| Ok(base_url.clone()),
            None,
        )
    }

    /// The real constructor both of the above delegate to: a per-provider
    /// gateway-URL resolver closure (`provider_for_index`'s output ->
    /// already-provisioned `base_url`, matching [`provision_gateway`]'s
    /// own return shape -- callers that want auto-spawn-if-unreachable
    /// pass `provision_gateway` itself, as [`Self::new`] does; callers
    /// that just want a fixed URL, like [`Self::new_with_gateway_url`],
    /// pass a closure that ignores `provider` entirely) and, optionally, a
    /// jsonl cache directory. `None` disables persistence entirely (pure
    /// in-memory history, matching pre-persistence behavior) rather than
    /// silently picking a directory the caller didn't ask for.
    pub fn new_with_gateway_resolver_and_cache_dir(
        thread_names: &[&str],
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError>,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(BridgeError::Runtime)?;

        let store = match &cache_dir {
            Some(dir) => Some(JsonlStore::open(dir.clone()).map_err(BridgeError::Cache)?),
            None => None,
        };
        let events: Arc<Mutex<VecDeque<BridgeEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut slots = Vec::with_capacity(thread_names.len());

        // Resolve (and, for the production resolver, auto-spawn if
        // needed) every distinct provider's gateway once, up front --
        // not inside the per-thread loop below, so two threads sharing a
        // provider (the normal case: v1's four static threads alternate
        // codex/claude, two threads per provider) never race each other
        // spawning a duplicate `acpx-server`. `provision_gateway` is
        // also independently idempotent (it probes reachability before
        // ever spawning), so this cache is a belt-and-suspenders
        // ordering guarantee -- and an efficiency win, since it means
        // `resolve_gateway` (whose production implementation does a
        // real, mildly expensive TCP probe) only runs once per distinct
        // provider rather than once per thread.
        let mut resolved_urls: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (idx, _name) in thread_names.iter().enumerate() {
            let provider = provider_for_index(idx).to_string();
            if !resolved_urls.contains_key(&provider) {
                resolved_urls.insert(provider.clone(), resolve_gateway(&provider)?);
            }
        }

        // One real `Gateway` connection per distinct URL, connected once
        // here (not inside the per-thread loop below) -- see `gateways`
        // field's own doc comment. `runtime.block_on` is safe here: the
        // runtime has no other work queued yet (no threads have been
        // spawned), so this cannot deadlock against anything this
        // constructor itself is waiting on.
        let mut gateways: std::collections::HashMap<String, Arc<acpx_client::Gateway>> =
            std::collections::HashMap::new();
        for url in resolved_urls.values() {
            if !gateways.contains_key(url) {
                gateways.insert(
                    url.clone(),
                    Arc::new(runtime.block_on(acpx_client::Gateway::connect(url.clone()))),
                );
            }
        }

        // `spawn_acpx_thread_with_gateway` calls the free-function `tokio::spawn` internally,
        // which needs an active runtime context on this (calling) thread --
        // `enter()` provides that for the duration of this loop. The tasks
        // it schedules then run on the runtime's own worker threads for the
        // rest of the process's life, well past this guard's drop.
        let _guard = runtime.enter();
        for (idx, name) in thread_names.iter().enumerate() {
            let thread_id = slug(name);

            // Cold-start seed: read whatever this thread's jsonl file
            // already holds -- of any prior shape/length -- *before*
            // spawning the live connection below, so `history(idx)` is
            // immediately populated for the first render.
            //
            // A load failure here (missing/renamed field, truncated
            // write, hand-edited file, whatever) is deliberately *not*
            // propagated as a fatal `BridgeError` -- doing so would take
            // down every other thread's live agent connection too, just
            // because one thread's cache file happened to be malformed.
            // "No conflict in UI views when content varies in json" cuts
            // both ways: a cache file this crate itself never wrote (or
            // wrote in some earlier, incompatible shape) must not be
            // able to disable the whole chat panel -- it degrades to an
            // empty scrollback for *that thread only*, same as any other
            // cache miss.
            let (seeded, cached_session_id) = match &store {
                Some(s) => match s.load(&thread_id) {
                    Ok(cached) => {
                        let session_id = cached
                            .trailer
                            .as_ref()
                            .map(|trailer| trailer.acp_session_id.trim())
                            .filter(|session_id| !session_id.is_empty())
                            .map(str::to_owned);
                        (cached.messages, session_id)
                    }
                    Err(e) => {
                        eprintln!(
                            "panel-rust: jsonl cache load failed for thread {thread_id:?} ({e}); starting this thread with empty history rather than failing the whole bridge"
                        );
                        (Vec::new(), None)
                    }
                },
                None => (Vec::new(), None),
            };

            let provider = provider_for_index(idx);
            let base_url = resolved_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
            let gateway = gateways.get(&base_url).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway connection missing for {base_url}"))
            })?;
            let mut handle = spawn_acpx_thread_with_gateway(gateway);
            let mut events_rx = handle.take_events();
            let handle = Arc::new(handle);

            let slot = Arc::new(ThreadSlot {
                thread_id: thread_id.clone(),
                handle: handle.clone(),
                history: Mutex::new(seeded),
                acp_session_id: Mutex::new(None),
                pending_requests: Mutex::new(Vec::new()),
                terminal_buffers: Mutex::new(HashMap::new()),
                terminal_order: Mutex::new(Vec::new()),
                session_modes: Mutex::new(None),
                config_options: Mutex::new(Vec::new()),
            });
            slots.push(slot.clone());

            let events_out = events.clone();
            let store_for_task = store.clone();
            let slot_for_task = slot;
            let handle_for_task = handle;

            // Open this thread's ACP session *synchronously* (from this
            // constructor's point of view -- via `block_on` on the
            // background runtime, not on the caller's own async task).
            // This closes a real race that otherwise existed here: if
            // `AgentBridge::new` returned immediately and opened the
            // session purely in the background, a caller that called
            // `send_prompt` right away (exactly what a "renders
            // smoothly, then the user immediately sends a follow-up"
            // flow looks like) could have its `SendPrompt` command
            // reach the actor *before* `OpenSession` had been
            // processed, hitting `NoActiveSession` and silently never
            // producing a `TurnEnded` -- observed directly as a flaky
            // test failure in this module before this fix. The cost is
            // bounded, one-time blocking during panel creation (one
            // local subprocess handshake per thread), which is an
            // acceptable trade for "a message sent right after startup
            // must actually go through."
            let cwd = cwd_for_session();
            let session_result = if let Some(session_id) = cached_session_id.clone() {
                match runtime
                    .block_on(handle_for_task.resume_session(session_id.clone(), cwd.clone()))
                {
                    Ok(()) => Ok(session_id),
                    Err(resume_error) => {
                        eprintln!(
                            "panel-rust: cached acpx session resume failed for thread {thread_id:?} ({resume_error}); opening a fresh session"
                        );
                        runtime.block_on(handle_for_task.open_session(cwd))
                    }
                }
            } else {
                runtime.block_on(handle_for_task.open_session(cwd))
            };
            match session_result {
                Ok(session_id) => {
                    *slot_for_task
                        .acp_session_id
                        .lock()
                        .expect("acp_session_id mutex poisoned") = Some(session_id);
                    // Persist the gateway id immediately. A window can close
                    // before the first turn reaches TurnEnded; the next
                    // launch must still be able to resume this session.
                    persist_thread_snapshot(store_for_task.as_ref(), &slot_for_task, now_token());

                    // `session/load` can replay the backend's transcript.
                    // The jsonl cache already rendered that transcript, so
                    // consume the buffered replay in sequence order. This
                    // avoids duplicating the cached prefix while preserving
                    // legitimate repeated messages.
                    if cached_session_id.is_some() {
                        let mut cached_index = 0usize;
                        while let Ok(ev) = events_rx.try_recv() {
                            if let AgentEvent::Message(message) = &ev {
                                let mut history = slot_for_task
                                    .history
                                    .lock()
                                    .expect("history mutex poisoned");
                                if !replay_matches_cached_position(
                                    &history,
                                    &mut cached_index,
                                    message,
                                ) {
                                    history.push(message.clone());
                                    if let Some(store) = &store_for_task {
                                        if let Err(e) =
                                            store.append(&slot_for_task.thread_id, message)
                                        {
                                            eprintln!(
                                                "panel-rust: jsonl append failed for {}: {e}",
                                                slot_for_task.thread_id
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    events
                        .lock()
                        .expect("event queue mutex poisoned")
                        .push_back(BridgeEvent {
                            thread_index: idx,
                            event: AgentEvent::Error(format!("open_session failed: {e}")),
                        });
                }
            }

            runtime.spawn(async move {
                while let Some(ev) = events_rx.recv().await {
                    match &ev {
                        AgentEvent::Message(msg) => {
                            slot_for_task
                                .history
                                .lock()
                                .expect("history mutex poisoned")
                                .push(msg.clone());
                            if let Some(store) = &store_for_task {
                                if let Err(e) = store.append(&slot_for_task.thread_id, msg) {
                                    eprintln!(
                                        "panel-rust: jsonl append failed for {}: {e}",
                                        slot_for_task.thread_id
                                    );
                                }
                            }
                        }
                        AgentEvent::TurnEnded(_) => {
                            persist_thread_snapshot(
                                store_for_task.as_ref(),
                                &slot_for_task,
                                now_token(),
                            );
                        }
                        AgentEvent::Error(_) => {}
                        AgentEvent::PermissionRequest(req) => {
                            slot_for_task
                                .pending_requests
                                .lock()
                                .expect("pending_requests mutex poisoned")
                                .push(req.clone());
                        }
                        AgentEvent::TerminalOutput(term_ev) => {
                            store_terminal_output(&slot_for_task, term_ev);
                        }
                        AgentEvent::SessionModes(_)
                        | AgentEvent::CurrentModeChanged(_)
                        | AgentEvent::ConfigOptions(_) => {
                            store_capability_event(&slot_for_task, &ev);
                        }
                    }
                    events_out
                        .lock()
                        .expect("event queue mutex poisoned")
                        .push_back(BridgeEvent {
                            thread_index: idx,
                            event: ev,
                        });
                }
            });
        }
        drop(_guard);

        Ok(AgentBridge {
            runtime,
            slots,
            events,
            gateway_urls: resolved_urls,
            gateways,
            store,
            local_terminals: std::cell::RefCell::new(std::collections::HashMap::new()),
        })
    }

   /// Adds one open thread using the already-provisioned provider gateway.
   /// The session is opened synchronously before the new slot is exposed to
   /// the UI, so selecting the row and sending immediately cannot race
   /// `session/new`.
   pub fn add_thread(&mut self, name: &str) -> Result<usize, BridgeError> {
        self.add_thread_with_profile(name, None)
    }

    /// Same as [`Self::add_thread`], but selects a named ACPX profile for
    /// the new thread's `session/new` call (`_acpx.profile`, via
    /// [`AcpxThreadHandle::open_session_with_profile`]) -- the live hook
    /// for the settings sheet's profile picker: a profile with
    /// `allow_terminal_access`/`allow_fs_access` enabled only actually
    /// unlocks those interactive request cards for threads opened with
    /// it selected, not retroactively for already-open threads (ACPX has
    /// no `session/set_profile`; changing a live session's profile means
    /// opening a new one). `None` behaves identically to `add_thread`
    /// (native/unmanaged mode, no `_acpx.profile` sent at all).
    pub fn add_thread_with_profile(
        &mut self,
        name: &str,
        profile: Option<&str>,
    ) -> Result<usize, BridgeError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(BridgeError::Gateway("thread name cannot be empty".into()));
        }
        let thread_id = slug(name);
        if self.slots.iter().any(|slot| slot.thread_id == thread_id) {
            return Err(BridgeError::Gateway(format!(
                "thread already exists: {name}"
            )));
        }

        let idx = self.slots.len();
        let provider = provider_for_index(idx);
        let base_url =
            self.gateway_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
        let gateway = self.gateways.get(&base_url).cloned().ok_or_else(|| {
            BridgeError::Gateway(format!("gateway connection missing for {base_url}"))
        })?;
        let (seeded, cached_session_id) = match &self.store {
            Some(store) => match store.load(&thread_id) {
                Ok(cached) => (
                    cached.messages,
                    cached
                        .trailer
                        .as_ref()
                        .map(|trailer| trailer.acp_session_id.trim())
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned),
                ),
                Err(error) => {
                    eprintln!("panel-rust: new thread cache load failed for {thread_id}: {error}");
                    (Vec::new(), None)
                }
            },
            None => (Vec::new(), None),
        };

        let mut handle = {
            let _guard = self.runtime.enter();
            spawn_acpx_thread_with_gateway(gateway)
        };
        let mut events_rx = handle.take_events();
        let handle = Arc::new(handle);
        let slot = Arc::new(ThreadSlot {
            thread_id: thread_id.clone(),
            handle: handle.clone(),
            history: Mutex::new(seeded),
            acp_session_id: Mutex::new(None),
            pending_requests: Mutex::new(Vec::new()),
            terminal_buffers: Mutex::new(HashMap::new()),
            terminal_order: Mutex::new(Vec::new()),
            session_modes: Mutex::new(None),
            config_options: Mutex::new(Vec::new()),
        });
       let cwd = cwd_for_session();
       let session_id = if let Some(session_id) = cached_session_id.clone() {
           match self
               .runtime
               .block_on(handle.resume_session(session_id.clone(), cwd.clone()))
           {
               Ok(()) => session_id,
                Err(_) => self
                    .runtime
                    .block_on(open_session_maybe_profiled(&handle, cwd, profile))
                    .map_err(|error| BridgeError::Gateway(error.to_string()))?,
            }
        } else {
            self.runtime
                .block_on(open_session_maybe_profiled(&handle, cwd, profile))
                .map_err(|error| BridgeError::Gateway(error.to_string()))?
        };
        *slot
            .acp_session_id
            .lock()
            .expect("acp_session_id mutex poisoned") = Some(session_id);
        persist_thread_snapshot(self.store.as_ref(), &slot, now_token());

        if cached_session_id.is_some() {
            let mut cached_index = 0usize;
            while let Ok(event) = events_rx.try_recv() {
                if let AgentEvent::Message(message) = event {
                    let mut history = slot.history.lock().expect("history mutex poisoned");
                    if !replay_matches_cached_position(&history, &mut cached_index, &message) {
                        history.push(message.clone());
                        if let Some(store) = &self.store {
                            let _ = store.append(&slot.thread_id, &message);
                        }
                    }
                }
            }
        }

        let events_out = self.events.clone();
        let store_for_task = self.store.clone();
        let slot_for_task = slot.clone();
        self.runtime.spawn(async move {
            while let Some(event) = events_rx.recv().await {
                match &event {
                    AgentEvent::Message(message) => {
                        slot_for_task
                            .history
                            .lock()
                            .expect("history mutex poisoned")
                            .push(message.clone());
                        if let Some(store) = &store_for_task {
                            let _ = store.append(&slot_for_task.thread_id, message);
                        }
                    }
                    AgentEvent::TurnEnded(_) => {
                        persist_thread_snapshot(
                            store_for_task.as_ref(),
                            &slot_for_task,
                            now_token(),
                        );
                    }
                    AgentEvent::Error(_) => {}
                    AgentEvent::PermissionRequest(req) => {
                        slot_for_task
                            .pending_requests
                            .lock()
                            .expect("pending_requests mutex poisoned")
                            .push(req.clone());
                    }
                    AgentEvent::TerminalOutput(term_ev) => {
                        store_terminal_output(&slot_for_task, term_ev);
                    }
                    AgentEvent::SessionModes(_)
                    | AgentEvent::CurrentModeChanged(_)
                    | AgentEvent::ConfigOptions(_) => {
                        store_capability_event(&slot_for_task, &event);
                    }
                }
                events_out
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event,
                    });
            }
        });
        self.slots.push(slot);
        Ok(idx)
    }

    /// Drains every event queued since the last call. Non-blocking, safe
    /// to call from the Slint/UI thread on a timer -- see `lib.rs`'s
    /// `panel_rust_poll`. By the time an event is visible here, any
    /// history mutation it implies has already been applied (see the
    /// forwarder task above), so callers can immediately follow up with
    /// `history(idx)` for a consistent view.
    pub fn poll(&self) -> Vec<BridgeEvent> {
        self.events
            .lock()
            .expect("event queue mutex poisoned")
            .drain(..)
            .collect()
    }

    /// Snapshot of a thread's full scrollback (jsonl-seeded entries plus
    /// anything streamed live since), in display order.
    pub fn history(&self, idx: usize) -> Vec<ChatMessage> {
        self.slots
            .get(idx)
            .map(|s| s.history.lock().expect("history mutex poisoned").clone())
            .unwrap_or_default()
    }

    /// Snapshot of a thread's currently-pending interactive requests
    /// (`session/request_permission`, `fs/*`, `terminal/create`) --
    /// what a permission/approval request-card component should render.
    /// In practice at most one entry (see [`ThreadSlot::pending_requests`]'s
    /// doc comment), but returned as a `Vec` for the same reason that
    /// field is one.
    pub fn pending_requests(&self, idx: usize) -> Vec<AgentRequestEvent> {
        self.slots
            .get(idx)
            .map(|s| {
                s.pending_requests
                    .lock()
                    .expect("pending_requests mutex poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    /// Current live snapshot of `terminal_id` on thread `idx`, if any
    /// `AgentEvent::TerminalOutput` has been observed for it yet.
    pub fn terminal_buffer(&self, idx: usize, terminal_id: &str) -> Option<TerminalBuffer> {
        self.slots.get(idx).and_then(|s| {
            s.terminal_buffers
                .lock()
                .expect("terminal_buffers mutex poisoned")
                .get(terminal_id)
                .cloned()
        })
    }

   /// Every terminal id known on thread `idx` so far, first-seen order
   /// -- what a terminal-view component iterates to render one card per
   /// live/finished terminal. Paired with [`Self::terminal_buffer`] for
   /// each id's current output/exit state.
   pub fn active_terminals(&self, idx: usize) -> Vec<String> {
       self.slots
           .get(idx)
           .map(|s| {
               s.terminal_order
                   .lock()
                   .expect("terminal_order mutex poisoned")
                   .clone()
           })
           .unwrap_or_default()
   }

    /// `profiles/list` against thread `idx`'s bound gateway -- what the
    /// settings sheet's profile picker populates its choices from.
    /// Blocking (`block_on` on the background runtime, same "settings
    /// UI is a low-frequency, blocking-acceptable action" convention
    /// `open_session`'s own `block_on` use documents) since this is
    /// called synchronously from a Slint button-click handler with no
    /// other useful place to await a future. Returns an empty list
    /// (rather than propagating the error to a UI with no error-toast
    /// mechanism yet) if the call fails -- the picker then just shows
    /// no choices, same degrade-gracefully posture already used for the
    /// existing free-text profile fields.
    pub fn list_profiles(&self, idx: usize) -> Vec<crate::gateway_actor::ProfileSummary> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.list_profiles())
            .unwrap_or_default()
    }

    /// `mcp_servers/list` against thread `idx`'s bound gateway -- what
    /// the settings sheet's MCP-server list populates from. Same
    /// blocking/degrade-gracefully-on-error convention as
    /// [`Self::list_profiles`].
    pub fn list_mcp_servers(&self, idx: usize) -> Vec<serde_json::Value> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.list_mcp_servers())
            .unwrap_or_default()
    }

    /// `mcp_servers/create`. Returns `true` on success -- the caller
    /// (`lib.rs`'s settings-sheet save handler) is expected to re-call
    /// [`Self::list_mcp_servers`] afterward to refresh the UI list from
    /// the gateway's own state, same "don't optimistically mutate
    /// client-side state" posture the mode/config selector uses.
    pub fn create_mcp_server(&self, idx: usize, entry: serde_json::Value) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime.block_on(handle.create_mcp_server(entry)).is_ok()
    }

    /// `mcp_servers/update` -- same payload shape as [`Self::
    /// create_mcp_server`].
    pub fn update_mcp_server(&self, idx: usize, entry: serde_json::Value) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime.block_on(handle.update_mcp_server(entry)).is_ok()
    }

    /// `mcp_servers/delete`.
    pub fn delete_mcp_server(&self, idx: usize, name: &str) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.delete_mcp_server(name.to_string()))
            .is_ok()
    }

    /// `agents/list` against thread `idx`'s bound gateway -- the
    /// registry catalogue (installed/not-installed/runtime-missing
    /// status per entry) an agent-catalog UI section populates from.
    /// Same blocking/degrade-gracefully-on-error convention as
    /// [`Self::list_profiles`].
    pub fn list_agents(&self, idx: usize) -> Vec<serde_json::Value> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        let handle = slot.handle.clone();
        self.runtime.block_on(handle.list_agents()).unwrap_or_default()
    }

    /// `agents/install` -- client-initiated installer trigger. Returns
    /// `true` on success; the caller is expected to re-call
    /// [`Self::list_agents`] afterward to refresh the catalogue's
    /// status from the gateway's own real detection, not a client-side
    /// optimistic flip to "installed".
    pub fn install_agent(&self, idx: usize, agent_id: &str) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.install_agent(agent_id.to_string()))
            .is_ok()
    }

    /// Opens (or returns the already-open) client-local PTY terminal
    /// for thread `idx` -- see [`crate::local_terminal::LocalTerminal`]'s
    /// doc comment for what "client-local" means (a real shell process
    /// this panel spawns itself, never touching the gateway). Returns
    /// `false` if `idx` is out of range or the real PTY spawn failed
    /// (e.g. no shell resolvable); the caller degrades to "no terminal
    /// card shown" in that case, same posture as this crate's other
    /// gateway-call accessors.
    pub fn open_local_terminal(&self, idx: usize, cols: u16, rows: u16) -> bool {
        if idx >= self.slots.len() {
            return false;
        }
        let mut local_terminals = self.local_terminals.borrow_mut();
        if local_terminals.contains_key(&idx) {
            return true;
        }
        match crate::local_terminal::LocalTerminal::spawn(cols, rows) {
            Ok(term) => {
                local_terminals.insert(idx, term);
                true
            }
            Err(error) => {
                eprintln!("panel-rust: failed to spawn local terminal for thread {idx}: {error}");
                false
            }
        }
    }

    /// `true` if thread `idx` currently has an open client-local
    /// terminal (drives whether the Slint card renders at all).
    pub fn has_local_terminal(&self, idx: usize) -> bool {
        self.local_terminals.borrow().contains_key(&idx)
    }

    /// A snapshot of thread `idx`'s local terminal's current VT100
    /// screen state, or `None` if no terminal is open. `&mut self`
    /// Interior-mutable (`RefCell`, `&self`) rather than `&mut self` --
    /// checking whether the shell process has exited (`LocalTerminal::
    /// has_exited`) requires a non-blocking `waitpid`-family call, which
    /// the underlying `Child` trait only exposes as `&mut self`, but
    /// every other per-thread read accessor on this type is `&self`
    /// (see the field's own doc comment), so this borrows mutably
    /// through the `RefCell` instead of taking `&mut self`.
    pub fn local_terminal_snapshot(&self, idx: usize) -> Option<LocalTerminalSnapshot> {
        let mut local_terminals = self.local_terminals.borrow_mut();
        let term = local_terminals.get_mut(&idx)?;
        let (cursor_row, cursor_col) = term.cursor_position();
        Some(LocalTerminalSnapshot {
            screen_text: term.screen_text(),
            cols: term.cols(),
            rows: term.rows(),
            cursor_row,
            cursor_col,
            has_exited: term.has_exited(),
        })
    }

    /// Writes raw input bytes to thread `idx`'s local terminal, if one
    /// is open. A no-op (not an error) if none is open -- the caller
    /// (a Slint `FocusScope::key-pressed` handler) has no meaningful
    /// recovery action either way.
    pub fn write_local_terminal_input(&self, idx: usize, bytes: &[u8]) {
        if let Some(term) = self.local_terminals.borrow_mut().get_mut(&idx) {
            if let Err(error) = term.write_input(bytes) {
                eprintln!("panel-rust: local terminal write_input failed for thread {idx}: {error}");
            }
        }
    }

    /// Live-resizes thread `idx`'s local terminal, if one is open.
    pub fn resize_local_terminal(&self, idx: usize, cols: u16, rows: u16) {
        if let Some(term) = self.local_terminals.borrow_mut().get_mut(&idx) {
            if let Err(error) = term.resize(cols, rows) {
                eprintln!("panel-rust: local terminal resize failed for thread {idx}: {error}");
            }
        }
    }

    /// Closes (kills, see `LocalTerminal`'s `Drop` impl) thread `idx`'s
    /// local terminal, if one is open.
    pub fn close_local_terminal(&self, idx: usize) {
        self.local_terminals.borrow_mut().remove(&idx);
    }

    /// Answers a pending interactive request (identified by `relay_id`)
    /// with `response` and removes it from the thread's pending queue --
    /// called from the Slint approve/reject button callbacks via
    /// `lib.rs`. Fire-and-forget on the background runtime, same as
    /// [`Self::send_prompt`]: the caller is the synchronous UI thread,
    /// and any failure (gateway gone, relay already timed out) surfaces
    /// as a queued `AgentEvent::Error` rather than a return value this
    /// call site couldn't usefully act on. Removing the entry from
    /// `pending_requests` happens synchronously, before the async
    /// response is even sent -- the UI should stop showing this
    /// request's card immediately on click, regardless of whether the
    /// gateway round trip that follows succeeds.
    pub fn respond_to_request(&self, idx: usize, relay_id: &str, response: serde_json::Value) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        {
            let mut pending = slot
                .pending_requests
                .lock()
                .expect("pending_requests mutex poisoned");
            pending.retain(|req| req.relay_id != relay_id);
        }
        let handle = slot.handle.clone();
        let events_out = self.events.clone();
        let relay_id = relay_id.to_string();
        self.runtime.spawn(async move {
            if let Err(e) = handle.respond_agent_request(relay_id, response).await {
                events_out
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("respond_agent_request failed: {e}")),
                    });
            }
        });
    }

    /// Immediately (synchronously) records a locally-originated message
    /// (the user's own compose-box send) into both in-memory history and
    /// the jsonl cache, ahead of any network round trip -- so
    /// `history(idx)` reflects it the instant this returns, and a crash
    /// before the agent's reply arrives still leaves the user's own
    /// message durably cached.
    pub fn push_local(&self, idx: usize, msg: ChatMessage) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        slot.history
            .lock()
            .expect("history mutex poisoned")
            .push(msg.clone());
        if let Some(store) = &self.store {
            if let Err(e) = store.append(&slot.thread_id, &msg) {
                eprintln!(
                    "panel-rust: jsonl append failed for {}: {e}",
                    slot.thread_id
                );
            }
        }
    }

    /// Fire-and-forget: dispatches `text` to the given thread's bound
    /// agent on the background runtime. Errors surface as a queued
    /// `AgentEvent::Error`, consistent with every other agent-originated
    /// event, rather than via a return value the (synchronous) caller
    /// couldn't usefully act on anyway.
    pub fn send_prompt(&self, idx: usize, text: String) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(e) = handle.send_prompt(text).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("send_prompt failed: {e}")),
                    });
            }
        });
    }

    /// Dispatches the control operation on the handle's independent cancel
    /// connection. It deliberately does not wait for the prompt task.
    pub fn cancel_prompt(&self, idx: usize) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(e) = handle.cancel_session().await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session/cancel failed: {e}")),
                    });
            }
        });
    }

    /// Most recently advertised `modes` for thread `idx` -- what the
    /// settings-sheet mode selector reads to decide whether to show
    /// itself at all (`None`/empty `available` -> hidden, matching the
    /// Coverage Matrix's "capability-gated selection" requirement, not
    /// a control that's always present and silently no-ops). Read-only
    /// snapshot of [`ThreadSlot::session_modes`], updated by
    /// [`store_capability_event`] as `AgentEvent::SessionModes`/
    /// `CurrentModeChanged` events are drained through `poll()`.
    pub fn session_modes(&self, idx: usize) -> Option<SessionModesEvent> {
        let slot = self.slots.get(idx)?;
        slot.session_modes
            .lock()
            .expect("session_modes mutex poisoned")
            .clone()
    }

    /// Most recently advertised `configOptions` for thread `idx` -- see
    /// [`Self::session_modes`]'s doc comment for the same capability-
    /// gating rationale (empty vec -> selector hidden).
    pub fn config_options(&self, idx: usize) -> Vec<ConfigOptionInfo> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        slot.config_options
            .lock()
            .expect("config_options mutex poisoned")
            .clone()
    }

    /// Dispatches `session/set_mode` on the background runtime. Fire-
    /// and-forget like [`Self::send_prompt`]/[`Self::cancel_prompt`]:
    /// the caller is the synchronous UI thread, and a failure surfaces
    /// as a queued `AgentEvent::Error` rather than a return value. A
    /// successful call has no immediate visible effect on `session_
    /// modes(idx)` -- a real backend still owns `currentModeId` and
    /// confirms the change via a live `current_mode_update`
    /// notification (see `AgentEvent::CurrentModeChanged`'s doc
    /// comment), so the settings sheet should treat this as
    /// "requested", not "applied", until that event arrives.
    pub fn set_mode(&self, idx: usize, mode_id: String) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(e) = handle.set_mode(mode_id).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session/set_mode failed: {e}")),
                    });
            }
        });
    }

    /// Dispatches `session/set_config_option` on the background
    /// runtime. Unlike [`Self::set_mode`], a successful call's own
    /// response carries the full updated `configOptions[]` -- the actor
    /// (`crate::gateway_actor::thread_actor`) already re-emits that as a
    /// fresh `AgentEvent::ConfigOptions`, which `poll()`/`store_
    /// capability_event` apply the same as any other occurrence, so
    /// `config_options(idx)` reflects the change shortly after this
    /// call resolves without any extra plumbing here.
    pub fn set_config_option(&self, idx: usize, config_id: String, value: serde_json::Value) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(e) = handle.set_config_option(config_id, value).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!(
                            "session/set_config_option failed: {e}"
                        )),
                    });
            }
        });
    }
}

impl Drop for AgentBridge {
    fn drop(&mut self) {
        // Ask every actor to stop so its forwarder task's `events_rx.recv()`
        // returns `None` and unwinds cleanly, instead of relying purely on
        // the runtime's own shutdown-cancels-outstanding-tasks behavior.
        for slot in &self.slots {
            slot.handle.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Standalone-thread constructor (its own dedicated connection, not
    // the bridge's shared-gateway pool) -- used directly by tests below
    // that want to talk to a `TestGateway` without going through a full
    // `AgentBridge`.
    use crate::gateway_actor::spawn_acpx_thread;
    use crate::protocol_types::MessageKind;

    /// Real, already-built `acpx-server` binary next to this crate's own
    /// checkout -- same dev-checkout-relative-path convention
    /// `resolve_acpx_server_bin` uses in production.
    fn acpx_server_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
    }

    fn mock_agent_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/debug/rui-mock-agent")
    }

    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.local_addr().expect("local_addr").port()
    }

    /// Spawns a real `acpx-server` child process on a fresh ephemeral
    /// port, retrying the whole pick-port/spawn/wait-for-connect cycle
    /// (bounded at 5 attempts) if the process never becomes reachable
    /// within one attempt's own shorter window.
    ///
    /// **Why this exists.** `free_port()`'s own "bind a listener, read
    /// its port, then immediately drop the listener" trick has an
    /// unavoidable TOCTOU gap: the port is released back to the OS the
    /// instant the listener drops, and nothing stops a *different*
    /// concurrently-running test's own `free_port()` call (this crate's
    /// real-process tests each spawn their own `acpx-server`, and the
    /// default `cargo test` runner runs many of them in parallel) from
    /// claiming the exact same port before this function's own spawned
    /// process gets to bind it. When that race is lost, `acpx-server`
    /// fails its own bind and exits immediately, and the previous single-
    /// shot 100x30ms connect-retry loop just spun for its full ~3s doing
    /// nothing before every caller of it (this function's predecessor)
    /// silently proceeded anyway with a `base_url` nothing was listening
    /// on -- surfacing later as a confusing "gateway request timed out"
    /// failure in whichever test happened to run at the time, not a
    /// clear "port collision" signal. **Observed directly**: re-running
    /// this crate's full `--lib` suite back-to-back under the default
    /// parallel runner rotates which real-process test fails from run to
    /// run, while every test passes cleanly under `--test-threads=1` --
    /// exactly the signature of port contention, not a logic bug in any
    /// one test (documented in this plan's own Progress Log across two
    /// prior sessions before this fix).
    fn spawn_acpx_server_with_retry(
        configure: impl Fn(&mut std::process::Command, u16),
    ) -> (std::process::Child, String) {
        for attempt in 0..5 {
            let port = free_port();
            let mut command = std::process::Command::new(acpx_server_bin());
            configure(&mut command, port);
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut child = command
                .spawn()
                .expect("spawn real acpx-server binary for test");

            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            let mut reachable = false;
            while std::time::Instant::now() < deadline {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    reachable = true;
                    break;
                }
                if let Ok(Some(_status)) = child.try_wait() {
                    // The process already exited (most likely: lost the
                    // bind race for this exact port) -- no point
                    // continuing to poll a socket nothing will ever
                    // listen on.
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            if reachable {
                return (child, format!("http://127.0.0.1:{port}"));
            }
            let _ = child.kill();
            let _ = child.wait();
            if attempt < 4 {
                std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1)));
            }
        }
        panic!(
            "acpx-server never became reachable after 5 fresh-port attempts -- \
             this looks like more than ordinary port contention"
        );
    }

    /// A real, locally-spawned `acpx-server` process (with the real
    /// `rui-mock-agent` as its backend) for this module's tests to dial
    /// -- matches this project's established "spawn the real binary,
    /// don't fake the gateway boundary" testing discipline (see
    /// `rui-acpx-client`'s own `gateway_e2e_test.rs`). Killed on drop.
    struct TestGateway {
        child: std::process::Child,
        pub base_url: String,
    }

    impl TestGateway {
        fn spawn() -> Self {
            Self::spawn_with_persona("test")
        }

        /// Same as [`Self::spawn`], but tags the backend's replies with
        /// `persona` (via `RUI_MOCK_AGENT_PERSONA`) -- used by the
        /// multi-provider isolation test below to prove which gateway a
        /// reply actually came through.
        fn spawn_with_persona(persona: &str) -> Self {
            Self::spawn_with_persona_and_db(persona, None)
        }

        fn spawn_with_persona_and_db(persona: &str, db_path: Option<&std::path::Path>) -> Self {
            Self::spawn_with_backend_cmd(
                &mock_agent_bin().to_string_lossy(),
                persona,
                db_path,
            )
        }

        /// Same as [`Self::spawn_with_persona_and_db`], but with an
        /// arbitrary `ACPX_BACKEND_CMD` instead of the real
        /// `rui-mock-agent` binary -- used by the interactive-relay test
        /// below, which needs a stand-in backend that sends a real
        /// mid-turn `session/request_permission` request (`rui-mock-agent`
        /// only speaks the plain three-notification-then-EndTurn shape
        /// its own module doc describes, no agent-initiated requests).
        fn spawn_with_backend_cmd(
            backend_cmd: &str,
            persona: &str,
            db_path: Option<&std::path::Path>,
        ) -> Self {
            let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
                command
                    .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                    .env("ACPX_BACKEND_CMD", backend_cmd)
                    .env("ACPX_DEFAULT_AGENT_ID", persona)
                    .env("RUI_MOCK_AGENT_PERSONA", persona)
                    .env("RUST_LOG", "error");
                if let Some(db_path) = db_path {
                    command.env("ACPX_DB_PATH", db_path);
                }
            });
            TestGateway { child, base_url }
        }
    }

    impl Drop for TestGateway {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// `new_with_gateway_resolver_and_cache_dir` with every provider
    /// pinned to the same single `TestGateway` -- the shape most of this
    /// module's tests want (they're exercising jsonl-cache/bridge
    /// behavior, not multi-provider routing itself, which
    /// `two_threads_route_to_two_distinct_gateways_by_provider` below
    /// covers separately).
    fn bridge_with_single_gateway(
        names: &[&str],
        gateway: &TestGateway,
        cache_dir: Option<PathBuf>,
    ) -> Result<AgentBridge, BridgeError> {
        let base_url = gateway.base_url.clone();
        AgentBridge::new_with_gateway_resolver_and_cache_dir(
            names,
            move |_provider| Ok(base_url.clone()),
            cache_dir,
        )
    }

    #[test]
    fn add_thread_opens_a_persistent_session_and_routes_prompts() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Thread One", "Thread Two"];
        let mut bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("bridge");

        let index = bridge.add_thread("New thread 1").expect("add thread");
        assert_eq!(index, 2);
        assert!(bridge.history(index).is_empty());

        bridge.push_local(
            index,
            ChatMessage {
                kind: MessageKind::User,
                text: "hello from a new thread".into(),
                status: None,
            },
        );
        bridge.send_prompt(index, "hello from a new thread".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = false;
        while std::time::Instant::now() < deadline && !ended {
            ended = bridge
                .poll()
                .into_iter()
                .any(|event| matches!(event.event, AgentEvent::TurnEnded(_)));
            if !ended {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(ended, "new thread prompt did not finish");
        assert!(bridge
            .history(index)
            .iter()
            .any(|message| { message.text.contains("HELLO FROM A NEW THREAD") }));
        assert!(cache_dir.path().join("new-thread-1.jsonl").is_file());
    }

    /// Cold-start persistence: a message written by one bridge instance
    /// remains the first message visible to a second bridge instance pointed
    /// at the same cache dir. Since this test does not send a prompt, the
    /// transcript-faithful gateway load has no backend turns to replay.
    #[test]
    fn history_persists_across_bridge_restarts_via_jsonl_cache() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Thread One"];

        {
            let bridge =
                bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                    .expect("first bridge");
            bridge.push_local(
                0,
                ChatMessage {
                    kind: MessageKind::User,
                    text: "hello from run one".into(),
                    status: None,
                },
            );
            assert_eq!(bridge.history(0).len(), 1);
        }

        let bridge2 =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("second bridge");
        let history = bridge2.history(0);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "hello from run one");
        assert_eq!(history[0].kind, MessageKind::User);
    }

    #[test]
    fn bridge_relaunch_resumes_cached_gateway_session_without_duplicate_replay() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let gateway = TestGateway::spawn_with_persona_and_db(
            "codex",
            Some(&db_dir.path().join("acpx.sqlite3")),
        );
        let names = ["Thread One"];

        let first_session_id;
        {
            let bridge =
                bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                    .expect("first bridge");
            first_session_id = bridge.slots[0]
                .acp_session_id
                .lock()
                .expect("session mutex")
                .clone()
                .expect("first session id");
            bridge.push_local(
                0,
                ChatMessage {
                    kind: MessageKind::User,
                    text: "first turn".into(),
                    status: None,
                },
            );
            bridge.send_prompt(0, "first turn".into());

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut ended = false;
            while std::time::Instant::now() < deadline && !ended {
                ended = bridge
                    .poll()
                    .into_iter()
                    .any(|event| matches!(event.event, AgentEvent::TurnEnded(_)));
                if !ended {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            assert!(ended, "first bridge turn did not finish");
        }

        let bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("relaunched bridge");
        let resumed_session_id = bridge.slots[0]
            .acp_session_id
            .lock()
            .expect("session mutex")
            .clone()
            .expect("resumed session id");
        assert_eq!(resumed_session_id, first_session_id);

        let history = bridge.history(0);
        assert_eq!(
            history
                .iter()
                .filter(|message| message.text.contains("FIRST TURN"))
                .count(),
            1,
            "session/load replay must not duplicate jsonl-cached history: {history:?}"
        );

        bridge.push_local(
            0,
            ChatMessage {
                kind: MessageKind::User,
                text: "second turn".into(),
                status: None,
            },
        );
        bridge.send_prompt(0, "second turn".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = false;
        while std::time::Instant::now() < deadline && !ended {
            ended = bridge
                .poll()
                .into_iter()
                .any(|event| matches!(event.event, AgentEvent::TurnEnded(_)));
            if !ended {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(ended, "resumed bridge turn did not finish");
        assert!(
            bridge
                .history(0)
                .iter()
                .any(|message| message.text.contains("SECOND TURN")),
            "new prompt did not continue the resumed gateway session"
        );
    }

    #[test]
    fn replay_matching_preserves_identical_messages_at_distinct_positions() {
        let message = ChatMessage {
            kind: MessageKind::Agent,
            text: "same answer".into(),
            status: None,
        };
        let mut history = vec![message.clone(), message.clone()];
        let mut cached_index = 0;

        assert!(replay_matches_cached_position(
            &history,
            &mut cached_index,
            &message
        ));
        assert!(replay_matches_cached_position(
            &history,
            &mut cached_index,
            &message
        ));
        assert_eq!(cached_index, 2);

        assert!(!replay_matches_cached_position(
            &history,
            &mut cached_index,
            &message
        ));
        history.push(message.clone());
        history.push(message);
        assert_eq!(history.len(), 4);
    }

    #[test]
    fn replay_matching_skips_cached_user_messages_without_duplicate_agent_updates() {
        let user = ChatMessage {
            kind: MessageKind::User,
            text: "same answer".into(),
            status: None,
        };
        let agent = ChatMessage {
            kind: MessageKind::Agent,
            text: "same answer".into(),
            status: None,
        };
        let history = vec![user, agent.clone()];
        let mut cached_index = 0;

        assert!(replay_matches_cached_position(
            &history,
            &mut cached_index,
            &agent
        ));
        assert_eq!(cached_index, 2);
    }

    #[test]
    fn session_id_is_persisted_before_first_turn_completes() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Thread One"];
        let bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("bridge");

        let cached = JsonlStore::open(cache_dir.path())
            .expect("cache store")
            .load("thread-one")
            .expect("cached thread");
        assert_eq!(
            cached
                .trailer
                .expect("session trailer should be written at open")
                .acp_session_id,
            bridge.slots[0]
                .acp_session_id
                .lock()
                .expect("session mutex")
                .clone()
                .expect("active session")
        );
    }

    #[test]
    fn dropping_bridge_does_not_close_gateway_session() {
        let gateway = TestGateway::spawn();
        let names = ["Thread One"];
        let session_id;
        {
            let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");
            session_id = bridge.slots[0]
                .acp_session_id
                .lock()
                .expect("session mutex")
                .clone()
                .expect("active session");
        }

        let runtime = tokio::runtime::Runtime::new().expect("checker runtime");
        let sessions = runtime.block_on(async {
            let checker = spawn_acpx_thread(gateway.base_url.clone());
            let sessions = checker.list_sessions().await.expect("list sessions");
            checker.shutdown();
            sessions
        });
        assert!(
            sessions
                .iter()
                .any(|session| session.acp_session_id == session_id),
            "AgentBridge drop must not send session/close; got {sessions:?}"
        );
    }

    /// No cross-thread bleed in the jsonl cache -- each thread's file is
    /// keyed by its own slug.
    #[test]
    fn distinct_threads_get_isolated_cache_files() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Thread A", "Thread B"];
        let bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("bridge");
        bridge.push_local(
            0,
            ChatMessage {
                kind: MessageKind::User,
                text: "a-only".into(),
                status: None,
            },
        );
        bridge.push_local(
            1,
            ChatMessage {
                kind: MessageKind::User,
                text: "b-only".into(),
                status: None,
            },
        );
        assert_eq!(bridge.history(0)[0].text, "a-only");
        assert_eq!(bridge.history(1)[0].text, "b-only");

        let a_file = std::fs::read_to_string(cache_dir.path().join("thread-a.jsonl")).unwrap();
        let b_file = std::fs::read_to_string(cache_dir.path().join("thread-b.jsonl")).unwrap();
        assert!(a_file.contains("a-only"));
        assert!(b_file.contains("b-only"));
        assert!(!a_file.contains("b-only"));
        assert!(!b_file.contains("a-only"));
    }

    /// `new_with_gateway_url` (no cache dir) keeps working in-memory-only,
    /// so the pre-persistence test suite / call sites are unaffected.
    #[test]
    fn no_cache_dir_means_no_jsonl_file_written() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Solo Thread"];
        let bridge =
            AgentBridge::new_with_gateway_url(&names, gateway.base_url.clone()).expect("bridge");
        bridge.push_local(
            0,
            ChatMessage {
                kind: MessageKind::User,
                text: "not persisted".into(),
                status: None,
            },
        );
        assert_eq!(bridge.history(0).len(), 1);
        assert!(!cache_dir.path().join("solo-thread.jsonl").exists());
    }

    #[test]
    fn slug_collapses_non_alphanumerics_and_lowercases() {
        assert_eq!(slug("Fix timeline crash"), "fix-timeline-crash");
        assert_eq!(slug("Export pipeline bug!"), "export-pipeline-bug");
    }

    #[test]
    fn provider_for_index_alternates_codex_and_claude() {
        assert_eq!(provider_for_index(0), "codex");
        assert_eq!(provider_for_index(1), "claude");
        assert_eq!(provider_for_index(2), "codex");
        assert_eq!(provider_for_index(3), "claude");
    }

    #[test]
    fn packaged_gateway_binary_resolution_prefers_override_then_relative_install() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let packaged = bin_dir.join("acpx-server");
        std::fs::write(&packaged, b"binary").expect("packaged binary");
        let exe = bin_dir.join("panel");

        assert_eq!(
            resolve_acpx_server_bin_from(
                Some("/explicit/acpx-server"),
                Some(&exe),
                Path::new("/manifest"),
            ),
            PathBuf::from("/explicit/acpx-server")
        );
        assert_eq!(
            resolve_acpx_server_bin_from(None, Some(&exe), Path::new("/manifest")),
            packaged
        );

        let libexec_dir = temp.path().join("libexec");
        std::fs::create_dir_all(&libexec_dir).expect("libexec dir");
        let libexec_bin = libexec_dir.join("acpx-server");
        std::fs::write(&libexec_bin, b"binary").expect("libexec binary");
        std::fs::remove_file(&packaged).expect("remove sibling binary");
        assert_eq!(
            resolve_acpx_server_bin_from(None, Some(&exe), Path::new("/manifest")),
            bin_dir.join("../libexec/acpx-server")
        );
    }

    #[test]
    fn packaged_gateway_binary_resolution_falls_back_to_dev_checkout() {
        assert_eq!(
            resolve_acpx_server_bin_from(None, None, Path::new("/manifest")),
            PathBuf::from("/manifest/../acpx/target/debug/acpx-server")
        );
    }

    #[test]
    fn cache_directory_resolution_follows_packaged_state_precedence() {
        let manifest = Path::new("/manifest");
        assert_eq!(
            resolve_cache_dir_from(
                Some("/explicit/cache"),
                Some("/xdg"),
                None,
                Some("/home/user"),
                manifest,
            ),
            PathBuf::from("/explicit/cache")
        );
        assert_eq!(
            resolve_cache_dir_from(None, Some("/xdg"), None, Some("/home/user"), manifest),
            PathBuf::from("/xdg/shotcut/rui-thread-cache")
        );
        assert_eq!(
            resolve_cache_dir_from(None, None, None, Some("/home/user"), manifest),
            PathBuf::from("/home/user/.local/state/shotcut/rui-thread-cache")
        );
        assert_eq!(
            resolve_cache_dir_from(None, None, None, None, manifest),
            PathBuf::from("/manifest/../.rui-thread-cache")
        );
        assert_eq!(
            resolve_cache_dir_from(
                None,
                None,
                Some("C:/Users/test/AppData/Local"),
                None,
                manifest
            ),
            PathBuf::from("C:/Users/test/AppData/Local/Shotcut/rui-thread-cache")
        );
    }

    /// Regression guard for a real bug found by this session's own
    /// headless smoke test: a bare TCP-connect "is something listening"
    /// check treated an unrelated, non-acpx HTTP service already bound
    /// to the default port as a reusable gateway, silently breaking
    /// every session on that provider. `probe_acpx_gateway` must reject
    /// a listener that doesn't actually speak acpx's JSON-RPC shape.
    #[test]
    fn probe_acpx_gateway_rejects_a_non_acpx_http_listener() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        std::thread::spawn(move || {
            // A trivial, real (not acpx) HTTP server -- always answers
            // "405 Method Not Allowed" with no JSON-RPC body, mirroring
            // the real unrelated service this bug was found against.
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        assert!(
            !probe_acpx_gateway(port),
            "a non-acpx HTTP listener must not be mistaken for a reusable gateway"
        );
    }

    /// The positive control for the same probe: a real, locally-spawned
    /// `acpx-server` must pass.
    #[test]
    fn probe_acpx_gateway_accepts_a_real_gateway() {
        let gateway = TestGateway::spawn();
        let port: u16 = gateway
            .base_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("parse port from base_url");
        assert!(
            probe_acpx_gateway(port),
            "a real acpx-server must pass its own liveness probe"
        );
    }

    #[test]
    fn probe_acpx_gateway_checks_provider_identity_when_requested() {
        let gateway = TestGateway::spawn_with_persona("codex");
        let port: u16 = gateway
            .base_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("parse port from base_url");
        assert!(probe_acpx_gateway_for_agent(port, Some("codex")));
        assert!(!probe_acpx_gateway_for_agent(port, Some("claude")));
    }

    /// End-to-end: a jsonl cache file seeded up front with a varied mix
    /// of message kinds (thinking/tool-call/user/agent, i.e. not just plain
    /// user/agent turns) renders immediately via `history(0)`, and once
    /// the live gateway-backed thread streams a real reply for a new prompt, the
    /// pre-seeded entries are neither lost nor reordered -- the live
    /// messages land strictly after them. This is the concrete
    /// "json loading renders smoothly, no conflict with later async live
    /// reload" contract this module's docs describe.
    #[test]
    fn varied_seeded_json_and_live_reload_compose_without_conflict() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Fix timeline crash"];
        let thread_id = slug(names[0]);

        // Seed the cache directly (as if written by a prior run) with a
        // deliberately varied mix of message kinds, independent of this
        // bridge -- mirrors "content varies in json".
        let seed_store = JsonlStore::open(cache_dir.path()).expect("open store for seeding");
        let seeded_messages = vec![
            ChatMessage {
                kind: MessageKind::User,
                text: "add a crossfade".into(),
                status: None,
            },
            ChatMessage {
                kind: MessageKind::Thinking,
                text: "considering the timeline structure".into(),
                status: None,
            },
            ChatMessage {
                kind: MessageKind::ToolCall,
                text: "edit.add_transition(...)".into(),
                status: None,
            },
            ChatMessage {
                kind: MessageKind::Agent,
                text: "done, crossfade added".into(),
                status: None,
            },
        ];
        seed_store
            .overwrite(
                &thread_id,
                &seeded_messages,
                &ThreadTrailer {
                    acp_session_id: "prior-run-session".into(),
                    title: Some(thread_id.clone()),
                    updated_at: Some("unix:1".into()),
                    message_count: seeded_messages.len(),
                },
            )
            .expect("seed cache file");

        let bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("bridge");

        // Renders smoothly from disk immediately, before any live
        // connection work has necessarily completed.
        let initial = bridge.history(0);
        assert_eq!(initial, seeded_messages);

        // Drive one real live turn through the gateway-backed thread and
        // wait (bounded) for its events to land via poll().
        bridge.send_prompt(0, "second look".into());
        // By construction, `AgentBridge::new*` only returns once every
        // thread's session is already open (see the constructor's own
        // comment on why), so this prompt is guaranteed to actually
        // reach the mock agent -- a short bound is enough.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_turn_ended = false;
        while std::time::Instant::now() < deadline && !saw_turn_ended {
            for ev in bridge.poll() {
                if let AgentEvent::TurnEnded(_) = ev.event {
                    saw_turn_ended = true;
                }
            }
            if !saw_turn_ended {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(
            saw_turn_ended,
            "timed out waiting for the mock agent's turn to end"
        );

        let after = bridge.history(0);
        // The four pre-seeded, varied-kind messages are untouched and
        // still first, in original order.
        assert_eq!(&after[..4], &seeded_messages[..]);
        // The gateway-backed mock agent's reply (uppercased echo, per
        // mock_agent.rs) is
        // appended strictly after them, not interleaved or overwriting.
        assert!(after.len() > 4);
        assert!(after.iter().skip(4).any(|m| m.text.contains("SECOND LOOK")));

        // And the on-disk file reflects the same merged, non-conflicting
        // view after the TurnEnded-triggered trailer overwrite.
        let reloaded = seed_store.load(&thread_id).expect("reload from disk");
        assert_eq!(&reloaded.messages[..4], &seeded_messages[..]);
        assert!(reloaded.messages.len() > 4);
    }

    /// Regression guard for a real bug this session's manual smoke test
    /// caught: one thread's malformed/incompatible jsonl cache file must
    /// not disable the whole bridge (and every other thread's live agent
    /// connection with it) -- it should degrade to an empty scrollback
    /// for *that thread only*, exactly like a cache miss.
    #[test]
    fn malformed_jsonl_for_one_thread_does_not_break_construction_or_other_threads() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Broken Thread", "Healthy Thread"];

        // Hand-write a cache file with a bogus trailer field name --
        // exactly the kind of "content varies in json" mismatch this
        // module has to tolerate (e.g. a field renamed in a later
        // version of this crate, or a hand-edited file).
        std::fs::write(
            cache_dir.path().join("broken-thread.jsonl"),
            b"{\"line_kind\":\"trailer\",\"acp_session_id\":\"x\",\"title\":null,\"updated_at\":null,\"message_count\":0}\n",
        )
        .expect("write malformed cache file");

        let seed_store = JsonlStore::open(cache_dir.path()).expect("open store for seeding");
        seed_store
            .overwrite(
                "healthy-thread",
                &[ChatMessage {
                    kind: MessageKind::Agent,
                    text: "healthy scrollback".into(),
                    status: None,
                }],
                &ThreadTrailer {
                    acp_session_id: "ok".into(),
                    title: Some("Healthy Thread".into()),
                    updated_at: Some("unix:1".into()),
                    message_count: 1,
                },
            )
            .expect("seed healthy thread");

        // Must not error out entirely just because thread 0's cache is bad.
        let bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("bridge construction must survive one thread's bad cache file");

        // Broken thread degrades to empty history, not a fatal error.
        assert!(bridge.history(0).is_empty());
        // Healthy thread is completely unaffected.
        assert_eq!(bridge.history(1)[0].text, "healthy scrollback");
    }

    /// Real multi-provider routing: two distinct threads, resolved to two
    /// distinct (locally-spawned) `acpx-server` gateway processes by
    /// `provider_for_index`, each tagging its reply with its own persona
    /// -- the concrete `AgentBridge`-level version of
    /// `rui-acpx-client`'s own `two_gateways_stay_isolated_no_cross_provider_bleed`
    /// test, proving the wiring in *this* crate's constructor (provider
    /// resolution, per-provider gateway auto-spawn) also keeps threads
    /// isolated, not just the lower-level transport.
    #[test]
    fn two_threads_route_to_two_distinct_gateways_by_provider() {
        let codex_gateway = TestGateway::spawn_with_persona("codex");
        let claude_gateway = TestGateway::spawn_with_persona("claude");
        let codex_url = codex_gateway.base_url.clone();
        let claude_url = claude_gateway.base_url.clone();
        let names = ["Codex Thread", "Claude Thread"];

        let bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &names,
            move |provider| {
                if provider == "codex" {
                    Ok(codex_url.clone())
                } else {
                    Ok(claude_url.clone())
                }
            },
            None,
        )
        .expect("bridge with two distinct gateways");

        bridge.send_prompt(0, "ping".into());
        bridge.send_prompt(1, "ping".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = [false, false];
        while std::time::Instant::now() < deadline && !(ended[0] && ended[1]) {
            for ev in bridge.poll() {
                if let AgentEvent::TurnEnded(_) = ev.event {
                    ended[ev.thread_index] = true;
                }
            }
            if !(ended[0] && ended[1]) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(
            ended[0] && ended[1],
            "timed out waiting for both threads' turns to end"
        );

        let codex_history = bridge.history(0);
        let claude_history = bridge.history(1);
        let codex_reply = codex_history
            .iter()
            .find(|m| m.text.contains("PING"))
            .expect("codex thread reply");
        let claude_reply = claude_history
            .iter()
            .find(|m| m.text.contains("PING"))
            .expect("claude thread reply");
        assert!(
            codex_reply.text.starts_with("[CODEX]"),
            "got: {:?}",
            codex_reply.text
        );
        assert!(
            claude_reply.text.starts_with("[CLAUDE]"),
            "got: {:?}",
            claude_reply.text
        );
    }

    /// Same real stand-in-backend shell-script technique
    /// `acpx-server/tests/agent_request_relay_test.rs` uses, one layer up
    /// the stack: proves the interactive `session/request_permission`
    /// relay is wired all the way through `AgentBridge` -- not just
    /// `acpx-client`/`rui-acpx-client` in isolation. A real acpx-server
    /// relays a mid-turn permission request to this bridge as
    /// `AgentEvent::PermissionRequest`; `respond_to_request` answers it
    /// with `allow-once` (deliberately not the profile's default
    /// `AutoReject` policy, which would pick `reject-once` -- see the
    /// acpx-server test's own doc comment for why that's the right
    /// "only the live relay path could produce this" signal); the
    /// backend's own final `agent_message_chunk` echoes back which
    /// option it actually received, so `bridge.history` is the
    /// observable proof the live answer -- not the auto-policy fallback
    /// -- reached the backend.
    #[test]
    fn permission_request_relay_round_trips_through_the_bridge() {
        // Written to a real temp file (rather than passed as `sh -c
        // '...'`) because `ACPX_BACKEND_CMD` is parsed by naive
        // whitespace-splitting (see `acpx-server/src/config.rs`), which
        // would mangle an inline multi-word script.
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("stand_in_backend.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":999,"method":"session/request_permission","params":{"sessionId":"backend-abc","toolCall":{"toolCallId":"call-1","title":"Run a risky command"},"options":[{"optionId":"allow-once","name":"Allow","kind":"allow_once"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]}}\n'
    reply=""
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":999' && { reply="$reply_line"; break; }
    done
    chosen=$(echo "$reply" | grep -o '"optionId":"[^"]*"' | head -1 | cut -d: -f2 | tr -d '"')
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-abc","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CHOSE: %s"}}}}\n' "$chosen"
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#,
        )
        .expect("write stand-in backend script");

        let gateway = {
            let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
                command
                    .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                    .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
                    .env("ACPX_DEFAULT_AGENT_ID", "relay-test")
                    .env("RUST_LOG", "error");
            });
            TestGateway { child, base_url }
        };

        let names = ["Relay Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        bridge.send_prompt(0, "trigger the permission request".into());

        // Wait for the PermissionRequest event to surface, then answer
        // it -- exercising the exact path a real Slint approve-button
        // click drives via `PanelSingleton::answer_pending_request`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut answered = false;
        while std::time::Instant::now() < deadline && !answered {
            let pending = bridge.pending_requests(0);
            if let Some(event) = pending.first() {
                assert_eq!(event.method, "session/request_permission");
                let response = crate::permission::build_response(event, true);
                bridge.respond_to_request(0, &event.relay_id, response);
                answered = true;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(answered, "permission request never surfaced on the bridge");
        assert!(
            bridge.pending_requests(0).is_empty(),
            "pending_requests should be cleared synchronously by respond_to_request"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = false;
        while std::time::Instant::now() < deadline && !ended {
            ended = bridge
                .poll()
                .into_iter()
                .any(|event| matches!(event.event, AgentEvent::TurnEnded(_)));
            if !ended {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(ended, "prompt turn did not finish after answering the relay");

        let history = bridge.history(0);
       assert!(
           history.iter().any(|m| m.text.contains("CHOSE: allow-once")),
           "expected the backend's own echo to reflect the live-relayed \
           allow-once answer, not the profile's AutoReject default \
           (which would have picked reject-once): got {history:?}"
      );
   }

    /// Coverage-matrix `session/cancel` row: proves a real slow turn gets
    /// exactly one cancel and ends with `stopReason: "cancelled"`, driven
    /// through the same `AgentBridge::cancel_prompt` call
    /// `PanelSingleton::on_stop_requested` invokes from the Stop button.
    ///
    /// The stand-in backend never replies to `session/prompt` on its own
    /// (matching the real ACP spec: `session/cancel` is a client-sent
    /// *notification*, and the in-flight prompt call is what eventually
    /// resolves) -- it only replies once it sees `session/cancel` arrive on
    /// the same stdio stream, using the prompt's own captured `id`. If
    /// `cancel_prompt` failed to reach the backend at all, this test would
    /// hang until its own deadline and fail with `ended == false`, so a
    /// pass is proof the cancel notification, not a coincidental timeout,
    /// is what unblocked the turn.
    #[test]
    fn cancel_prompt_ends_a_slow_turn_with_cancelled_stop_reason() {
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("stand_in_backend.sh");
        let prompt_id_path = script_dir.path().join("prompt_id");
        std::fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    echo "$id" > {prompt_id_path}
  elif echo "$line" | grep -q '"method":"session/cancel"'; then
    prompt_id=$(cat {prompt_id_path})
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"cancelled"}}}}\n' "$prompt_id"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#,
                prompt_id_path = prompt_id_path.display(),
            ),
        )
        .expect("write stand-in backend script");

        let gateway = {
            let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
                command
                    .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                    .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
                    .env("ACPX_DEFAULT_AGENT_ID", "cancel-test")
                    .env("RUST_LOG", "error");
            });
            TestGateway { child, base_url }
        };

        let names = ["Cancel Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        bridge.send_prompt(0, "start a slow task".into());

        // Wait for the backend to actually be mid-prompt (its script has
        // captured the prompt's own `id`) before cancelling -- a cancel
        // that raced ahead of the prompt reaching the backend would prove
        // nothing about the cancel path itself.
        let capture_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < capture_deadline && !prompt_id_path.is_file() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            prompt_id_path.is_file(),
            "backend never observed the in-flight session/prompt"
        );

        bridge.cancel_prompt(0);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut stop_reason = None;
        while std::time::Instant::now() < deadline && stop_reason.is_none() {
            for event in bridge.poll() {
                if let AgentEvent::TurnEnded(reason) = event.event {
                    stop_reason = Some(reason);
                }
            }
            if stop_reason.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert_eq!(
            stop_reason.as_deref(),
            Some("cancelled"),
            "cancel_prompt should have produced exactly one TurnEnded(\"cancelled\"), got {stop_reason:?}"
        );
    }

   /// Real end-to-end proof of the profile-picker path this crate
   /// exposes to `lib.rs`'s settings sheet: `AgentBridge::list_profiles`
   /// sees a real profile registered on the gateway (including its
   /// capability flags), and `AgentBridge::add_thread_with_profile`
   /// actually threads `_acpx.profile` through to a real `session/new`
    /// call -- proven by the new thread's own terminal/create relay
    /// succeeding, which only happens when `allow_terminal_access` is
    /// true for the session's resolved profile (the default/no-profile
    /// path has it false, see `acpx_core::Profile::allow_terminal_access`'s
    /// default).
    #[test]
    fn add_thread_with_profile_unlocks_terminal_access_end_to_end() {
        // This test needs a stand-in backend that sends a real mid-turn
        // `terminal/create` request, which `rui-mock-agent`/
        // `spawn_with_backend_cmd`'s default backend cannot do -- reuse
        // the same stand-in shell script technique
        // `permission_request_relay_round_trips_through_the_bridge`
        // uses, driving a raw `acpx-server` process directly (built
        // below) instead of going through `spawn_with_backend_cmd`.
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("stand_in_backend.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-profile"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":971,"method":"terminal/create","params":{"sessionId":"backend-profile","command":"sh","args":["-c","printf profile-ok"]}}\n'
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":971' && break
    done
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#,
        )
        .expect("write stand-in backend script");

        let port = free_port();
        let mut command = std::process::Command::new(acpx_server_bin());
        command
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
            .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
            .env("ACPX_DEFAULT_AGENT_ID", "profile-picker-agent")
            .env("RUST_LOG", "error")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = command.spawn().expect("spawn real acpx-server binary");
        let base_url = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let gateway = TestGateway { child, base_url };

        // Register a profile with allow_terminal_access before either
        // list_profiles or add_thread_with_profile touches it.
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let client = acpx_client::raw::GatewayClient::new(gateway.base_url.clone());
            client
                .call(
                    "profiles/create",
                    serde_json::json!({
                        "name": "picker-enabled",
                        "agent_id": "profile-picker-agent",
                        "allow_terminal_access": true
                    }),
                    None,
                )
                .await
                .expect("profiles/create");
        });

        // Two seed threads (not one): `resolved_urls`/`gateway_urls` is
        // populated once, at construction, only for the providers the
        // *initial* thread list actually alternates across
        // (`provider_for_index`, codex at even indices, claude at odd
        // -- see that fn's own doc comment on why: production always
        // starts from the fixed four-thread list, so both providers are
        // always pre-resolved by the time any `add_thread*` call runs).
        // A single-seed-thread bridge would leave "claude" unresolved,
        // so `add_thread_with_profile`'s new thread at index 1 would
        // fail with "gateway URL missing for claude" before ever
        // reaching the profile/terminal-relay behavior this test
        // actually exercises. Both seed names still resolve to the same
        // single real `TestGateway` (`bridge_with_single_gateway`'s
        // resolver ignores the provider argument), so this doesn't
        // change what's under test.
        let mut bridge =
            bridge_with_single_gateway(&["Seed Thread", "Seed Thread Two"], &gateway, None)
                .expect("bridge with two seed threads");

        let profiles = bridge.list_profiles(0);
        assert!(
            profiles
                .iter()
                .any(|p| p.name == "picker-enabled" && p.allow_terminal_access),
            "expected list_profiles to see the just-created profile with \
             allow_terminal_access=true, got {profiles:?}"
        );

        let idx = bridge
            .add_thread_with_profile("Profile Thread", Some("picker-enabled"))
            .expect("add_thread_with_profile");
        bridge.send_prompt(idx, "start a terminal".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut relay_seen = false;
        while std::time::Instant::now() < deadline && !relay_seen {
            let pending = bridge.pending_requests(idx);
            if let Some(event) = pending.first() {
                assert_eq!(event.method, "terminal/create");
                let response = crate::permission::build_response(event, true);
                bridge.respond_to_request(idx, &event.relay_id, response);
                relay_seen = true;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(
            relay_seen,
            "expected a terminal/create relay on the profile-selected thread -- \
             a thread opened without this profile would never see one, since \
             the default profile has allow_terminal_access=false"
        );
    }

    /// Coverage-matrix `session/set_mode`/`session/set_config_option`
    /// row: proves (a) a real `session/new` response's `modes`/
    /// `configOptions` fields reach `AgentBridge::session_modes`/
    /// `config_options`, (b) `AgentBridge::set_mode` actually sends
    /// `session/set_mode` with the exact chosen `modeId` (proven by the
    /// stand-in backend only writing a marker file once it observes
    /// that call -- if `set_mode` silently no-opped or targeted the
    /// wrong session, the marker would never appear and this test would
    /// hang to its own deadline and fail), and (c) `AgentBridge::
    /// set_config_option`'s round trip re-emits the backend's *own*
    /// updated `configOptions[]` (with the new `currentValue`) as a
    /// fresh `AgentEvent::ConfigOptions` that `config_options(idx)`
    /// then reflects -- not just a client-side echo of the value this
    /// test sent.
    #[test]
    fn set_mode_and_set_config_option_reach_a_real_backend_and_update_bridge_state() {
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("mode_config_backend.sh");
        let set_mode_marker = script_dir.path().join("set_mode_id");
        let set_config_marker = script_dir.path().join("set_config_option_call");
        std::fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-mc","modes":{{"currentModeId":"ask","availableModes":[{{"id":"ask","name":"Ask"}},{{"id":"code","name":"Code","description":"Autonomous coding"}}]}},"configOptions":[{{"id":"model","name":"Model","type":"select","currentValue":"gpt-5","options":[{{"value":"gpt-5","name":"GPT-5"}},{{"value":"gpt-5-mini","name":"GPT-5 mini"}}]}}]}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/set_mode"'; then
    mode_id=$(echo "$line" | grep -o '"modeId":"[^"]*"' | head -1 | cut -d: -f2 | tr -d '"')
    echo "$mode_id" > {set_mode_marker}
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/set_config_option"'; then
    config_id=$(echo "$line" | grep -o '"configId":"[^"]*"' | head -1 | cut -d: -f2 | tr -d '"')
    value=$(echo "$line" | grep -o '"value":"[^"]*"' | head -1 | cut -d: -f2 | tr -d '"')
    printf '%s %s\n' "$config_id" "$value" > {set_config_marker}
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"configOptions":[{{"id":"model","name":"Model","type":"select","currentValue":"%s","options":[{{"value":"gpt-5","name":"GPT-5"}},{{"value":"gpt-5-mini","name":"GPT-5 mini"}}]}}]}}}}\n' "$id" "$value"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#,
                set_mode_marker = set_mode_marker.display(),
                set_config_marker = set_config_marker.display(),
            ),
        )
        .expect("write stand-in backend script");

        let gateway = {
            let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
                command
                    .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                    .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
                    .env("ACPX_DEFAULT_AGENT_ID", "mode-config-test")
                    .env("RUST_LOG", "error");
            });
            TestGateway { child, base_url }
        };

        let names = ["Mode Config Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        // (a) session/new's own modes/configOptions reached bridge
        // state. `session/new` itself resolves synchronously (via
        // `block_on` inside `AgentBridge::new`), but the forwarder task
        // that applies `SessionModes`/`ConfigOptions` to `ThreadSlot`
        // (`store_capability_event`) is a separate spawned task racing
        // this assertion -- poll with a deadline, same convention every
        // other event-driven assertion in this module already follows
        // (see the cancel/terminal-relay tests above), rather than
        // assuming synchronous availability.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut modes = None;
        while std::time::Instant::now() < deadline && modes.is_none() {
            modes = bridge.session_modes(0);
            if modes.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let modes = modes.expect("session/new's modes should have been captured by now");
        assert_eq!(modes.current_mode_id, "ask");
        assert_eq!(
            modes.available.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["ask", "code"]
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut options = Vec::new();
        while std::time::Instant::now() < deadline && options.is_empty() {
            options = bridge.config_options(0);
            if options.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, "model");
        assert_eq!(options[0].current_value.as_deref(), Some("gpt-5"));
        assert_eq!(options[0].options.len(), 2);

        // (b) set_mode reaches the real backend with the exact modeId.
        bridge.set_mode(0, "code".to_string());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !set_mode_marker.is_file() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let observed_mode_id =
            std::fs::read_to_string(&set_mode_marker).unwrap_or_default();
        assert_eq!(
            observed_mode_id.trim(),
            "code",
            "backend never observed session/set_mode with modeId=code"
        );

        // (c) set_config_option reaches the backend, and the bridge's
        // config_options(0) is refreshed from that call's own response
        // (the backend's *chosen* currentValue, not a client echo).
        bridge.set_config_option(0, "model".to_string(), serde_json::json!("gpt-5-mini"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !set_config_marker.is_file() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let observed_call = std::fs::read_to_string(&set_config_marker).unwrap_or_default();
        assert_eq!(
            observed_call.trim(),
            "model gpt-5-mini",
            "backend never observed session/set_config_option(configId=model, value=gpt-5-mini)"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut updated_value = None;
        while std::time::Instant::now() < deadline && updated_value.is_none() {
            updated_value = bridge
                .config_options(0)
                .into_iter()
                .find(|o| o.id == "model")
                .and_then(|o| o.current_value)
                .filter(|v| v == "gpt-5-mini");
            if updated_value.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert_eq!(
            updated_value.as_deref(),
            Some("gpt-5-mini"),
            "config_options(0) should reflect the backend's own updated currentValue \
             after session/set_config_option resolves"
        );
    }

    /// Coverage-matrix `mcp_servers/*`/`agents/*` rows, proven through
    /// `AgentBridge`'s own blocking accessors (not just `rui-acpx-
    /// client`'s actor, which `rui-acpx-client/tests/mcp_agents_e2e_
    /// test.rs` already covers directly) -- these are exactly what
    /// `lib.rs`'s settings-sheet callbacks call from a Slint
    /// button-click handler, so this is the layer a UI bug would
    /// actually manifest at.
    #[test]
    fn mcp_server_crud_and_agent_catalog_reach_a_real_backend_through_the_bridge() {
        let gateway = TestGateway::spawn();
        let names = ["Settings Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        assert!(
            bridge.list_mcp_servers(0).is_empty(),
            "expected no MCP servers on a fresh gateway"
        );
        assert!(bridge.create_mcp_server(
            0,
            serde_json::json!({ "name": "bridge-fs", "command": "mcp-bridge-fs" })
        ));
        let after_create = bridge.list_mcp_servers(0);
        assert_eq!(after_create.len(), 1);
        assert_eq!(after_create[0]["name"], "bridge-fs");

        assert!(bridge.update_mcp_server(
            0,
            serde_json::json!({ "name": "bridge-fs", "command": "mcp-bridge-fs-v2" })
        ));
        let after_update = bridge.list_mcp_servers(0);
        assert_eq!(after_update.len(), 1);
        assert_eq!(after_update[0]["command"], "mcp-bridge-fs-v2");

        assert!(bridge.delete_mcp_server(0, "bridge-fs"));
        assert!(
            bridge.list_mcp_servers(0).is_empty(),
            "expected the server to be gone after delete"
        );

        // Agent catalog: real fallback/live registry entries, each with
        // a real detection status -- not a client-side stub.
        let agents = bridge.list_agents(0);
        assert!(
            agents.iter().any(|a| a["id"] == "codex-acp"),
            "expected a codex-acp entry from the registry, got {agents:?}"
        );
        assert!(
            !bridge.install_agent(0, "definitely-not-a-real-agent-id"),
            "install_agent against an unknown id should fail against the real registry, not succeed"
        );
    }

    /// Client-local PTY terminal, proven through `AgentBridge`'s own
    /// accessors (`local_terminal.rs`'s own tests already prove the
    /// lower `LocalTerminal` layer against a real shell directly --
    /// this proves the bridge's per-thread open/write/resize/close
    /// wrapper reaches the exact same real behavior, the layer `lib.rs`
    /// actually calls from Slint callbacks). No gateway involved at all
    /// -- `TestGateway` here only supplies a thread to index into,
    /// proving thread-index scoping (two threads get two independent
    /// real shell processes) rather than anything ACP-related.
    #[test]
    fn local_terminal_open_write_resize_and_close_reach_a_real_shell_through_the_bridge() {
        let gateway = TestGateway::spawn();
        let names = ["Terminal Thread One", "Terminal Thread Two"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        assert!(!bridge.has_local_terminal(0));
        assert!(bridge.local_terminal_snapshot(0).is_none());

        assert!(bridge.open_local_terminal(0, 80, 24));
        assert!(bridge.has_local_terminal(0));
        // Idempotent -- opening again on the same thread must not spawn
        // a second shell process, just report the existing one is open.
        assert!(bridge.open_local_terminal(0, 80, 24));

        bridge.write_local_terminal_input(0, b"echo BRIDGE_PTY_MARKER_998877\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = false;
        while std::time::Instant::now() < deadline && !seen {
            if let Some(snapshot) = bridge.local_terminal_snapshot(0) {
                if snapshot.screen_text.contains("BRIDGE_PTY_MARKER_998877") {
                    seen = true;
                }
            }
            if !seen {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(seen, "expected the real shell's own echoed output through the bridge");

        bridge.resize_local_terminal(0, 100, 40);
        let resized = bridge
            .local_terminal_snapshot(0)
            .expect("terminal still open after resize");
        assert_eq!(resized.cols, 100);
        assert_eq!(resized.rows, 40);

        // Thread 1's own local terminal is untouched -- proves the map
        // is genuinely keyed per thread index, not a single shared slot.
        assert!(!bridge.has_local_terminal(1));

        bridge.close_local_terminal(0);
        assert!(!bridge.has_local_terminal(0));
        assert!(bridge.local_terminal_snapshot(0).is_none());
    }
}
