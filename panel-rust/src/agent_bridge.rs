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
//! Backed by [`rui_acp_client::JsonlStore`] -- one `<thread_id>.jsonl`
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

use rui_acp_client::{AgentEvent, ChatMessage, JsonlStore, ThreadTrailer};
use rui_acpx_client::{spawn_acpx_thread, AcpxThreadHandle};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(thiserror::Error, Debug)]
pub enum BridgeError {
    #[error("failed to start background async runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("jsonl cache error: {0}")]
    Cache(#[source] rui_acp_client::CacheError),
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
}

/// Owns the background runtime, the per-thread agent connections, the
/// jsonl cache, and the event queue the UI thread drains via `poll`.
pub struct AgentBridge {
    runtime: tokio::runtime::Runtime,
    slots: Vec<Arc<ThreadSlot>>,
    events: Arc<Mutex<VecDeque<BridgeEvent>>>,
    #[allow(dead_code)] // kept alive for its Drop / for future direct use
    store: Option<JsonlStore>,
}

/// Turns a UI thread display name into a filesystem-safe, stable jsonl
/// cache key -- lowercased, non-alphanumerics collapsed to `-`. Stable
/// across runs as long as `THREAD_NAMES` (in `lib.rs`) doesn't change,
/// which is the v1 fixed-thread-list assumption documented there.
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
fn resolve_acpx_server_bin() -> PathBuf {
    if let Ok(bin) = std::env::var("RUI_ACPX_SERVER_BIN") {
        return PathBuf::from(bin);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
}

/// Resolves the mock backend agent binary the locally-spawned gateway
/// should proxy to: `RUI_ACP_AGENT_CMD` env override (a real
/// ACP-compliant agent binary/command), else the dev-checkout
/// `rui-mock-agent` built alongside `rui-acp-client` -- the same fallback
/// `resolve_agent_command` used for the (now-retired) direct-subprocess
/// path, kept as the acpx-gateway's own default backend for dev/test.
fn resolve_backend_agent_command() -> String {
    if let Ok(cmd) = std::env::var("RUI_ACP_AGENT_CMD") {
        return cmd;
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rui-acp-client/target/debug/rui-mock-agent")
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
/// `acpx-server` (and, transitively, its own backend subprocess) is a
/// completely ordinary child of *this* process, inheriting this
/// process's session/process-group rather than being placed in a new
/// one -- so it is reparented to init and keeps running if this process
/// (the panel / the whole host application) is killed by PID rather
/// than by process-group signal. This is exactly the "window close does
/// not imply session close" contract: the gateway process, and
/// therefore every session it holds open, survives the panel
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

/// Resolves the jsonl cache directory: `RUI_ACP_CACHE_DIR` env override,
/// else a dev-checkout fallback sibling to this crate.
pub fn resolve_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RUI_ACP_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.rui-thread-cache")
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

        // `spawn_acpx_thread` calls the free-function `tokio::spawn` internally,
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
            let seeded = match &store {
                Some(s) => match s.load(&thread_id) {
                    Ok(cached) => cached.messages,
                    Err(e) => {
                        eprintln!(
                            "panel-rust: jsonl cache load failed for thread {thread_id:?} ({e}); starting this thread with empty history rather than failing the whole bridge"
                        );
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };

            let provider = provider_for_index(idx);
            let base_url = resolved_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
            let mut handle = spawn_acpx_thread(base_url);
            let mut events_rx = handle.take_events();
            let handle = Arc::new(handle);

            let slot = Arc::new(ThreadSlot {
                thread_id: thread_id.clone(),
                handle: handle.clone(),
                history: Mutex::new(seeded),
                acp_session_id: Mutex::new(None),
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
            match runtime.block_on(handle_for_task.open_session(cwd_for_session())) {
                Ok(session_id) => {
                    *slot_for_task
                        .acp_session_id
                        .lock()
                        .expect("acp_session_id mutex poisoned") = Some(session_id);
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
                            if let Some(store) = &store_for_task {
                                let hist = slot_for_task
                                    .history
                                    .lock()
                                    .expect("history mutex poisoned")
                                    .clone();
                                let session_id = slot_for_task
                                    .acp_session_id
                                    .lock()
                                    .expect("acp_session_id mutex poisoned")
                                    .clone()
                                    .unwrap_or_default();
                                let trailer = ThreadTrailer {
                                    acp_session_id: session_id,
                                    title: Some(slot_for_task.thread_id.clone()),
                                    updated_at: Some(now_token()),
                                    message_count: hist.len(),
                                };
                                if let Err(e) =
                                    store.overwrite(&slot_for_task.thread_id, &hist, &trailer)
                                {
                                    eprintln!(
                                        "panel-rust: jsonl trailer overwrite failed for {}: {e}",
                                        slot_for_task.thread_id
                                    );
                                }
                            }
                        }
                        AgentEvent::Error(_) => {}
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
            store,
        })
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
    use rui_acp_client::MessageKind;

    /// Real, already-built `acpx-server` binary next to this crate's own
    /// checkout -- same dev-checkout-relative-path convention
    /// `resolve_acpx_server_bin` uses in production.
    fn acpx_server_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
    }

    fn mock_agent_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rui-acp-client/target/debug/rui-mock-agent")
    }

    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.local_addr().expect("local_addr").port()
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
            let port = free_port();
            let child = std::process::Command::new(acpx_server_bin())
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env(
                    "ACPX_BACKEND_CMD",
                    mock_agent_bin().to_string_lossy().to_string(),
                )
                .env("ACPX_DEFAULT_AGENT_ID", persona)
                .env("RUI_MOCK_AGENT_PERSONA", persona)
                .env("RUST_LOG", "error")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn real acpx-server binary for test");
            let base_url = format!("http://127.0.0.1:{port}");
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
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

    /// Cold-start persistence: a message written by one bridge instance
    /// is visible (without any live agent involvement) to a second bridge
    /// instance pointed at the same cache dir -- the "later async live
    /// reload" contract from a prior run's perspective.
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
}
