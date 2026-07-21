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
//!   gateway/session reconciliation happens on the bridge runtime after
//!   construction. Prompt and control operations wait for that attachment,
//!   so a follow-up submitted immediately after first render is preserved
//!   without blocking panel creation.
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

use crate::conversation::ConversationState;
use crate::gateway_actor::{
    spawn_acpx_thread_with_delayed_gateway, spawn_acpx_thread_with_gateway,
    AcpxThreadGatewaySetter, AcpxThreadHandle,
};
use crate::jsonl_store::{
    JsonlStore, TerminalRuntimeSnapshot, ThreadRuntimeSnapshot, ThreadTrailer,
};
use crate::protocol_types::{
    AgentEvent, AgentRequestEvent, ChatMessage, ConfigOptionInfo, SessionModesEvent,
    TerminalOutputEvent,
};
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

/// Panel-owned thread identity used to reopen the same ACPX session after a
/// host restart. The provider is persisted instead of inferred from list
/// position, so restoring a subset of threads cannot silently switch agents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSpec {
    pub display_name: String,
    pub provider: String,
    pub session_id: Option<String>,
    pub profile_name: Option<String>,
}

/// The resolved binding returned once a thread has opened or resumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadBinding {
    pub thread_id: String,
    pub session_id: String,
}

fn specs_for_names(thread_names: &[&str]) -> Vec<ThreadSpec> {
    thread_names
        .iter()
        .enumerate()
        .map(|(idx, name)| ThreadSpec {
            display_name: (*name).to_owned(),
            provider: provider_for_index(idx).to_owned(),
            session_id: None,
            profile_name: None,
        })
        .collect()
}

/// One UI thread's state: its live agent handle, its jsonl-backed
/// scrollback (seeded at cold start, appended to live), and the ACP
/// session id once `open_session` resolves (used to fill the trailer).
struct ThreadSlot {
    thread_id: String,
    provider: String,
    handle: Arc<AcpxThreadHandle>,
    history: Mutex<Vec<ChatMessage>>,
    acp_session_id: Mutex<Option<String>>,
    /// Phase 3 (chat-panel-production-ui/execution-plan.md): whether
    /// `history`'s current in-memory content is missing older messages
    /// still available in the jsonl cache -- set from the seeding
    /// `JsonlStore::tail()` call's own `older_available` flag, cleared
    /// once [`AgentBridge::load_older_page`] walks all the way back to
    /// the thread's real start. `false` unconditionally when there is
    /// no cache dir at all (nothing on disk to page through).
    older_available: Mutex<bool>,
    /// The 0-based index (into the thread's full ordered cached message
    /// list) of the oldest message currently loaded into `history` --
    /// what the next [`AgentBridge::load_older_page`] call passes to
    /// [`crate::jsonl_store::JsonlStore::predecessor_page`] to keep
    /// paging further back. Meaningless (always `0`) once
    /// `older_available` is `false`.
    oldest_loaded_index: Mutex<usize>,
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
    /// Phase 2 step 3 (chat-panel-production-ui/execution-plan.md):
    /// typed, merged conversation view -- `history` above stays the
    /// raw, unmerged, append-only `ChatMessage` feed (JSONL cache
    /// format, exact-count-preserving for every test/consumer that
    /// already depends on it); this is the *rendered* view real UI
    /// code should read from instead, where streamed chunks are merged
    /// by message id and tool-call status updates replace their
    /// existing row instead of appending a duplicate -- see
    /// `crate::conversation::ConversationState`'s own doc comment.
    /// Rebuilt from `history`'s full contents on every mutation via
    /// [`rebuild_transcript`] rather than maintained incrementally --
    /// see that function's doc comment for why.
    transcript: Mutex<ConversationState>,
    /// Background ACPX attachment is intentionally separate from cached
    /// transcript restoration. Commands wait for this completion signal so
    /// they cannot reach the actor before `session/new`/`session/load`.
    attachment: Mutex<AttachmentState>,
    attachment_ready: tokio::sync::Notify,
    /// Set once [`AgentBridge::close_thread`] has sent a real
    /// `session/close` for this thread. Purely a presentation flag --
    /// see that method's doc comment and this plan's Coverage Matrix
    /// `session/close`/`session/delete` row. `false` for the lifetime
    /// of every thread until a caller explicitly closes it (never set
    /// implicitly by window/process teardown).
    closed: Mutex<bool>,
    /// `thread_item_project_context` phase: the project directory this
    /// thread's session was actually opened/resumed/reattached against
    /// (the `cwd` passed to ACP at creation time -- see `cwd_for_session`),
    /// captured once and never updated afterward, since ACP has no way to
    /// move an existing session to a new cwd. `None` when no project was
    /// active at creation time (the pre-`active_project_binding` default).
    project_path: Option<PathBuf>,
}

#[derive(Default)]
struct AttachmentState {
    complete: bool,
    error: Option<String>,
}

/// One terminal's current known state, as last observed via
/// `AgentEvent::TerminalOutput`. See [`ThreadSlot::terminal_buffers`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    gateways: Arc<Mutex<std::collections::HashMap<String, Arc<acpx_client::Gateway>>>>,
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
    local_terminals:
        std::cell::RefCell<std::collections::HashMap<usize, crate::local_terminal::LocalTerminal>>,
    // `chat_sessions_project_path` phase: the active MLT project's path
    // (set from `PanelSingleton::active_project_path` via
    // `set_active_project_path`), consulted by `cwd_for_session` at every
    // new-session call site instead of the process's own cwd, once one is
    // known. `Arc<Mutex<..>>`, not a plain field, so the background
    // attachment task spawned in the constructor's loop (which runs on a
    // tokio worker thread, well past this struct's own lifetime scope at
    // spawn time) can observe updates made after construction.
    session_cwd_override: Arc<Mutex<Option<PathBuf>>>,
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
    mcp_servers: Vec<serde_json::Value>,
) -> Result<String, crate::gateway_actor::AcpxThreadError> {
    handle
        .open_session_with(cwd, profile.map(str::to_string), mcp_servers)
        .await
}

/// Recomputes `slot.transcript` from `slot.history`'s current full
/// contents -- call this after any mutation of `history` (a new
/// message pushed, live or replayed). See `ThreadSlot::transcript`'s
/// own doc comment on why this is a full rebuild rather than an
/// incremental merge.
fn refresh_transcript(slot: &ThreadSlot) {
    let history = slot.history.lock().expect("history mutex poisoned").clone();
    let rebuilt = crate::conversation::rebuild_from_chat_messages(&slot.thread_id, &history);
    *slot.transcript.lock().expect("transcript mutex poisoned") = rebuilt;
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
            *slot
                .session_modes
                .lock()
                .expect("session_modes mutex poisoned") = Some(modes.clone());
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

/// Persists interaction state independently of the transcript JSONL/trailer.
/// This is intentionally called for every request, terminal, and capability
/// transition because those state updates are sparse compared with message
/// chunks and a restart must be able to reconstruct the visible cards before
/// the gateway attachment finishes.
fn persist_runtime_snapshot(store: Option<&JsonlStore>, slot: &ThreadSlot) {
    let Some(store) = store else {
        return;
    };
    let terminal_order = slot
        .terminal_order
        .lock()
        .expect("terminal_order mutex poisoned")
        .clone();
    let terminal_buffers = slot
        .terminal_buffers
        .lock()
        .expect("terminal_buffers mutex poisoned")
        .clone();
    let snapshot = ThreadRuntimeSnapshot {
        pending_requests: slot
            .pending_requests
            .lock()
            .expect("pending_requests mutex poisoned")
            .clone(),
        terminals: terminal_order
            .into_iter()
            .filter_map(|terminal_id| {
                terminal_buffers
                    .get(&terminal_id)
                    .map(|buffer| TerminalRuntimeSnapshot {
                        terminal_id,
                        output: buffer.output.clone(),
                        truncated: buffer.truncated,
                        exit_status: buffer.exit_status,
                    })
            })
            .collect(),
        session_modes: slot
            .session_modes
            .lock()
            .expect("session_modes mutex poisoned")
            .clone(),
        config_options: slot
            .config_options
            .lock()
            .expect("config_options mutex poisoned")
            .clone(),
    };
    if let Err(error) = store.write_runtime_snapshot(&slot.thread_id, &snapshot) {
        eprintln!(
            "panel-rust: interaction snapshot persist failed for {}: {error}",
            slot.thread_id
        );
    }
}

/// Phase 3 step 2: how many of a thread's newest cached messages a
/// cold-start seed loads before requiring an explicit [`AgentBridge::
/// load_older_page`] call to see further back -- generous enough that
/// every existing test's small hand-seeded fixture (a handful of
/// messages) still loads in full within one page (unchanged test
/// behavior), while still genuinely bounding memory/IO for a real
/// long-lived thread with thousands of cached messages (see `jsonl_
/// store.rs`'s own 10,000-message test for the underlying primitive's
/// own bound proof).
const HISTORY_PAGE_SIZE: usize = 500;

/// Cold-start seeding for one thread (Phase 3 steps 1-2): loads only
/// the newest `page_size` cached messages plus the standalone trailer
/// file -- never a full-file read of a potentially large jsonl file --
/// and derives the same `cached_session_id` `load()`'s trailer field
/// used to. Returns `(seeded_messages, cached_session_id,
/// older_available, oldest_loaded_index)`, ready to populate a new
/// `ThreadSlot`. A load failure on either the tail page or the trailer
/// degrades this *one* thread to an empty seed (same "don't take down
/// every other thread's live connection over one bad cache file"
/// posture the pre-existing `load()`-based seeding always had) rather
/// than propagating a fatal `BridgeError`.
fn seed_thread_from_cache(
    store: Option<&JsonlStore>,
    thread_id: &str,
    page_size: usize,
) -> (
    Vec<ChatMessage>,
    Option<String>,
    bool,
    usize,
    ThreadRuntimeSnapshot,
) {
    let Some(store) = store else {
        return (Vec::new(), None, false, 0, ThreadRuntimeSnapshot::default());
    };
    let page = match store.tail(thread_id, page_size) {
        Ok(page) => page,
        Err(e) => {
            eprintln!(
                "panel-rust: jsonl cache tail load failed for thread {thread_id:?} ({e}); starting this thread with empty history rather than failing the whole bridge"
            );
            return (Vec::new(), None, false, 0, ThreadRuntimeSnapshot::default());
        }
    };
    let cached_session_id = match store.trailer(thread_id) {
        Ok(trailer) => trailer
            .as_ref()
            .map(|t| t.acp_session_id.trim())
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        Err(e) => {
            eprintln!(
                "panel-rust: jsonl trailer load failed for thread {thread_id:?} ({e}); treating as no prior session"
            );
            None
        }
    };
    let runtime_snapshot = match store.runtime_snapshot(thread_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!(
                "panel-rust: interaction snapshot load failed for thread {thread_id:?} ({error}); restoring transcript only"
            );
            ThreadRuntimeSnapshot::default()
        }
    };
    (
        page.messages,
        cached_session_id,
        page.older_available,
        page.oldest_loaded_index,
        runtime_snapshot,
    )
}

/// Compares a local cache trailer with metadata from the backend-selected
/// `session/list`. A failed/unsupported list is deliberately non-fatal:
/// reattachment remains available and the next successful reconciliation can
/// still perform a full load. A successful selector list that omits the
/// persisted session, or a listed session with no local trailer, is stale by
/// definition and must use `session/load`.
fn remote_cache_is_stale(
    store: Option<&JsonlStore>,
    thread_id: &str,
    session_id: &str,
    remote_sessions: Option<&[crate::gateway_actor::RemoteThreadInfo]>,
) -> bool {
    let Some(remote_sessions) = remote_sessions else {
        return false;
    };
    let Some(remote) = remote_sessions
        .iter()
        .find(|session| session.acp_session_id == session_id)
    else {
        return true;
    };
    let local = store.and_then(|store| match store.trailer(thread_id) {
        Ok(trailer) => trailer,
        Err(error) => {
            eprintln!(
                "panel-rust: unable to read transcript trailer for {thread_id:?} during reconciliation: {error}"
            );
            None
        }
    });
    JsonlStore::is_stale(local.as_ref(), &remote.title, &remote.updated_at)
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

/// Resolves the `skills-mcp-server` binary path (`skill_injection_
/// verification` phase): `RUI_SKILLS_MCP_SERVER_BIN` env override, else a
/// path relative to this crate's own `CARGO_MANIFEST_DIR`, same
/// convention as [`resolve_acpx_server_bin`].
fn resolve_skills_mcp_server_bin_from(
    override_bin: Option<&str>,
    current_exe: Option<&Path>,
    manifest_dir: &Path,
) -> PathBuf {
    if let Some(bin) = override_bin.filter(|bin| !bin.is_empty()) {
        return PathBuf::from(bin);
    }
    if let Some(parent) = current_exe.and_then(Path::parent) {
        let candidate = parent.join("skills-mcp-server");
        if candidate.is_file() {
            return candidate;
        }
    }
    manifest_dir.join("target/debug/skills-mcp-server")
}

fn resolve_skills_mcp_server_bin() -> PathBuf {
    resolve_skills_mcp_server_bin_from(
        std::env::var("RUI_SKILLS_MCP_SERVER_BIN").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Builds the `mcpServers` array `session/new`/`session/load` now send
/// (previously always `[]`, see `gateway_actor::thread_actor`'s doc
/// comments on `Command::OpenSession`/`Command::ResumeSession`) -- one
/// entry pointing at `skills-mcp-server`, always present regardless of
/// which ACPX profile (if any) the session uses. `project_path` is the
/// active MLT project's *file* path (`PanelSingleton::active_project_path`
/// as threaded through `AgentBridge::session_cwd_override`) -- passed
/// through as-is; `skills-mcp-server` itself derives the project's
/// `.skills/` directory from its parent, same as `refresh_skills_model`
/// (lib.rs) already does.
fn skills_mcp_servers_entry(
    project_path: Option<&std::path::Path>,
    provider: &str,
) -> Vec<serde_json::Value> {
    let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
    let mut args = vec![
        "--global-dir".to_string(),
        global_dir.to_string_lossy().into_owned(),
    ];
    if let Some(project_path) = project_path.and_then(|p| p.parent()) {
        args.push("--project-dir".to_string());
        args.push(project_path.to_string_lossy().into_owned());
    }
    let mut entries = vec![serde_json::json!({
        "name": "skills",
        "command": resolve_skills_mcp_server_bin().to_string_lossy(),
        "args": args,
    })];
    entries.extend(snapshotd_mcp_server_entry(provider));
    entries
}

/// snapshotd's video/media-editing MCP surface (`project.*`/`edit.*`/
/// `sap.call`/etc, see `snapshotd/internal/mcpadapter`) is served over SSE
/// by `snapshotd serve`, on by default -- but nothing ever added it to the
/// `mcpServers` array `session/new`/`session/load` send, unlike the
/// `skills` stdio server above. Found live: a real running `snapshotd
/// serve` instance's MCP SSE listener answered a probe correctly, yet no
/// chat session ever advertised it to the backend agent at all -- a
/// genuine "MCP server not made available by default" gap, not just an
/// auth or process-wiring issue. `SNAPSHOTD_MCP_SSE_ADDR` mirrors
/// `snapshotd/internal/config`'s own env var for where that listener
/// binds (default `127.0.0.1:7777`); only included when a real listener
/// actually answers there, so this stays silent instead of advertising a
/// dead MCP server when no snapshotd daemon is running at all.
///
/// **`"type": "http"`, not `"sse"`, even though the URL is the same `/sse`
/// endpoint** -- found live, correcting this function's own first draft:
/// an `sse`-typed entry made real `codex-acp` reject `session/new`
/// outright (`Invalid request: Codex doesn't support MCP SSE transport
/// protocol`; its own advertised `mcpCapabilities` are `{http: true, sse:
/// false}`). Re-sending the *identical* URL as `"type": "http"` instead
/// was accepted by both real adapters and confirmed live end-to-end on
/// both: a real `session/prompt` on each backend discovered the
/// `snapshotd` tools and completed a real tool call (`daemon_listProjects`
/// on Codex, an attempted `daemon_list` on Claude) -- codex-acp's
/// "http" transport client evidently tolerates this server's classic
/// SSE-stream response shape in practice, so one entry shape now covers
/// both providers instead of needing a provider-gated split.
fn snapshotd_mcp_server_entry(provider: &str) -> Vec<serde_json::Value> {
    let _ = provider; // kept for call-site symmetry / future per-provider gating if a real incompatibility turns up.
    let addr = std::env::var("SNAPSHOTD_MCP_SSE_ADDR").unwrap_or_else(|_| "127.0.0.1:7777".to_string());
    if !probe_http_endpoint(&addr, "/sse") {
        return Vec::new();
    }
    vec![serde_json::json!({
        "type": "http",
        "name": "snapshotd",
        "url": format!("http://{addr}/sse"),
        "headers": [],
    })]
}

/// One-shot "is anything answering here at all" probe -- deliberately not
/// a full protocol round trip like `probe_acpx_gateway_once` (an SSE
/// stream never sends a normal HTTP response body to read), just enough
/// to avoid advertising a dead MCP server to every new session when no
/// snapshotd daemon happens to be running on this machine.
fn probe_http_endpoint(addr: &str, path: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(socket_addr) = addr.parse::<std::net::SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_millis(300))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    // An SSE endpoint streams indefinitely -- a short-timeout partial read
    // that returns *some* bytes (the start of the HTTP/SSE response) is
    // already proof of life; a connect-but-nothing-ever-sent case (or
    // connection refused/reset) is treated as "not a real listener".
    matches!(stream.read(&mut buf), Ok(n) if n > 0)
}

/// Resolves the mock backend agent binary the locally-spawned gateway
/// should proxy to: `RUI_ACP_AGENT_CMD` env override (a real
/// ACP-compliant agent binary/command) if set, else the dev-checkout
/// `rui-mock-agent` binary this crate itself builds (`src/bin/
/// mock_agent.rs`, ported directly from the former `rui-acp-client`
/// crate's own `[[bin]]` of the same name -- Phase 2, chat-panel-
/// production-ui/execution-plan.md) *only if `RUI_USE_DEV_MOCK_AGENT=1`
/// is also set* -- the acpx-gateway's own default backend for dev/test.
///
/// Returns `None` (previously always returned a `String`, unconditionally)
/// when neither applies -- a real production install: no operator has set
/// `RUI_ACP_AGENT_CMD`, and `<CARGO_MANIFEST_DIR>/target/debug/
/// rui-mock-agent` is a compile-time-baked dev-checkout path that doesn't
/// exist on an end user's machine at all. The old code returned that
/// nonexistent path anyway, and [`spawn_gateway_process`] set
/// `ACPX_BACKEND_CMD` to it *unconditionally* -- so a real release install
/// with no operator-started acpx-server never reached a real agent on this
/// autospawn path at all: acpx-server would try to exec a garbage path
/// instead of falling back to its own real, working default
/// (`npx -y @agentclientprotocol/codex-acp@1.1.2`, see
/// `acpx-server/src/config.rs`'s `ServerConfig::from_env`). Found via
/// `/verify-impl`'s production-build lens (this session); see
/// designa-v2-plan-order.meta.json's skill_injection_verification/
/// runtime_and_edge_pass `verified[]` entries for the original finding.
///
/// **Second real bug this closes** (found live against a real running
/// instance, not just by re-reading the code): checking only
/// `dev_mock_agent.is_file()` is not actually a "is this a dev/test
/// context" signal -- that debug binary exists on disk in ANY checkout
/// that has ever run `cargo build`/`cargo test` once, including a
/// checkout being used specifically to verify real end-to-end acpx
/// behavior. That meant every new (non-resumed) chat session silently
/// talked to the mock agent instead of the real gateway default, with no
/// way to tell short of reading source -- exactly the "new sessions don't
/// go through real acpx" symptom observed live. Now gated behind an
/// explicit `RUI_USE_DEV_MOCK_AGENT=1` opt-in so the file's mere existence
/// is never sufficient by itself.
///
/// `RUI_TEST_MODE=1` is the single source of truth gating this entire
/// function: neither `RUI_ACP_AGENT_CMD` nor `RUI_USE_DEV_MOCK_AGENT` has
/// any effect without it, even if set. Found live: an ordinary interactive
/// dev launch (a person running `./snapflow` directly, not a test harness)
/// has no reason to ever route a real chat session to a mock/overridden
/// backend, and a leaked/inherited env var from an unrelated shell or CI
/// context was enough to silently do exactly that with no error, no log,
/// and no way to tell short of reading source -- the same class of bug as
/// the dev_mock_agent-existence issue above, just one layer up. Automated
/// tests and explicit local mock-agent workflows must set `RUI_TEST_MODE=1`
/// themselves; a plain dev/production launch must never need to.
fn resolve_backend_agent_command() -> Option<String> {
    if std::env::var("RUI_TEST_MODE").as_deref() != Ok("1") {
        return None;
    }
    if let Ok(cmd) = std::env::var("RUI_ACP_AGENT_CMD") {
        return Some(cmd);
    }
    if std::env::var("RUI_USE_DEV_MOCK_AGENT").as_deref() != Ok("1") {
        return None;
    }
    let dev_mock_agent = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug/rui-mock-agent");
    if dev_mock_agent.is_file() {
        return Some(dev_mock_agent.to_string_lossy().into_owned());
    }
    None
}

/// `ServerConfig::from_env`'s own `ACPX_BACKEND_CMD` fallback
/// (`acpx-server/src/config.rs`) is unconditionally `codex-acp` -- it has
/// no notion of `provider`/`ACPX_DEFAULT_AGENT_ID` at all, that field is
/// display-only. So when [`resolve_backend_agent_command`] found no
/// explicit override, a "claude" persona gateway was *still* silently
/// spawning the real `codex-acp` adapter as its backend: found live
/// (chat-panel-live-fixes.md phase 4) via a running instance's actual
/// spawned child-process list, not by re-reading the code. Mirrors the
/// per-provider default `acpx/scripts/openhands-acpx-claude.sh` already
/// documents and tests for exactly this integration point.
fn default_backend_command_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("npx -y @agentclientprotocol/claude-agent-acp@0.58.1"),
        // "codex" (and anything else) already matches acpx-server's own
        // built-in default -- nothing to override.
        _ => None,
    }
}

/// Reads `CODEX_API_KEY` out of the Codex CLI's own on-disk login
/// (`~/.codex/auth.json`, overrideable via `ACPX_CODEX_AUTH_FILE`), the
/// same recipe `acpx/scripts/openhands-acpx-codex.sh` already uses (there
/// via `jq`) to give the real `codex-acp` adapter noninteractive
/// `api-key` auth instead of its `chat-gpt` device-login flow, which does
/// not complete headlessly (see `acpx/TEST_REPORT.md`'s documented
/// limitation) -- exactly the `-32000: backend requires authentication`
/// error this closes for a system that already has `codex login`
/// completed. Returns `None` on any missing file/field/parse error so the
/// caller can fall back to whatever `codex-acp` does with no key (still
/// better than a hard failure at gateway-spawn time).
fn read_codex_api_key_from_auth_file() -> Option<String> {
    let path = std::env::var_os("ACPX_CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| codex_home_dir().map(|dir| dir.join("auth.json")))?;
    let contents = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Resolves the real Codex CLI's own `.codex` directory (holding
/// `auth.json` and `config.toml`) -- shared by
/// read_codex_api_key_from_auth_file, read_codex_model_provider_from_config,
/// and spawn_gateway_process's own `CODEX_HOME` wiring. Prefers
/// `ACPX_CODEX_AUTH_FILE`'s parent (set by snapshotd's procmgr.Launch to
/// the real, unsandboxed user's `~/.codex/auth.json` when this process is
/// running inside a sandboxed per-project HOME -- see that Go code's own
/// doc comment) over `$HOME/.codex`, since `$HOME` itself is exactly what's
/// sandboxed and wrong in that case.
fn codex_home_dir() -> Option<PathBuf> {
    std::env::var_os("ACPX_CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .or_else(|| dirs_home().map(|home| home.join(".codex")))
}

/// Builds (or reuses) a project-scoped `.codex` directory containing only
/// symlinks to the real `auth.json`/`config.toml` from
/// [`codex_home_dir`], and returns its path -- this, not the real
/// `.codex` directory itself, is what gets handed to the child process as
/// `CODEX_HOME`.
///
/// **Real live bug this closes.** Pointing `CODEX_HOME` straight at the
/// real `~/.codex` (the previous fix, made to solve Bifrost auth) also
/// hands the bundled Codex engine the real, unsandboxed `~/.codex/sessions/`
/// directory -- found live: a fresh per-project gateway's own `session/list`
/// call (`acpx-core/src/router.rs`'s `dispatch_session_list_real`) forwards
/// straight to that engine, which happily reported the real user's entire
/// personal session history (1200+ rollout files spanning every project
/// ever worked in on this host, not just this one), and acpx-server
/// auto-imported every single one as a "discovered" gateway session
/// (`translate_or_register_backend_session`) until instantly hitting
/// `max_sessions_per_tenant` -- confirmed by every row in a *freshly
/// deleted and recreated* per-project session db carrying a `created_at`
/// within microseconds of the gateway's own process start, not spread
/// across real usage. Deleting the local db and relaunching only
/// re-triggered the same import from the real, untouched `~/.codex/sessions`.
///
/// The fix mirrors this whole file's existing sandboxing philosophy (see
/// `procmgr.go`'s `qtHomeDir` doc comment for the same idea applied to the
/// Qt process's `$HOME`): give the engine its own project-scoped `.codex`
/// with an empty `sessions/`, so its `session/list` genuinely starts empty
/// for a fresh project, while still symlinking in just the two files
/// (`auth.json`, `config.toml`) actually needed for Bifrost auth to keep
/// working. Symlinks (not copies) so a real, external `codex login`
/// refreshing `auth.json` is still picked up without any resync step.
fn sandboxed_codex_home(cache_dir: &PathBuf) -> Option<PathBuf> {
    let real_home = codex_home_dir()?;
    let sandboxed = cache_dir.join("codex-home");
    std::fs::create_dir_all(&sandboxed).ok()?;
    for name in ["auth.json", "config.toml"] {
        let link = sandboxed.join(name);
        if link.exists() || link.symlink_metadata().is_ok() {
            continue;
        }
        let target = real_home.join(name);
        if !target.exists() {
            continue;
        }
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&target, &link);
        #[cfg(not(unix))]
        let _ = std::fs::copy(&target, &link);
    }
    Some(sandboxed)
}

/// Reads the top-level `model_provider = "..."` key out of the Codex
/// CLI's own `~/.codex/config.toml`, so codex-acp is told to use whatever
/// custom model provider (e.g. an internal proxy/gateway) this system's
/// real `codex` CLI is already configured for -- found live, not assumed:
/// this system's stored `CODEX_API_KEY` (from auth.json) is a
/// provider-specific token, not a raw OpenAI secret key, and codex-acp
/// defaults to calling `https://api.openai.com` directly when
/// MODEL_PROVIDER is unset, which genuinely rejects that token with a
/// real 401 from OpenAI's own API ("invalid_api_key") -- the key was
/// never invalid, it just was never meant to be used against that
/// endpoint. codex-acp's own MODEL_PROVIDER runtime option (its README:
/// "model provider to pass to Codex for new sessions") routes through the
/// bundled real Codex engine's own `[model_providers.<name>]` config
/// table instead, the same one the real `codex` CLI already uses
/// successfully with this exact key. Minimal line-based TOML parse
/// (stops at the first `[table]` header, i.e. before any nested table
/// could shadow a same-named top-level key) rather than pulling in a full
/// TOML parser dependency for one scalar field.
fn read_codex_model_provider_from_config() -> Option<String> {
    let contents = std::fs::read_to_string(codex_home_dir()?.join("config.toml")).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            break;
        }
        let Some(rest) = trimmed.strip_prefix("model_provider") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        let value = value.strip_prefix('"').unwrap_or(value);
        let value = value.split('"').next().unwrap_or("").trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
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
    let request = if expected_agent.is_some() {
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
        // acpx-server's `/health` handler now reports a `defaultAgentId`
        // field alongside `status` (see acpx-server/src/transport/http.rs),
        // so we can actually verify provider identity instead of treating
        // any "ready" gateway as reusable regardless of which provider was
        // requested.
        //
        // `defaultAgentId == "default"` (acpx-server's own compiled-in
        // default, unless `ACPX_DEFAULT_AGENT_ID` overrides it -- see
        // `acpx-server/src/config.rs`) means the gateway was never told
        // it's provider-specific: this is exactly the shape of
        // snapshotd's bundled gateway (`AcpxEnabled`, see
        // `provision_gateway`'s doc comment), which fronts one real
        // backend shared across every provider rather than one gateway
        // per provider. Rejecting that as a mismatch just because its id
        // says "default" instead of "codex"/"claude" was the actual bug:
        // it silently fell through to auto-spawning a second, separate
        // `acpx-server`, which then failed outright on any checkout that
        // hasn't built its own local acpx binary (this worktree
        // included) instead of just reusing the perfectly good shared
        // gateway that was already answering.
        matches!(
            envelope.get("status").and_then(|s| s.as_str()),
            Some("ready") | Some("recovering")
        ) && envelope
            .get("defaultAgentId")
            .and_then(|id| id.as_str())
            .is_some_and(|id| id == expected_agent || id == "default")
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
///
///    **This is also where `snapshotd`'s own bundled gateway lands**:
///    `snapshotd`'s `AcpxEnabled` defaults ON whenever an `acpx-server`
///    binary is discoverable (`SNAPSHOTD_ACPX_ENABLED` unset -- see
///    `snapshotd/internal/config/config.go`), bound to this exact same
///    default port 8790, and its own `AcpxBackendCmd` defaults to
///    *empty*, which means the bundled `acpx-server` picks its own
///    real, auth-requiring backend -- **not** a mock. So on a machine
///    where snapshotd is running normally, step 2 above already reuses
///    a real, production-backed gateway with zero extra configuration;
///    there is no separate "production mode" switch to flip. Do not
///    hand-launch a second ad hoc `acpx-server` (e.g. with
///    `ACPX_BACKEND_CMD` forced to `rui-mock-agent`, the dev/test
///    default below) for manual/live verification just because this is
///    real-feeling infra -- that only shadows the real one and makes
///    every thread look like it's talking to a fake backend. Only set
///    `RUI_ACPX_<PROVIDER>_URL`/spawn a throwaway mock gateway for
///    isolated automated tests (see `keyboard_shortcut_tests`'s
///    `TestPanel`), never as a substitute for snapshotd's already-real
///    default.
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
    // Shared snapshotd-owned gateway (default bind): prefer env
    // RUI_ACPX_DEFAULT_URL when set for all providers.
    if let Ok(url) = std::env::var("RUI_ACPX_DEFAULT_URL") {
        return Ok(url);
    }

    let default_port: u16 = if provider == "codex" { 8790 } else { 8791 };
    if probe_acpx_gateway_for_agent(default_port, Some(provider)) {
        return Ok(format!("http://127.0.0.1:{default_port}"));
    }
    // Healthy gateway on default codex port may still serve both providers
    // when snapshotd bundles one acpx-server — reuse if any acpx answers.
    // Any acpx on the shared default port (snapshotd single gateway).
    if provider != "codex" && probe_acpx_gateway_once(default_port, None) {
        return Ok(format!("http://127.0.0.1:{default_port}"));
    }

    // When snapshotd (or an operator) owns acpx, do not auto-spawn a second
    // gateway. RUI_ACPX_NO_AUTOSPAWN=1 or SNAPSHOTD_ACPX_ENABLED=1 with a
    // healthy URL already handled above; if neither env URL nor probe hit,
    // fail closed rather than fork a competing process.
    let no_autospawn = std::env::var_os("RUI_ACPX_NO_AUTOSPAWN").is_some()
        || std::env::var("SNAPSHOTD_ACPX_ENABLED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    if no_autospawn {
        return Err(format!(
            "no acpx gateway for {provider} at env URL or :{default_port}; \
             auto-spawn disabled (RUI_ACPX_NO_AUTOSPAWN / SNAPSHOTD_ACPX_ENABLED)"
        ));
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
        .env("ACPX_DEFAULT_AGENT_ID", provider)
        .env("RUI_MOCK_AGENT_PERSONA", provider)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Only set ACPX_BACKEND_CMD when we have a real command to point it
    // at (an explicit override, or a dev-checkout mock binary confirmed
    // to actually exist) -- leaving it unset lets acpx-server fall back
    // to its own real, working default (a genuine LLM-backed ACP adapter)
    // instead of being pointed at a nonexistent path. See
    // resolve_backend_agent_command's doc comment for the full story.
    if let Some(backend_cmd) = resolve_backend_agent_command() {
        cmd.env("ACPX_BACKEND_CMD", backend_cmd);
    } else if let Some(backend_cmd) = default_backend_command_for_provider(provider) {
        // No explicit override: acpx-server's own built-in default is
        // codex-only (see default_backend_command_for_provider's doc
        // comment), so a non-codex provider needs its real adapter
        // spelled out here instead of silently getting codex-acp anyway.
        cmd.env("ACPX_BACKEND_CMD", backend_cmd);
    }
    // Independent of which ACPX_BACKEND_CMD branch above fired --
    // found live via /verify-impl-style subagent review: this used to be
    // an else-if off the branches above, so an explicit RUI_ACP_AGENT_CMD/
    // RUI_USE_DEV_MOCK_AGENT override (still legitimately codex-acp
    // underneath, e.g. an operator pinning a specific npx version) would
    // silently skip this auth wiring and reintroduce the original
    // -32000 auth error with no diagnostic. A real backend-command
    // override for a non-codex adapter is harmless to check here too --
    // the provider == "codex" guard keeps it a no-op in that case.
    if provider == "codex" {
        // acpx-server's own default already resolves to the real
        // codex-acp adapter; give it a noninteractive path to this
        // system's already-authenticated Codex CLI login instead of
        // codex-acp's headless-incapable chat-gpt device flow (see
        // read_codex_api_key_from_auth_file's doc comment).
        if std::env::var_os("ACPX_NATIVE_AUTH_METHOD_ID").is_none() {
            cmd.env("ACPX_NATIVE_AUTH_METHOD_ID", "api-key");
        }
        if std::env::var_os("CODEX_API_KEY").is_none() {
            if let Some(key) = read_codex_api_key_from_auth_file() {
                cmd.env("CODEX_API_KEY", key);
            }
        }
        // See read_codex_model_provider_from_config's own doc comment:
        // this system's stored Codex API key is only valid against the
        // custom model provider (e.g. an internal proxy) the real codex
        // CLI is already configured for, not OpenAI's own API directly --
        // codex-acp defaults to the latter unless told otherwise.
        if std::env::var_os("MODEL_PROVIDER").is_none() {
            if let Some(provider) = read_codex_model_provider_from_config() {
                cmd.env("MODEL_PROVIDER", provider);
            }
        }
        // MODEL_PROVIDER names a provider (e.g. "bifrost"); the actual
        // [model_providers.bifrost] table (base_url, wire_api, etc.) still
        // has to be resolved from a real config.toml somewhere -- found
        // live: the bundled Codex engine reads $CODEX_HOME/config.toml
        // (default $HOME/.codex), which is this launch's *sandboxed*
        // $HOME, so it has none of that and fails with "Model provider
        // `bifrost` not found" even with MODEL_PROVIDER correctly set.
        // CODEX_HOME (a real, documented override the bundled `codex`
        // engine itself supports -- see its own `--help`) redirects that
        // lookup -- but *not* to the real ~/.codex directly (see
        // sandboxed_codex_home's doc comment for the real session-history
        // leak that caused live), only to a project-scoped mirror
        // containing just the auth/config files.
        if std::env::var_os("CODEX_HOME").is_none() {
            let sandboxed_home = cache_dir
                .and_then(sandboxed_codex_home)
                .or_else(codex_home_dir);
            if let Some(dir) = sandboxed_home {
                cmd.env("CODEX_HOME", dir);
            }
        }
    }
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

/// The `cwd` argument ACP's `session/new` wants. `chat_sessions_project_path`
/// phase: prefers the active MLT project's path (see
/// `AgentBridge::set_active_project_path`) when one is known, since that's
/// the directory a skill/session should actually be scoped to; falls back
/// to the process's own working directory (with `.` as a last resort) when
/// no project is open, matching this function's pre-existing behavior.
fn cwd_for_session(session_cwd_override: &Mutex<Option<PathBuf>>) -> PathBuf {
    session_cwd_override
        .lock()
        .expect("session cwd override mutex poisoned")
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
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

async fn wait_for_attachment(slot: &ThreadSlot) -> Result<(), String> {
    loop {
        let notified = slot.attachment_ready.notified();
        {
            let state = slot.attachment.lock().expect("attachment mutex poisoned");
            if state.complete {
                return state.error.clone().map_or(Ok(()), Err);
            }
        }
        notified.await;
    }
}

fn complete_attachment(slot: &ThreadSlot, error: Option<String>) {
    if std::env::var_os("RUI_PANEL_INPUT_TRACE").is_some() {
        eprintln!(
            "panel-rust attachment: thread={} session={:?} error={error:?}",
            slot.thread_id,
            slot.acp_session_id
                .lock()
                .expect("acp_session_id mutex poisoned")
                .as_deref()
        );
    }
    {
        let mut state = slot.attachment.lock().expect("attachment mutex poisoned");
        state.complete = true;
        state.error = error;
    }
    slot.attachment_ready.notify_waiters();
}

fn spawn_event_forwarder(
    runtime: &tokio::runtime::Handle,
    mut events_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    events_out: Arc<Mutex<VecDeque<BridgeEvent>>>,
    store_for_task: Option<JsonlStore>,
    slot_for_task: Arc<ThreadSlot>,
    idx: usize,
) {
    runtime.spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            match &ev {
                AgentEvent::Message(msg) => {
                    slot_for_task
                        .history
                        .lock()
                        .expect("history mutex poisoned")
                        .push(msg.clone());
                    refresh_transcript(&slot_for_task);
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
                    persist_thread_snapshot(store_for_task.as_ref(), &slot_for_task, now_token());
                    slot_for_task
                        .transcript
                        .lock()
                        .expect("transcript mutex poisoned")
                        .mark_all_streaming_completed();
                }
                AgentEvent::Error(_) => {}
                AgentEvent::PermissionRequest(req) => {
                    slot_for_task
                        .pending_requests
                        .lock()
                        .expect("pending_requests mutex poisoned")
                        .push(req.clone());
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
                }
                AgentEvent::TerminalOutput(term_ev) => {
                    store_terminal_output(&slot_for_task, term_ev);
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
                }
                AgentEvent::SessionModes(_)
                | AgentEvent::CurrentModeChanged(_)
                | AgentEvent::ConfigOptions(_) => {
                    store_capability_event(&slot_for_task, &ev);
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
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

fn spawn_background_attachment(
    runtime: &tokio::runtime::Runtime,
    slot: Arc<ThreadSlot>,
    handle: Arc<AcpxThreadHandle>,
    mut events_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    events_out: Arc<Mutex<VecDeque<BridgeEvent>>>,
    store: Option<JsonlStore>,
    idx: usize,
    requested_session_id: Option<String>,
    has_cached_transcript: bool,
    profile_name: Option<String>,
    attachment_gate: Arc<tokio::sync::Mutex<()>>,
    session_cwd_override: Arc<Mutex<Option<PathBuf>>>,
) {
    // Resolved synchronously, before the async task below, not inside it:
    // skills_mcp_servers_entry now transitively probes snapshotd's MCP
    // liveness over a real (blocking std::net::TcpStream) connection --
    // rust-audit's "blocking calls inside async fn" anti-pattern would
    // otherwise apply here, tying up a tokio worker thread (and holding
    // attachment_gate's async guard, below) for the probe's connect/read
    // timeouts. This function itself is a plain sync fn, so the blocking
    // call here is no different from provision_gateway's own pre-existing
    // synchronous network probes at construction time.
    let mcp_servers = skills_mcp_servers_entry(
        session_cwd_override
            .lock()
            .expect("session cwd override mutex poisoned")
            .as_deref(),
        &slot.provider,
    );
    runtime.spawn(async move {
        let attachment_guard = attachment_gate.lock().await;
        let cwd = cwd_for_session(&session_cwd_override);
        let result = if let Some(session_id) = requested_session_id.clone() {
            let remote_sessions = handle
                .list_sessions_for_agent(slot.provider.clone())
                .await
                .ok();
            let cache_is_stale = remote_cache_is_stale(
                store.as_ref(),
                &slot.thread_id,
                &session_id,
                remote_sessions.as_deref(),
            );
            let resume_result = if has_cached_transcript && !cache_is_stale {
                match handle.reattach_session(session_id.clone(), cwd.clone()).await {
                    Ok(()) => Ok(()),
                    Err(reattach_error) => {
                        eprintln!(
                            "panel-rust: session/resume unavailable for cached thread {:?} ({reattach_error}); falling back to session/load",
                            slot.thread_id
                        );
                        handle.resume_session(session_id.clone(), cwd.clone(), mcp_servers.clone()).await
                    }
                }
            } else {
                handle.resume_session(session_id.clone(), cwd.clone(), mcp_servers.clone()).await
            };
            match resume_result {
                Ok(()) => Ok(session_id),
                Err(resume_error) => {
                    eprintln!(
                        "panel-rust: cached acpx session resume failed for thread {:?} ({resume_error}); opening a fresh session",
                        slot.thread_id
                    );
                    open_session_maybe_profiled(&handle, cwd, profile_name.as_deref(), mcp_servers.clone()).await
                }
            }
        } else {
            open_session_maybe_profiled(&handle, cwd, profile_name.as_deref(), mcp_servers.clone()).await
        };

        match result {
            Ok(session_id) => {
                *slot
                    .acp_session_id
                    .lock()
                    .expect("acp_session_id mutex poisoned") = Some(session_id);
                persist_thread_snapshot(store.as_ref(), &slot, now_token());

                if requested_session_id.is_some() {
                    let mut cached_index = 0usize;
                    let mut replayed_any = false;
                    while let Ok(ev) = events_rx.try_recv() {
                        if let AgentEvent::Message(message) = &ev {
                            let mut history = slot.history.lock().expect("history mutex poisoned");
                            if !replay_matches_cached_position(&history, &mut cached_index, message) {
                                history.push(message.clone());
                                replayed_any = true;
                                if let Some(store) = &store {
                                    if let Err(error) = store.append(&slot.thread_id, message) {
                                        eprintln!(
                                            "panel-rust: jsonl append failed for {}: {error}",
                                            slot.thread_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if replayed_any {
                        refresh_transcript(&slot);
                    }
                }
                complete_attachment(&slot, None);
            }
            Err(error) => {
                let message = format!("open_session failed: {error}");
                complete_attachment(&slot, Some(message.clone()));
                events_out
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(message),
                });
            }
        }
        drop(attachment_guard);
        spawn_event_forwarder(
            // The current task runs inside this exact runtime; spawning with
            // the handle keeps all thread-slot plumbing explicit.
            &tokio::runtime::Handle::current(),
            events_rx,
            events_out,
            store,
            slot,
            idx,
        );
    });
}

impl AgentBridge {
    /// Production constructor: every thread's acpx gateway URL resolved
    /// (env-override-or-local-autospawn, see [`provision_gateway`]) +
    /// real (dev-checkout) cache dir.
    pub fn new(thread_names: &[&str]) -> Result<Self, BridgeError> {
        let cache_dir = resolve_cache_dir();
        let cache_dir_for_resolver = cache_dir.clone();
        let specs = specs_for_names(thread_names);
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
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
        let specs = specs_for_names(thread_names);
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            move |_provider| Ok(base_url.clone()),
            None,
        )
    }

    /// Production constructor for durable panel thread records. The caller
    /// provides each thread's persisted provider/session/profile binding;
    /// cached transcript paging still comes from the local JSONL store.
    pub fn new_with_thread_specs(thread_specs: &[ThreadSpec]) -> Result<Self, BridgeError> {
        let cache_dir = resolve_cache_dir();
        let cache_dir_for_resolver = cache_dir.clone();
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            thread_specs,
            move |provider| {
                provision_gateway(provider, Some(&cache_dir_for_resolver))
                    .map_err(BridgeError::Gateway)
            },
            Some(cache_dir),
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
        let specs = specs_for_names(thread_names);
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            resolve_gateway,
            cache_dir,
        )
    }

    fn new_with_thread_specs_and_gateway_resolver_and_cache_dir(
        thread_specs: &[ThreadSpec],
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
        let mut slots = Vec::with_capacity(thread_specs.len());

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
        for spec in thread_specs {
            let provider = spec.provider.clone();
            if !resolved_urls.contains_key(&provider) {
                resolved_urls.insert(provider.clone(), resolve_gateway(&provider)?);
            }
        }
        // Always resolve both known providers (see provider_for_index), not
        // just whichever ones happen to appear in thread_specs -- a cold
        // start with zero initial threads (an empty specs slice is valid
        // and normal, not just a test fixture) previously left gateway_urls
        // completely empty, so add_thread_with_profile_and_provider's own
        // gateway_urls lookup failed for every single new thread with no
        // existing thread ever able to bootstrap a provider into the map.
        for provider in ["codex", "claude"] {
            if !resolved_urls.contains_key(provider) {
                resolved_urls.insert(provider.to_string(), resolve_gateway(provider)?);
            }
        }

        // Gateway connection is intentionally deferred. Cached transcript and
        // interaction state below must be observable before any remote
        // handshake/session reconciliation completes.
        let gateways = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut gateway_setters: HashMap<String, Vec<AcpxThreadGatewaySetter>> = HashMap::new();
        // Pre-seed every resolved provider URL, including ones with zero
        // current threads (e.g. codex/claude on an empty cold start) --
        // the loop below spawns a Gateway::connect() task per key in this
        // map, and self.gateways only ever gets populated by that loop.
        // Without this, a provider with no initial thread never gets a
        // self.gateways entry, and any later add_thread_with_profile_
        // and_provider call for that provider falls into its own "wait
        // for a connection nothing will ever establish" 10s timeout --
        // the exact cause of a real "click + -> agent never responds"
        // bug found live via a real VNC session and reproduced by
        // add_thread_after_empty_cold_start_reaches_a_real_codex_backend.
        // An empty Vec here is fine: the connect task's per-setter loop
        // simply has nothing to iterate until a real thread's own setter
        // is added to self.gateways separately (already-connected
        // gateways are read from that map directly, not re-delivered
        // through this one-shot setter list).
        for url in resolved_urls.values() {
            gateway_setters.entry(url.clone()).or_default();
        }
        let mut attachment_gates: HashMap<String, Arc<tokio::sync::Mutex<()>>> = HashMap::new();
        let session_cwd_override: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

        // `spawn_acpx_thread_with_gateway` calls the free-function `tokio::spawn` internally,
        // which needs an active runtime context on this (calling) thread --
        // `enter()` provides that for the duration of this loop. The tasks
        // it schedules then run on the runtime's own worker threads for the
        // rest of the process's life, well past this guard's drop.
        let _guard = runtime.enter();
        for (idx, spec) in thread_specs.iter().enumerate() {
            let thread_id = slug(&spec.display_name);

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
            let (seeded, cached_session_id, older_available, oldest_loaded_index, runtime_snapshot) =
                seed_thread_from_cache(store.as_ref(), &thread_id, HISTORY_PAGE_SIZE);
            let has_cached_transcript = !seeded.is_empty();

            let provider = spec.provider.as_str();
            let base_url = resolved_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
            let (mut handle, gateway_setter) = spawn_acpx_thread_with_delayed_gateway();
            gateway_setters
                .entry(base_url.clone())
                .or_default()
                .push(gateway_setter);
            let attachment_gate = attachment_gates
                .entry(base_url.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            let events_rx = handle.take_events();
            let handle = Arc::new(handle);

            let slot = Arc::new(ThreadSlot {
                thread_id: thread_id.clone(),
                provider: spec.provider.clone(),
                handle: handle.clone(),
                transcript: Mutex::new(crate::conversation::rebuild_from_chat_messages(
                    &thread_id, &seeded,
                )),
                history: Mutex::new(seeded),
                acp_session_id: Mutex::new(None),
                older_available: Mutex::new(older_available),
                oldest_loaded_index: Mutex::new(oldest_loaded_index),
                pending_requests: Mutex::new(runtime_snapshot.pending_requests),
                terminal_buffers: Mutex::new(
                    runtime_snapshot
                        .terminals
                        .iter()
                        .map(|terminal| {
                            (
                                terminal.terminal_id.clone(),
                                TerminalBuffer {
                                    output: terminal.output.clone(),
                                    truncated: terminal.truncated,
                                    exit_status: terminal.exit_status,
                                },
                            )
                        })
                        .collect(),
                ),
                terminal_order: Mutex::new(
                    runtime_snapshot
                        .terminals
                        .iter()
                        .map(|terminal| terminal.terminal_id.clone())
                        .collect(),
                ),
                session_modes: Mutex::new(runtime_snapshot.session_modes),
                config_options: Mutex::new(runtime_snapshot.config_options),
                attachment: Mutex::new(AttachmentState::default()),
                attachment_ready: tokio::sync::Notify::new(),
                closed: Mutex::new(false),
                // No project can be active yet at construction time --
                // `session_cwd_override` was just created above, unset.
                project_path: None,
            });
            slots.push(slot.clone());

            spawn_background_attachment(
                &runtime,
                slot,
                handle,
                events_rx,
                events.clone(),
                store.clone(),
                idx,
                spec.session_id.clone().or(cached_session_id),
                has_cached_transcript,
                spec.profile_name.clone(),
                attachment_gate,
                session_cwd_override.clone(),
            );
        }
        drop(_guard);

        for (url, setters) in gateway_setters {
            let gateways = gateways.clone();
            runtime.spawn(async move {
                let gateway = Arc::new(acpx_client::Gateway::connect(url.clone()).await);
                gateways
                    .lock()
                    .expect("gateways mutex poisoned")
                    .insert(url, gateway.clone());
                for setter in setters {
                    setter.set_gateway(gateway.clone());
                }
            });
        }

        Ok(AgentBridge {
            runtime,
            slots,
            events,
            gateway_urls: resolved_urls,
            gateways,
            store,
            local_terminals: std::cell::RefCell::new(std::collections::HashMap::new()),
            session_cwd_override,
        })
    }

    /// `chat_sessions_project_path` phase: called from the FFI-driven
    /// `panel_rust_set_project_path` path whenever the active MLT project
    /// changes, so every subsequently-opened/resumed/reattached session
    /// picks up the new project directory as its `cwd`. Deliberately does
    /// NOT retroactively move already-open sessions -- ACP has no
    /// "change an existing session's cwd" operation.
    pub fn set_active_project_path(&self, path: Option<PathBuf>) {
        *self
            .session_cwd_override
            .lock()
            .expect("session cwd override mutex poisoned") = path;
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
        self.add_thread_with_profile_and_provider(name, profile, None)
    }

    /// Creates a thread using a configured provider when the caller has a
    /// compatible default-agent preference; otherwise preserves the normal
    /// stable provider rotation.
    pub fn add_thread_with_profile_and_provider(
        &mut self,
        name: &str,
        profile: Option<&str>,
        preferred_provider: Option<&str>,
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
        let provider = preferred_provider
            .filter(|provider| self.gateway_urls.contains_key(*provider))
            .unwrap_or_else(|| provider_for_index(idx));
        let base_url =
            self.gateway_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
        let (seeded, cached_session_id, older_available, oldest_loaded_index, runtime_snapshot) =
            seed_thread_from_cache(self.store.as_ref(), &thread_id, HISTORY_PAGE_SIZE);
        let has_cached_transcript = !seeded.is_empty();

        // `thread_new_loading_state` phase: `session/new`/`session/resume`
        // is a real network round trip that must never block the calling
        // (single-threaded Slint UI) thread -- this used to `self.runtime.
        // block_on` it inline, which froze the whole UI for the call's
        // duration. Mirrors the constructor's own async-attachment pattern
        // instead: hand the gateway over through the same delayed-setter
        // `spawn_acpx_thread_with_delayed_gateway` uses (so creating the
        // handle itself never waits on the gateway either), then delegate
        // the actual session resolution to `spawn_background_attachment`
        // (the exact function the constructor's own per-thread loop already
        // uses for this), which sets `attachment`/notifies waiters and
        // persists the thread record once the session id is known --
        // `sync_thread_records` (lib.rs) already polls for that and was
        // written specifically to support creation returning before
        // attachment finishes.
        let (mut handle, gateway_setter) = {
            let _guard = self.runtime.enter();
            spawn_acpx_thread_with_delayed_gateway()
        };
        match self
            .gateways
            .lock()
            .expect("gateways mutex poisoned")
            .get(&base_url)
            .cloned()
        {
            Some(gateway) => gateway_setter.set_gateway(gateway),
            None => {
                // Only reachable in the narrow window right after
                // construction, before the background `Gateway::connect`
                // task (spawned in the constructor) has resolved yet.
                let gateways = self.gateways.clone();
                self.runtime.spawn(async move {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if let Some(gateway) = gateways
                            .lock()
                            .expect("gateways mutex poisoned")
                            .get(&base_url)
                            .cloned()
                        {
                            gateway_setter.set_gateway(gateway);
                            return;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                });
            }
        }
        let events_rx = handle.take_events();
        let handle = Arc::new(handle);
        let project_path_for_slot = self
            .session_cwd_override
            .lock()
            .expect("session cwd override mutex poisoned")
            .clone();
        let slot = Arc::new(ThreadSlot {
            thread_id: thread_id.clone(),
            provider: provider.to_string(),
            handle: handle.clone(),
            transcript: Mutex::new(crate::conversation::rebuild_from_chat_messages(
                &thread_id, &seeded,
            )),
            history: Mutex::new(seeded),
            acp_session_id: Mutex::new(None),
            older_available: Mutex::new(older_available),
            oldest_loaded_index: Mutex::new(oldest_loaded_index),
            pending_requests: Mutex::new(runtime_snapshot.pending_requests),
            terminal_buffers: Mutex::new(
                runtime_snapshot
                    .terminals
                    .iter()
                    .map(|terminal| {
                        (
                            terminal.terminal_id.clone(),
                            TerminalBuffer {
                                output: terminal.output.clone(),
                                truncated: terminal.truncated,
                                exit_status: terminal.exit_status,
                            },
                        )
                    })
                    .collect(),
            ),
            terminal_order: Mutex::new(
                runtime_snapshot
                    .terminals
                    .iter()
                    .map(|terminal| terminal.terminal_id.clone())
                    .collect(),
            ),
            session_modes: Mutex::new(runtime_snapshot.session_modes),
            config_options: Mutex::new(runtime_snapshot.config_options),
            attachment: Mutex::new(AttachmentState::default()),
            attachment_ready: tokio::sync::Notify::new(),
            closed: Mutex::new(false),
            project_path: project_path_for_slot,
        });
        self.slots.push(slot.clone());

        spawn_background_attachment(
            &self.runtime,
            slot,
            handle,
            events_rx,
            self.events.clone(),
            self.store.clone(),
            idx,
            cached_session_id,
            has_cached_transcript,
            profile.map(str::to_string),
            Arc::new(tokio::sync::Mutex::new(())),
            self.session_cwd_override.clone(),
        );

        Ok(idx)
    }

    /// `session/list` scoped to thread `idx`'s own provider -- what a
    /// recovery/import sheet populates its choices from. Blocking, same
    /// degrade-gracefully-on-error convention as [`Self::list_profiles`]
    /// (an empty list, not a propagated error, on failure -- there is no
    /// toast/error-surface mechanism for this read-only listing call
    /// yet).
    pub fn list_remote_sessions(&self, idx: usize) -> Vec<crate::gateway_actor::RemoteThreadInfo> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        let handle = slot.handle.clone();
        let provider = slot.provider.clone();
        self.runtime
            .block_on(handle.list_sessions_for_agent(provider))
            .unwrap_or_default()
    }

    /// Same as [`Self::list_remote_sessions`], narrowed to sessions not
    /// already bound to a local thread row -- the actual recovery/import
    /// sheet's candidate list (Coverage Matrix `session/list` row:
    /// "recoverable session list"). A session id already live on some
    /// `ThreadSlot::acp_session_id` is, by definition, not something a
    /// user needs to "recover": it's already attached and visible.
    pub fn recoverable_sessions(&self, idx: usize) -> Vec<crate::gateway_actor::RemoteThreadInfo> {
        let bound: std::collections::HashSet<String> = self
            .slots
            .iter()
            .filter_map(|slot| {
                slot.acp_session_id
                    .lock()
                    .expect("acp_session_id mutex poisoned")
                    .clone()
            })
            .collect();
        self.list_remote_sessions(idx)
            .into_iter()
            .filter(|session| !bound.contains(&session.acp_session_id))
            .collect()
    }

    /// Adds a new local thread row bound to an *already-existing* remote
    /// gateway session, via `session/load` (`AcpxThreadHandle::
    /// resume_session`) -- explicitly never `session/new`, per this
    /// plan's Coverage Matrix `session/list` row ("existing session
    /// attaches without new session"). `provider` must be an already-
    /// provisioned gateway (typically the same provider the caller
    /// listed `session_id` from via [`Self::recoverable_sessions`]) --
    /// unlike [`Self::add_thread`]/[`Self::add_thread_with_profile`],
    /// this does *not* derive the provider from `provider_for_index`
    /// (a brand-new local thread has no natural index-based provider
    /// assignment here; the provider is instead exactly whichever
    /// gateway the recovered session id actually lives on).
    /// `resume_session`'s own real history replay is what populates the
    /// new thread's transcript -- proven at the actor layer already
    /// (`resume_session_replays_history_via_session_load`); this method
    /// only wires that replay into a fresh `ThreadSlot`, the same shape
    /// [`Self::add_thread_with_profile`]'s own cached-session-resume
    /// branch already establishes for a different trigger (local jsonl
    /// cache instead of a picked remote session).
    pub fn add_thread_recovering_session(
        &mut self,
        name: &str,
        provider: &str,
        session_id: &str,
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
        let base_url =
            self.gateway_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
        let gateways = self.gateways.clone();
        let gateway = self.runtime.block_on(async move {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(gateway) = gateways
                    .lock()
                    .expect("gateways mutex poisoned")
                    .get(&base_url)
                    .cloned()
                {
                    return Ok(gateway);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(BridgeError::Gateway(format!(
                        "gateway connection missing for {base_url}"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })?;

        // Deliberately does not consult the local jsonl cache for
        // `thread_id` -- this is a *new* local thread identity being
        // bound to a pre-existing *remote* session, not a reopen of a
        // thread this panel already knew about (that path is `add_
        // thread_with_profile`'s own `cached_session_id` branch).
        let mut handle = {
            let _guard = self.runtime.enter();
            spawn_acpx_thread_with_gateway(gateway)
        };
        let mut events_rx = handle.take_events();
        let handle = Arc::new(handle);
        let project_path_for_slot = self
            .session_cwd_override
            .lock()
            .expect("session cwd override mutex poisoned")
            .clone();
        let slot = Arc::new(ThreadSlot {
            thread_id: thread_id.clone(),
            provider: provider.to_string(),
            handle: handle.clone(),
            transcript: Mutex::new(crate::conversation::rebuild_from_chat_messages(
                &thread_id,
                &[],
            )),
            history: Mutex::new(Vec::new()),
            acp_session_id: Mutex::new(None),
            older_available: Mutex::new(false),
            oldest_loaded_index: Mutex::new(0),
            pending_requests: Mutex::new(Vec::new()),
            terminal_buffers: Mutex::new(HashMap::new()),
            terminal_order: Mutex::new(Vec::new()),
            session_modes: Mutex::new(None),
            config_options: Mutex::new(Vec::new()),
            attachment: Mutex::new(AttachmentState {
                complete: true,
                error: None,
            }),
            attachment_ready: tokio::sync::Notify::new(),
            closed: Mutex::new(false),
            project_path: project_path_for_slot,
        });

        let cwd = cwd_for_session(&self.session_cwd_override);
        let mcp_servers = skills_mcp_servers_entry(
            self.session_cwd_override
                .lock()
                .expect("session cwd override mutex poisoned")
                .as_deref(),
            provider,
        );
        self.runtime
            .block_on(handle.resume_session(session_id.to_string(), cwd, mcp_servers))
            .map_err(|error| BridgeError::Gateway(error.to_string()))?;
        *slot
            .acp_session_id
            .lock()
            .expect("acp_session_id mutex poisoned") = Some(session_id.to_string());
        persist_thread_snapshot(self.store.as_ref(), &slot, now_token());

        // `resume_session`'s own replayed `session/update` history has
        // already fully arrived on `events_rx` by the time the call
        // above returns (it drains to a real ACP response before
        // `AcpxThreadHandle::resume_session` resolves -- see that
        // method's own actor-loop implementation) -- drain it now into
        // this brand-new slot's `history`, same `try_recv` sweep
        // `add_thread_with_profile`'s own cached-resume branch uses,
        // before handing the receiver off to the continuous forwarder
        // for anything that arrives afterward.
        let mut replayed_any = false;
        while let Ok(event) = events_rx.try_recv() {
            if let AgentEvent::Message(message) = event {
                slot.history
                    .lock()
                    .expect("history mutex poisoned")
                    .push(message.clone());
                replayed_any = true;
                if let Some(store) = &self.store {
                    let _ = store.append(&slot.thread_id, &message);
                }
            }
        }
        if replayed_any {
            refresh_transcript(&slot);
        }

        spawn_event_forwarder(
            &self.runtime.handle().clone(),
            events_rx,
            self.events.clone(),
            self.store.clone(),
            slot.clone(),
            idx,
        );
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

    /// Presentation-safe transport state for one thread's shared gateway.
    /// HTTP has no server-request channel, so the panel must visibly explain
    /// that approval controls are unavailable instead of resembling an
    /// interactive WebSocket session.
    pub fn transport_status(&self, idx: usize) -> String {
        let Some(slot) = self.slots.get(idx) else {
            return "Unavailable".to_owned();
        };
        let Some(url) = self.gateway_urls.get(&slot.provider) else {
            return "Unavailable".to_owned();
        };
        let gateways = self.gateways.lock().expect("gateways mutex poisoned");
        match gateways.get(url).map(|gateway| gateway.mode()) {
            Some(acpx_client::TransportMode::WebSocketInteractive) => "Live connection".to_owned(),
            Some(acpx_client::TransportMode::HttpDegraded) => {
                "HTTP fallback - approvals unavailable".to_owned()
            }
            None => "Connecting...".to_owned(),
        }
    }

    /// Snapshot of a thread's full scrollback (jsonl-seeded entries plus
    /// anything streamed live since), in display order.
    pub fn history(&self, idx: usize) -> Vec<ChatMessage> {
        self.slots
            .get(idx)
            .map(|s| s.history.lock().expect("history mutex poisoned").clone())
            .unwrap_or_default()
    }

    /// The durable identity of an already-open thread, used by the panel's
    /// local SQLite state store after creation and after a resumed startup.
    pub fn thread_binding(&self, idx: usize) -> Option<ThreadBinding> {
        self.slots.get(idx).and_then(|slot| {
            slot.acp_session_id
                .lock()
                .expect("acp_session_id mutex poisoned")
                .clone()
                .map(|session_id| ThreadBinding {
                    thread_id: slot.thread_id.clone(),
                    session_id,
                })
        })
    }

    /// Provider selected for a thread at creation time. This stays separate
    /// from display ordering so a restored subset cannot be reassigned merely
    /// because a preceding thread was deleted.
    pub fn thread_provider(&self, idx: usize) -> Option<String> {
        self.slots.get(idx).map(|slot| slot.provider.clone())
    }

    /// `thread_item_project_context` phase: the project directory this
    /// thread's session was opened against (see `ThreadSlot::project_path`'s
    /// doc comment) -- `None` when no project was active at creation time,
    /// distinct from `Some("")`, which never occurs here.
    pub fn thread_project_path(&self, idx: usize) -> Option<String> {
        self.slots.get(idx).and_then(|slot| {
            slot.project_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
        })
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

    /// `profiles/create` against thread `idx`'s bound gateway. Returns
    /// `true` on success -- the caller (`lib.rs`'s settings-sheet
    /// profile-management form) is expected to re-call [`Self::
    /// list_profiles`] afterward to refresh the UI list from the
    /// gateway's own state, same "don't optimistically mutate
    /// client-side state" posture [`Self::create_mcp_server`] uses.
    pub fn create_profile(&self, idx: usize, entry: serde_json::Value) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime.block_on(handle.create_profile(entry)).is_ok()
    }

    /// `profiles/update` -- same payload shape as [`Self::create_profile`].
    pub fn update_profile(&self, idx: usize, entry: serde_json::Value) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime.block_on(handle.update_profile(entry)).is_ok()
    }

    /// `profiles/delete`.
    pub fn delete_profile(&self, idx: usize, name: &str) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.delete_profile(name.to_string()))
            .is_ok()
    }

    /// Explicit, opt-in-only `session/close` on thread `idx` -- see
    /// `AcpxThreadHandle::close_session`'s doc comment: this is never
    /// sent implicitly by window/process teardown, only by a real UI
    /// action (the sidebar's per-thread close control, guarded by its
    /// own two-step confirm). On success, marks the thread `closed`
    /// ([`Self::thread_closed`]) so the sidebar can swap its status/
    /// controls without a second round trip. Blocking, same convention
    /// as [`Self::list_profiles`]/[`Self::create_profile`] -- called
    /// synchronously from a Slint button-click handler.
    pub fn close_thread(&self, idx: usize) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        let ok = self.runtime.block_on(handle.close_session()).is_ok();
        if ok {
            *slot.closed.lock().expect("closed mutex poisoned") = true;
        }
        ok
    }

    /// Explicit, opt-in-only `session/delete` on thread `idx` -- real
    /// backend-forwarded ACP method, see `AcpxThreadHandle::
    /// delete_session`'s doc comment. The panel does not have a
    /// mechanism to remove a thread row from the sidebar's fixed-index
    /// list (see `ThreadSlot`'s own doc comment on why threads are
    /// append-only), so a deleted thread stays visible with a
    /// `"closed"` status and no further close/delete controls -- this
    /// call always also marks the thread `closed` (deleting an unclosed
    /// session still ends its lifecycle from the panel's perspective,
    /// even though a caller should ordinarily close first).
    pub fn delete_thread(&self, idx: usize) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        let ok = self.runtime.block_on(handle.delete_session()).is_ok();
        if ok {
            *slot.closed.lock().expect("closed mutex poisoned") = true;
        }
        ok
    }

    /// Whether thread `idx` has been explicitly closed via
    /// [`Self::close_thread`]/[`Self::delete_thread`]. `false` for any
    /// out-of-range index or a thread that has never been closed.
    pub fn thread_closed(&self, idx: usize) -> bool {
        self.slots
            .get(idx)
            .map(|slot| *slot.closed.lock().expect("closed mutex poisoned"))
            .unwrap_or(false)
    }

    /// `mcp_servers/list` against thread `idx`'s bound gateway -- what
    /// the settings sheet's MCP-server list populates from. Same
    /// blocking/degrade-gracefully-on-error convention as
    /// [`Self::list_profiles`].
    pub fn list_mcp_servers(&self, idx: usize) -> Vec<crate::protocol_types::McpServerEntry> {
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
        self.runtime
            .block_on(handle.create_mcp_server(entry))
            .is_ok()
    }

    /// `mcp_servers/update` -- same payload shape as [`Self::
    /// create_mcp_server`].
    pub fn update_mcp_server(&self, idx: usize, entry: serde_json::Value) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.update_mcp_server(entry))
            .is_ok()
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
    pub fn list_agents(&self, idx: usize) -> Vec<crate::protocol_types::AgentCatalogEntry> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.list_agents())
            .unwrap_or_default()
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
                eprintln!(
                    "panel-rust: local terminal write_input failed for thread {idx}: {error}"
                );
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
        persist_runtime_snapshot(self.store.as_ref(), slot);
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
        refresh_transcript(slot);
        if let Some(store) = &self.store {
            if let Err(e) = store.append(&slot.thread_id, &msg) {
                eprintln!(
                    "panel-rust: jsonl append failed for {}: {e}",
                    slot.thread_id
                );
            }
        }
    }

    /// Snapshot of a thread's *merged* transcript view (Phase 2 step 3)
    /// -- streamed chunks merged by message id, tool-call status
    /// updates replaced in place rather than duplicated. This is what
    /// UI-facing code should read from instead of [`Self::history`]'s
    /// raw per-chunk feed; see [`crate::conversation::ConversationState`]
    /// and `ThreadSlot::transcript`'s own doc comments.
    pub fn transcript(&self, idx: usize) -> Vec<crate::conversation::TranscriptItem> {
        self.slots
            .get(idx)
            .map(|s| {
                s.transcript
                    .lock()
                    .expect("transcript mutex poisoned")
                    .items()
                    .to_vec()
            })
            .unwrap_or_default()
    }

    /// `true` if thread `idx` has older cached messages beyond what is
    /// currently loaded into memory -- what a `ChatView` scroll-to-top
    /// handler checks before bothering to call [`Self::load_older_page`]
    /// at all (Phase 3 step 2).
    pub fn has_older_page(&self, idx: usize) -> bool {
        self.slots
            .get(idx)
            .map(|s| {
                *s.older_available
                    .lock()
                    .expect("older_available mutex poisoned")
            })
            .unwrap_or(false)
    }

    /// Loads the next older page of thread `idx`'s cached transcript
    /// from disk and prepends it to `history` (oldest-first order
    /// preserved -- the page's own messages are already oldest-to-
    /// newest, and they all precede everything already in `history`),
    /// then rebuilds the merged `transcript` view from the new,
    /// larger `history`. Returns `false` (a no-op) if there is no
    /// cache configured, no older page available, or the thread index
    /// is out of range -- callers should stop calling this once it
    /// returns `false` rather than needing to separately poll
    /// [`Self::has_older_page`] first (though doing so to decide
    /// whether to show a "load more" affordance at all is still
    /// correct and cheap).
    pub fn load_older_page(&self, idx: usize) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let Some(store) = &self.store else {
            return false;
        };
        if !*slot
            .older_available
            .lock()
            .expect("older_available mutex poisoned")
        {
            return false;
        }
        let before_index = *slot
            .oldest_loaded_index
            .lock()
            .expect("oldest_loaded_index mutex poisoned");
        let page = match store.predecessor_page(&slot.thread_id, before_index, HISTORY_PAGE_SIZE) {
            Ok(page) => page,
            Err(e) => {
                eprintln!(
                    "panel-rust: load_older_page failed for thread {:?}: {e}",
                    slot.thread_id
                );
                return false;
            }
        };
        if page.messages.is_empty() {
            // Nothing actually came back (e.g. the cache file shrank
            // out from under this index somehow) -- treat as exhausted
            // rather than looping forever on a caller that keeps
            // retrying.
            *slot
                .older_available
                .lock()
                .expect("older_available mutex poisoned") = false;
            return false;
        }
        {
            let mut history = slot.history.lock().expect("history mutex poisoned");
            let mut prepended = page.messages;
            prepended.extend(history.drain(..));
            *history = prepended;
        }
        *slot
            .older_available
            .lock()
            .expect("older_available mutex poisoned") = page.older_available;
        *slot
            .oldest_loaded_index
            .lock()
            .expect("oldest_loaded_index mutex poisoned") = page.oldest_loaded_index;
        refresh_transcript(slot);
        true
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
        let slot = slot.clone();
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(error) = wait_for_attachment(&slot).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
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
        let slot = slot.clone();
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(error) = wait_for_attachment(&slot).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
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
        let slot = slot.clone();
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(error) = wait_for_attachment(&slot).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
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
        let slot = slot.clone();
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(error) = wait_for_attachment(&slot).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            if let Err(e) = handle.set_config_option(config_id, value).await {
                events
                    .lock()
                    .expect("event queue mutex poisoned")
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session/set_config_option failed: {e}")),
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

    /// `RUI_ACP_AGENT_CMD` (a real override) always wins over the
    /// dev-checkout mock-agent fallback -- the production-build regression
    /// found this session was resolve_backend_agent_command silently
    /// defaulting to a nonexistent path when NEITHER applies (see its own
    /// doc comment); this specific override-wins branch is unaffected by
    /// that fix but is the one part of this function cheaply testable
    /// without touching the real dev_mock_agent build artifact every other
    /// real-process test in this file also depends on existing.
    #[test]
    fn resolve_backend_agent_command_prefers_explicit_override() {
        // SAFETY (env mutation in a test): guarded by restoring the prior
        // value unconditionally before returning, and this whole suite
        // already runs under --test-threads=1 per this crate's own
        // convention for exactly this reason (see module doc references
        // elsewhere in this file to real-process serialization).
        let prior = std::env::var("RUI_ACP_AGENT_CMD").ok();
        let prior_test_mode = std::env::var("RUI_TEST_MODE").ok();
        unsafe {
            std::env::set_var("RUI_ACP_AGENT_CMD", "/some/real/agent --flag");
            std::env::set_var("RUI_TEST_MODE", "1");
        }
        let resolved = resolve_backend_agent_command();
        match prior {
            Some(value) => unsafe { std::env::set_var("RUI_ACP_AGENT_CMD", value) },
            None => unsafe { std::env::remove_var("RUI_ACP_AGENT_CMD") },
        }
        match prior_test_mode {
            Some(value) => unsafe { std::env::set_var("RUI_TEST_MODE", value) },
            None => unsafe { std::env::remove_var("RUI_TEST_MODE") },
        }
        assert_eq!(resolved.as_deref(), Some("/some/real/agent --flag"));
    }

    /// `RUI_TEST_MODE` is the sole gate for the whole function -- an
    /// explicit `RUI_ACP_AGENT_CMD` override must be inert without it, so a
    /// leaked/inherited env var from an unrelated shell or CI context can
    /// never silently redirect a real interactive launch's chat sessions
    /// to a mock or arbitrary command.
    #[test]
    fn resolve_backend_agent_command_ignores_override_without_test_mode() {
        let prior = std::env::var("RUI_ACP_AGENT_CMD").ok();
        let prior_test_mode = std::env::var("RUI_TEST_MODE").ok();
        unsafe {
            std::env::set_var("RUI_ACP_AGENT_CMD", "/some/real/agent --flag");
            std::env::remove_var("RUI_TEST_MODE");
        }
        let resolved = resolve_backend_agent_command();
        match prior {
            Some(value) => unsafe { std::env::set_var("RUI_ACP_AGENT_CMD", value) },
            None => unsafe { std::env::remove_var("RUI_ACP_AGENT_CMD") },
        }
        match prior_test_mode {
            Some(value) => unsafe { std::env::set_var("RUI_TEST_MODE", value) },
            None => unsafe { std::env::remove_var("RUI_TEST_MODE") },
        }
        assert_eq!(resolved, None);
    }

    /// The real bug found live this session: a debug build of
    /// `rui-mock-agent` merely existing on disk (true in any dev checkout
    /// that has ever run `cargo build`/`cargo test`) must NOT by itself
    /// route new sessions to the mock agent -- only an explicit
    /// `RUI_USE_DEV_MOCK_AGENT=1` opt-in may do that, so a dev checkout
    /// used to verify real acpx behavior gets the real gateway default
    /// unless someone deliberately asks for the mock.
    #[test]
    fn resolve_backend_agent_command_ignores_mock_binary_without_explicit_opt_in() {
        let prior_override = std::env::var("RUI_ACP_AGENT_CMD").ok();
        let prior_opt_in = std::env::var("RUI_USE_DEV_MOCK_AGENT").ok();
        unsafe {
            std::env::remove_var("RUI_ACP_AGENT_CMD");
            std::env::remove_var("RUI_USE_DEV_MOCK_AGENT");
        }
        let resolved = resolve_backend_agent_command();
        match prior_override {
            Some(value) => unsafe { std::env::set_var("RUI_ACP_AGENT_CMD", value) },
            None => unsafe { std::env::remove_var("RUI_ACP_AGENT_CMD") },
        }
        match prior_opt_in {
            Some(value) => unsafe { std::env::set_var("RUI_USE_DEV_MOCK_AGENT", value) },
            None => unsafe { std::env::remove_var("RUI_USE_DEV_MOCK_AGENT") },
        }
        // Even though target/debug/rui-mock-agent genuinely exists in this
        // checkout (every other real-process test in this file depends on
        // it), resolve_backend_agent_command must still return None here.
        assert_eq!(resolved, None);
    }

    /// acpx-server's own ACPX_BACKEND_CMD default is codex-only (see this
    /// function's own doc comment) -- "claude" is the one provider that
    /// genuinely needs an explicit override; every other provider string
    /// (including "codex" itself and anything unrecognized) must return
    /// None so spawn_gateway_process leaves ACPX_BACKEND_CMD unset and
    /// falls through to that real default instead.
    #[test]
    fn default_backend_command_for_provider_only_overrides_claude() {
        assert_eq!(
            default_backend_command_for_provider("claude"),
            Some("npx -y @agentclientprotocol/claude-agent-acp@0.58.1")
        );
        assert_eq!(default_backend_command_for_provider("codex"), None);
        assert_eq!(default_backend_command_for_provider("unknown-provider"), None);
    }

    /// read_codex_api_key_from_auth_file's real, only call site
    /// (spawn_gateway_process) requires this to be correct without ever
    /// touching this developer's actual ~/.codex/auth.json -- covered via
    /// ACPX_CODEX_AUTH_FILE pointing at a disposable temp file instead.
    #[test]
    fn read_codex_api_key_from_auth_file_reads_the_configured_field() {
        let dir = std::env::temp_dir().join(format!(
            "rui-codex-auth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_file = dir.join("auth.json");
        std::fs::write(&auth_file, r#"{"OPENAI_API_KEY": "sk-test-key"}"#)
            .expect("write temp auth file");

        let prior = std::env::var("ACPX_CODEX_AUTH_FILE").ok();
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
        }
        let found = read_codex_api_key_from_auth_file();
        match prior {
            Some(value) => unsafe { std::env::set_var("ACPX_CODEX_AUTH_FILE", value) },
            None => unsafe { std::env::remove_var("ACPX_CODEX_AUTH_FILE") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(found.as_deref(), Some("sk-test-key"));
    }

    /// Missing file, malformed JSON, an empty key, or a missing field
    /// must all fall back to None (letting codex-acp run with whatever
    /// auth it can find on its own) rather than panicking or returning a
    /// bogus empty-string "key".
    #[test]
    fn read_codex_api_key_from_auth_file_is_none_on_any_bad_input() {
        let missing = std::env::temp_dir().join(format!(
            "rui-codex-auth-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let prior = std::env::var("ACPX_CODEX_AUTH_FILE").ok();
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &missing);
        }
        let missing_file_result = read_codex_api_key_from_auth_file();

        let empty_key_file = std::env::temp_dir().join(format!(
            "rui-codex-auth-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&empty_key_file, r#"{"OPENAI_API_KEY": ""}"#)
            .expect("write empty-key temp auth file");
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &empty_key_file);
        }
        let empty_key_result = read_codex_api_key_from_auth_file();

        match prior {
            Some(value) => unsafe { std::env::set_var("ACPX_CODEX_AUTH_FILE", value) },
            None => unsafe { std::env::remove_var("ACPX_CODEX_AUTH_FILE") },
        }
        let _ = std::fs::remove_file(&empty_key_file);

        assert_eq!(missing_file_result, None);
        assert_eq!(empty_key_result, None);
    }

    /// read_codex_model_provider_from_config derives the .codex directory
    /// from ACPX_CODEX_AUTH_FILE's parent (matching how spawn_gateway_process
    /// actually calls it -- both auth.json and config.toml live in the
    /// same real ~/.codex directory), and must stop scanning at the first
    /// `[table]` header so a same-named key inside e.g.
    /// [model_providers.bifrost] can never shadow the real top-level
    /// model_provider value.
    #[test]
    fn read_codex_model_provider_from_config_reads_top_level_key_only() {
        let dir = std::env::temp_dir().join(format!(
            "rui-codex-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("config.toml"),
            "model_catalog_json = \"/home/siraj/.codex/bifrost-model-catalog.json\"\n\
             model_provider = \"bifrost\"\n\
             \n\
             [model_providers.bifrost]\n\
             base_url = \"http://bifrost.localdev.com/v1\"\n\
             model_provider = \"not-this-one\"\n",
        )
        .expect("write temp config.toml");
        // config.toml lives alongside a (never-read-in-this-test) auth.json
        // at the SAME path spawn_gateway_process derives its .codex dir
        // from -- the file need not exist for this function's own lookup.
        let auth_file = dir.join("auth.json");

        let prior = std::env::var("ACPX_CODEX_AUTH_FILE").ok();
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
        }
        let found = read_codex_model_provider_from_config();
        match prior {
            Some(value) => unsafe { std::env::set_var("ACPX_CODEX_AUTH_FILE", value) },
            None => unsafe { std::env::remove_var("ACPX_CODEX_AUTH_FILE") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(found.as_deref(), Some("bifrost"));
    }

    fn mock_agent_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/rui-mock-agent")
    }

    fn wait_for_thread_ready(bridge: &AgentBridge, idx: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let state = bridge.slots[idx]
                .attachment
                .lock()
                .expect("attachment mutex poisoned");
            if state.complete {
                assert!(
                    state.error.is_none(),
                    "thread attachment failed: {:?}",
                    state.error
                );
                return;
            }
            drop(state);
            assert!(
                std::time::Instant::now() < deadline,
                "thread attachment did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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

            // acpx-server's own startup (before it even binds its listen
            // socket) does a real network fetch of the ACP registry
            // (acpx-core's ensure_registry_loaded, called from
            // warm_default_profiles at the top of main.rs), falling back
            // to a bundled snapshot on any error. That client used to have
            // no timeout at all -- fixed (acpx-core/src/router.rs) to a
            // bounded 5s -- but even the bounded case can take a bit over
            // 1.5s to fail-and-fall-back in this sandbox's network
            // conditions (measured ~1.6s directly). 3s gives real headroom
            // without materially slowing down the common fast-startup case.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
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
            Self::spawn_with_backend_cmd(&mock_agent_bin().to_string_lossy(), persona, db_path)
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
                id: None,
                raw_input: None,
                raw_output: None,
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

    /// Real, live, billed reproduction of a bug found via a real VNC
    /// interactive session, not from prior automated coverage: a cold
    /// start with zero initial threads (the DEFAULT_THREAD_NAMES opt-in
    /// fix, 830ec21) plus the gateway_urls-for-every-known-provider fix
    /// (9b5fb03) plus RUI_TEST_MODE gating (76a1e16) together are supposed
    /// to add up to "click + -> a real codex reply, no mock, no manual env
    /// juggling" -- this proves that chain end to end against this
    /// machine's own real, already-logged-in Codex CLI auth, exactly the
    /// way `panel_rust_create`'s empty-cold-start path and
    /// `on_new_thread_requested`'s click handler actually call these
    /// functions, not a synthetic shortcut.
    ///
    /// `#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1`, matching
    /// `acpx/acpx-server/tests/real_ambient_multi_agent_test.rs`'s own
    /// convention -- makes a real, billed model call using whatever
    /// account this machine's Codex CLI is logged into.
    ///
    /// Run with:
    /// ```text
    /// ACPX_LIVE_TEST_AMBIENT=1 cargo test --lib \
    ///   agent_bridge::tests::add_thread_after_empty_cold_start_reaches_a_real_codex_backend \
    ///   -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn add_thread_after_empty_cold_start_reaches_a_real_codex_backend() {
        if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
                 machine's real, already-logged-in codex CLI session (makes a real \
                 billed API call)"
            );
            return;
        }

        let cache_dir = tempfile::tempdir().expect("tempdir");
        let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_DEFAULT_AGENT_ID", "codex")
                .env("ACPX_NATIVE_AUTH_METHOD_ID", "api-key")
                .env("RUST_LOG", "error");
            // Same real-auth wiring spawn_gateway_process uses for a real
            // "codex" thread -- no ACPX_BACKEND_CMD override, so
            // acpx-server falls through to its own real default (real
            // codex-acp via npx), not a mock.
            if std::env::var_os("CODEX_API_KEY").is_none() {
                if let Some(key) = read_codex_api_key_from_auth_file() {
                    command.env("CODEX_API_KEY", key);
                }
            }
        });
        let _gateway_guard = TestGateway {
            child,
            base_url: base_url.clone(),
        };

        // Empty initial thread_specs -- the exact cold-start shape
        // panel_rust_create now produces by default (830ec21), which is
        // what exposed the gateway_urls regression this test chain fixes.
        let mut bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &[],
            move |_provider| Ok(base_url.clone()),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge with zero initial threads");

        // Exactly what on_new_thread_requested calls.
        let index = bridge
            .add_thread_with_profile_and_provider("Real codex smoke test", None, Some("codex"))
            .expect("add_thread_with_profile_and_provider must succeed against a real, \
                     correctly-configured codex gateway");

        bridge.push_local(
            index,
            ChatMessage {
                kind: MessageKind::User,
                text: "Reply with exactly the single word PANG and nothing else.".into(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
            },
        );
        bridge.send_prompt(
            index,
            "Reply with exactly the single word PANG and nothing else.".into(),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut ended = false;
        while std::time::Instant::now() < deadline && !ended {
            ended = bridge
                .poll()
                .into_iter()
                .any(|event| matches!(event.event, AgentEvent::TurnEnded(_)));
            if !ended {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        assert!(ended, "real codex-acp turn did not finish within 60s");
        let history = bridge.history(index);
        assert!(
            history
                .iter()
                .any(|message| message.text.to_uppercase().contains("PANG")),
            "expected a real reply containing PANG, got: {history:?}"
        );
    }

    /// Coverage Matrix `session/list` row: recoverable-session listing
    /// and attach-without-`session/new` -- real gateway, two genuinely
    /// independent sessions on the same provider (one bound to the
    /// bridge's own thread, one deliberately orphaned by opening it
    /// through a raw `spawn_acpx_thread` handle the bridge never knew
    /// about), proving `recoverable_sessions` excludes the bound one and
    /// includes the orphan, and that `add_thread_recovering_session`
    /// genuinely replays the orphan's own real history via `session/
    /// load` rather than starting a fresh empty session.
    #[test]
    fn recoverable_sessions_lists_the_orphan_and_attaching_it_replays_its_real_history() {
        // Persona/agent id must match `provider_for_index(0)` ("codex")
        // -- `list_sessions_for_agent` selects the backend by this exact
        // registered supervisor key (`_acpx.agentId`), unlike plain
        // `session/new` (no `_acpx.profile`), which routes to whichever
        // single backend a gateway with no profile disambiguation
        // happens to supervise regardless of the panel-side `provider`
        // label.
        let gateway = TestGateway::spawn_with_persona("codex");
        let names = ["Bound Thread"];
        let mut bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        // Seed the bridge's own thread so its bound session_id is
        // unambiguous in the recoverable list (must never appear there).
        bridge.send_prompt(0, "hello from the bound thread".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline
            && !bridge
                .poll()
                .into_iter()
                .any(|e| matches!(e.event, AgentEvent::TurnEnded(_)))
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let bound_session_id = bridge
            .thread_binding(0)
            .expect("bound thread has a session id")
            .session_id;

        // A second, genuinely orphaned session on the same provider --
        // opened through a raw handle the bridge never constructed a
        // `ThreadSlot` for, exactly the "a session this panel process
        // never itself created" scenario `session/list` recovery exists
        // for (e.g. a prior panel run, or a session opened by a
        // different client entirely).
        let orphan_session_id = {
            let helper_rt = tokio::runtime::Runtime::new().expect("helper runtime");
            let _guard = helper_rt.enter();
            let orphan = spawn_acpx_thread(gateway.base_url.clone());
            helper_rt
                .block_on(orphan.open_session(std::env::current_dir().unwrap()))
                .expect("open_session for the orphan handle")
        };

        let recoverable = bridge.recoverable_sessions(0);
        assert!(
            recoverable
                .iter()
                .any(|s| s.acp_session_id == orphan_session_id),
            "the orphan session must appear in the recoverable list: {recoverable:?}"
        );
        assert!(
            !recoverable
                .iter()
                .any(|s| s.acp_session_id == bound_session_id),
            "the already-bound thread's own session must never appear as recoverable: {recoverable:?}"
        );

        let recovered_idx = bridge
            .add_thread_recovering_session("Recovered Thread", "codex", &orphan_session_id)
            .expect("add_thread_recovering_session");
        assert_eq!(
            bridge.thread_binding(recovered_idx).map(|b| b.session_id),
            Some(orphan_session_id.clone()),
            "the recovered thread must bind to the orphan's own session id, not a new one"
        );
        // The orphan session was never prompted, so it has no history to
        // replay -- what matters here is the *attach itself* succeeded
        // via `session/load` against a real pre-existing session id
        // (proven by the session-id-binding assertion above), not that
        // there happened to be text to replay. A separate, focused
        // history-replay proof already exists at the actor layer
        // (`resume_session_replays_history_via_session_load`).
        assert!(bridge.history(recovered_idx).is_empty());
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
                    id: None,
                    raw_input: None,
                    raw_output: None,
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
    fn restored_interaction_snapshot_is_available_before_gateway_events_arrive() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let store = JsonlStore::open(cache_dir.path()).expect("cache store");
        store
            .write_runtime_snapshot(
                "thread-one",
                &ThreadRuntimeSnapshot {
                    pending_requests: vec![AgentRequestEvent {
                        relay_id: "restored-relay".into(),
                        method: "terminal/create".into(),
                        raw_request: serde_json::json!({
                            "id": 17,
                            "method": "terminal/create",
                            "params": {"command": "echo"}
                        }),
                    }],
                    terminals: vec![TerminalRuntimeSnapshot {
                        terminal_id: "restored-terminal".into(),
                        output: "restored output\n".into(),
                        truncated: true,
                        exit_status: Some((Some(9), None)),
                    }],
                    session_modes: Some(SessionModesEvent {
                        current_mode_id: "ask".into(),
                        available: vec![crate::protocol_types::SessionModeInfo {
                            id: "ask".into(),
                            name: "Ask".into(),
                            description: None,
                        }],
                    }),
                    config_options: vec![crate::protocol_types::ConfigOptionInfo {
                        id: "model".into(),
                        name: "Model".into(),
                        description: None,
                        category: None,
                        kind: "select".into(),
                        current_value: Some("fast".into()),
                        options: vec![],
                    }],
                },
            )
            .expect("seed interaction snapshot");

        let bridge = bridge_with_single_gateway(
            &["Thread One"],
            &gateway,
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge");

        assert_eq!(bridge.pending_requests(0).len(), 1);
        assert_eq!(bridge.pending_requests(0)[0].relay_id, "restored-relay");
        assert_eq!(bridge.active_terminals(0), vec!["restored-terminal"]);
        assert_eq!(
            bridge
                .terminal_buffer(0, "restored-terminal")
                .expect("restored terminal")
                .output,
            "restored output\n"
        );
        assert_eq!(
            bridge
                .session_modes(0)
                .expect("restored modes")
                .current_mode_id,
            "ask"
        );
        assert_eq!(
            bridge.config_options(0)[0].current_value.as_deref(),
            Some("fast")
        );
    }

    #[test]
    fn cached_tail_renders_and_immediate_prompt_waits_for_background_attachment() {
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let store = JsonlStore::open(cache_dir.path()).expect("cache store");
        store
            .append(
                "thread-one",
                &ChatMessage {
                    kind: MessageKind::Agent,
                    text: "cached tail".into(),
                    status: None,
                    id: Some("cached-tail".into()),
                    raw_input: None,
                    raw_output: None,
                },
            )
            .expect("seed cached tail");

        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("delayed_new.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    sleep 1
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"slow-start"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#,
        )
        .expect("write delayed backend script");
        let gateway = TestGateway::spawn_with_backend_cmd(
            &format!("sh {}", script_path.display()),
            "slow-start",
            None,
        );

        let started = std::time::Instant::now();
        let bridge = bridge_with_single_gateway(
            &["Thread One"],
            &gateway,
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(300),
            "constructor waited for delayed session attachment"
        );
        assert_eq!(bridge.history(0)[0].text, "cached tail");

        bridge.send_prompt(0, "queued at startup".into());
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
        assert!(ended, "immediate prompt was not released after attachment");
        wait_for_thread_ready(&bridge, 0);
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
            wait_for_thread_ready(&bridge, 0);
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
                    id: None,
                    raw_input: None,
                    raw_output: None,
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
        wait_for_thread_ready(&bridge, 0);
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
                id: None,
                raw_input: None,
                raw_output: None,
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
            id: None,
            raw_input: None,
            raw_output: None,
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
    fn remote_session_metadata_selects_reattach_only_for_a_matching_trailer() {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let store = JsonlStore::open(cache_dir.path()).expect("open store");
        let trailer = ThreadTrailer {
            acp_session_id: "gateway-1".into(),
            title: Some("Fix export".into()),
            updated_at: Some("2026-07-16T10:00:00Z".into()),
            message_count: 1,
        };
        store
            .overwrite(
                "thread",
                &[ChatMessage {
                    kind: MessageKind::Agent,
                    text: "cached response".into(),
                    status: None,
                    id: Some("message-1".into()),
                    raw_input: None,
                    raw_output: None,
                }],
                &trailer,
            )
            .expect("seed cache");

        let matching = vec![crate::gateway_actor::RemoteThreadInfo {
            acp_session_id: "gateway-1".into(),
            agent_id: "codex".into(),
            title: Some("Fix export".into()),
            updated_at: Some("2026-07-16T10:00:00Z".into()),
        }];
        assert!(
            !remote_cache_is_stale(Some(&store), "thread", "gateway-1", Some(&matching)),
            "matching metadata should retain the cached tail and use session/resume"
        );

        let changed = vec![crate::gateway_actor::RemoteThreadInfo {
            updated_at: Some("2026-07-16T10:01:00Z".into()),
            ..matching[0].clone()
        }];
        assert!(
            remote_cache_is_stale(Some(&store), "thread", "gateway-1", Some(&changed)),
            "changed remote metadata must choose session/load reconciliation"
        );
        assert!(
            remote_cache_is_stale(Some(&store), "thread", "gateway-1", Some(&[])),
            "a successful selector result that omits the cached session must recover it"
        );
    }

    #[test]
    fn replay_matching_skips_cached_user_messages_without_duplicate_agent_updates() {
        let user = ChatMessage {
            kind: MessageKind::User,
            text: "same answer".into(),
            status: None,
            id: None,
            raw_input: None,
            raw_output: None,
        };
        let agent = ChatMessage {
            kind: MessageKind::Agent,
            text: "same answer".into(),
            status: None,
            id: None,
            raw_input: None,
            raw_output: None,
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
        wait_for_thread_ready(&bridge, 0);

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
            wait_for_thread_ready(&bridge, 0);
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
                id: None,
                raw_input: None,
                raw_output: None,
            },
        );
        bridge.push_local(
            1,
            ChatMessage {
                kind: MessageKind::User,
                text: "b-only".into(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
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
                id: None,
                raw_input: None,
                raw_output: None,
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

    /// Regression guard: a gateway whose `defaultAgentId` is still
    /// `"default"` (acpx-server's own compiled-in default, unmodified --
    /// exactly the shape of snapshotd's bundled gateway, which is shared
    /// across every provider rather than spun up per-provider) must be
    /// treated as reusable for *any* requested provider, not rejected as
    /// an identity mismatch. Before this fix, `provision_gateway` would
    /// silently ignore a perfectly good already-running shared gateway
    /// and fall through to auto-spawning a second one, which then failed
    /// outright wherever a local acpx binary hadn't been built.
    #[test]
    fn probe_acpx_gateway_treats_a_default_agent_id_as_matching_any_provider() {
        let gateway = TestGateway::spawn_with_persona("default");
        let port: u16 = gateway
            .base_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("parse port from base_url");
        assert!(probe_acpx_gateway_for_agent(port, Some("codex")));
        assert!(probe_acpx_gateway_for_agent(port, Some("claude")));
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
                id: None,
                raw_input: None,
                raw_output: None,
            },
            ChatMessage {
                kind: MessageKind::Thinking,
                text: "considering the timeline structure".into(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
            },
            ChatMessage {
                kind: MessageKind::ToolCall,
                text: "edit.add_transition(...)".into(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
            },
            ChatMessage {
                kind: MessageKind::Agent,
                text: "done, crossfade added".into(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
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
                    id: None,
                    raw_input: None,
                    raw_output: None,
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
        assert!(
            ended,
            "prompt turn did not finish after answering the relay"
        );

        let history = bridge.history(0);
        assert!(
            history.iter().any(|m| m.text.contains("CHOSE: allow-once")),
            "expected the backend's own echo to reflect the live-relayed \
           allow-once answer, not the profile's AutoReject default \
          (which would have picked reject-once): got {history:?}"
        );
    }

    /// Coverage Matrix `initialize`/connection-state row: proves
    /// `transport_status` reports the live-WS state against a real
    /// gateway, not merely that the constructor call returns `Ok`.
    /// `new_with_gateway_resolver_and_cache_dir` does **not** block on
    /// the shared per-provider `Gateway::connect()` task (only later
    /// command calls do, via `wait_for_attachment` -- see `AgentBridge`'s
    /// own `attachment`/`attachment_ready` doc comments), so this test
    /// polls with a bounded deadline rather than asserting on the very
    /// first read; once it settles, a real ACPX `initialize` round trip
    /// over a real WebSocket has genuinely completed -- this is the
    /// direct, observable proof of that, not an inferred one.
    ///
    /// **Why this project builds no client-facing `authenticate`/
    /// `logout` UI**: verified directly against `acpx-core::router`
    /// (`dispatch_native`'s `"authenticate"`/`"logout"` arms) before
    /// concluding this, not assumed from the method names alone --
    /// acpx's own `initialize` response always advertises
    /// `"authMethods": []` and omits `agentCapabilities.auth.logout`
    /// entirely (both real, deliberate router behavior, each with its
    /// own code comment explaining why: acpx's access control is
    /// transport-level HTTP-bearer/WS auth, not ACP-level session
    /// auth). A spec-compliant client only ever calls `authenticate` in
    /// response to a non-empty `authMethods` list and only calls
    /// `logout` if the capability is advertised -- since acpx never
    /// advertises either, a correct panel never has a reason to call
    /// them, and there is no real login/logout UI state to build
    /// without misrepresenting a capability this gateway does not have.
    /// The panel's actual, meaningful "connection/auth state" surface
    /// is exactly `transport_status`'s three real states (`Connecting`/
    /// `Live connection`/`HTTP fallback`), which this test exercises.
    #[test]
    fn transport_status_reports_live_connection_after_a_real_websocket_attach() {
        let gateway = TestGateway::spawn();
        let names = ["Status Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");
        // Construction deliberately does not block on the shared
        // per-provider `Gateway::connect()` task (only the actor's own
        // `session/new` attachment is guaranteed by other call sites'
        // `wait_for_attachment` -- see `AgentBridge`'s own doc comment
        // on `attachment`/`attachment_ready`), so `transport_status`
        // may briefly still read `"Connecting..."` immediately after
        // `new_with_gateway_resolver_and_cache_dir` returns. Poll with
        // a bounded deadline, same idiom this crate's other real-
        // process tests use for async background state, rather than
        // asserting on the very first read.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut status = bridge.transport_status(0);
        while status != "Live connection" && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            status = bridge.transport_status(0);
        }
        assert_eq!(
            status, "Live connection",
            "a freshly attached thread against a real, reachable acpx-server \
             must report the live WebSocket state, not Connecting/HTTP fallback"
        );
        // Out-of-range index degrades to a safe, non-panicking status
        // string rather than misreporting a live connection that
        // doesn't exist.
        assert_eq!(bridge.transport_status(99), "Unavailable");
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
            modes
                .available
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
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
        let observed_mode_id = std::fs::read_to_string(&set_mode_marker).unwrap_or_default();
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
        assert_eq!(after_create[0].name, "bridge-fs");

        assert!(bridge.update_mcp_server(
            0,
            serde_json::json!({ "name": "bridge-fs", "command": "mcp-bridge-fs-v2" })
        ));
        let after_update = bridge.list_mcp_servers(0);
        assert_eq!(after_update.len(), 1);
        assert_eq!(after_update[0].command.as_deref(), Some("mcp-bridge-fs-v2"));

        assert!(bridge.delete_mcp_server(0, "bridge-fs"));
        assert!(
            bridge.list_mcp_servers(0).is_empty(),
            "expected the server to be gone after delete"
        );

        // Agent catalog: real fallback/live registry entries, each with
        // a real detection status -- not a client-side stub.
        let agents = bridge.list_agents(0);
        assert!(
            agents.iter().any(|a| a.id == "codex-acp"),
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
        assert!(
            seen,
            "expected the real shell's own echoed output through the bridge"
        );

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

    /// Phase 2 step 3 (chat-panel-production-ui/execution-plan.md):
    /// proves `AgentBridge::transcript` actually reflects a real
    /// backend's multi-chunk streaming reply merged into one row, while
    /// `AgentBridge::history` keeps every raw chunk -- the exact
    /// contract `to_message_model_from_transcript` depends on. A stand-
    /// in backend sends three separate `agent_message_chunk`
    /// notifications all carrying the same real `messageId`, exactly
    /// how a real streaming backend would split one growing reply
    /// across several `session/update` pushes.
    #[test]
    fn transcript_merges_a_real_multi_chunk_streamed_reply_by_message_id() {
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("stand_in_backend.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-stream"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-stream","update":{"sessionUpdate":"agent_message_chunk","messageId":"reply-1","content":{"type":"text","text":"Hello"}}}}\n'
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-stream","update":{"sessionUpdate":"agent_message_chunk","messageId":"reply-1","content":{"type":"text","text":", "}}}}\n'
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-stream","update":{"sessionUpdate":"agent_message_chunk","messageId":"reply-1","content":{"type":"text","text":"world"}}}}\n'
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
                    .env("ACPX_DEFAULT_AGENT_ID", "stream-merge-test")
                    .env("RUST_LOG", "error");
            });
            TestGateway { child, base_url }
        };

        let names = ["Stream Merge Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");
        bridge.send_prompt(0, "say hello world".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut turn_ended = false;
        while std::time::Instant::now() < deadline && !turn_ended {
            for event in bridge.poll() {
                if let AgentEvent::TurnEnded(_) = event.event {
                    turn_ended = true;
                }
            }
            if !turn_ended {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(turn_ended, "backend never completed the streamed turn");

        let raw_history = bridge.history(0);
        assert_eq!(
            raw_history
                .iter()
                .filter(|m| m.text.contains("Hello") || m.text == ", " || m.text == "world")
                .count(),
            3,
            "expected 3 separate raw chunks in history, got {raw_history:?}"
        );

        let transcript = bridge.transcript(0);
        let merged = transcript
            .iter()
            .find_map(|item| match item {
                crate::conversation::TranscriptItem::Assistant {
                    text, message_id, ..
                } if message_id == "reply-1" => Some(text.clone()),
                _ => None,
            })
            .expect("expected exactly one merged Assistant transcript item for reply-1");
        assert_eq!(
            merged, "Hello, world",
            "expected the three chunks merged into one row in real messageId-arrival order"
        );
        let assistant_count = transcript
            .iter()
            .filter(|item| matches!(item, crate::conversation::TranscriptItem::Assistant { .. }))
            .count();
        assert_eq!(
            assistant_count, 1,
            "expected the transcript to have exactly one merged Assistant row, not one per chunk"
        );
    }

    /// Phase 3 steps 1-2 (chat-panel-production-ui/execution-plan.md),
    /// through the real `AgentBridge` construction path, not just
    /// `JsonlStore`'s own unit tests directly: a thread whose real
    /// jsonl cache holds far more than `HISTORY_PAGE_SIZE` messages
    /// cold-starts with only the newest page loaded, and repeated
    /// `load_older_page` calls walk backward through the rest in the
    /// correct order, ending with `has_older_page` reporting `false`
    /// and `history` holding every seeded message.
    #[test]
    fn cold_start_loads_only_the_newest_page_and_load_older_page_walks_back_to_the_start() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Long History Thread"];
        let thread_id = slug(names[0]);

        // Seed a real cache file with more than one page's worth of
        // messages, independent of this bridge (mirrors a prior run's
        // accumulated scrollback).
        let total_messages = HISTORY_PAGE_SIZE * 2 + 37;
        let seeded_messages: Vec<ChatMessage> = (0..total_messages)
            .map(|i| ChatMessage {
                // Alternating User/Agent -- a realistic shape (unlike an
                // uninterrupted run of same-kind chunks, which this
                // reducer's own synthetic-id merge heuristic is
                // *designed* to collapse into one growing message, see
                // `conversation::rebuild_from_chat_messages`'s doc
                // comment) so this test's own `transcript(0)` assertion
                // below is meaningful rather than incidentally
                // exercising the merge behavior a different, dedicated
                // test already covers.
                kind: if i % 2 == 0 {
                    MessageKind::User
                } else {
                    MessageKind::Agent
                },
                text: format!("message-{i}"),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
            })
            .collect();
        let seed_store = JsonlStore::open(cache_dir.path()).expect("open store for seeding");
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

        // Cold start loaded only the newest page, not the full 1037.
        let initial = bridge.history(0);
        assert_eq!(
            initial.len(),
            HISTORY_PAGE_SIZE,
            "cold start should load exactly one page, not the full cached history"
        );
        assert_eq!(
            initial[0].text,
            format!("message-{}", total_messages - HISTORY_PAGE_SIZE)
        );
        assert_eq!(
            initial[HISTORY_PAGE_SIZE - 1].text,
            format!("message-{}", total_messages - 1)
        );
        assert!(bridge.has_older_page(0));

        // First load_older_page call adds the next page back.
        assert!(bridge.load_older_page(0));
        let after_one = bridge.history(0);
        assert_eq!(after_one.len(), HISTORY_PAGE_SIZE * 2);
        assert_eq!(
            after_one[0].text,
            format!("message-{}", total_messages - HISTORY_PAGE_SIZE * 2)
        );
        assert!(bridge.has_older_page(0));

        // Second call reaches the real start (37 remaining messages).
        assert!(bridge.load_older_page(0));
        let after_two = bridge.history(0);
        assert_eq!(after_two.len(), total_messages);
        assert_eq!(after_two[0].text, "message-0");
        assert!(!bridge.has_older_page(0));

        // Further calls are a genuine no-op, not an error/duplicate.
        assert!(!bridge.load_older_page(0));
        assert_eq!(bridge.history(0).len(), total_messages);

        // The merged transcript view grew to match -- proves
        // `load_older_page` actually refreshed `transcript`, not just
        // `history`.
        assert_eq!(bridge.transcript(0).len(), total_messages);
    }

    /// `skill_injection_verification` phase: `skills_mcp_servers_entry`'s
    /// output shape -- the actual client-supplied `mcpServers` entry every
    /// `session/new`/`session/load` now sends (see `Command::OpenSession`/
    /// `Command::ResumeSession`'s doc comments), verified directly rather
    /// than through a real acpx-server round trip (this sandbox's
    /// acpx-server makes a real network call to cdn.agentclientprotocol.com
    /// at startup before binding its port -- confirmed directly by
    /// inspecting its own startup log -- making real round-trip tests here
    /// flaky on network latency, unrelated to this logic itself).
    #[test]
    fn skills_mcp_servers_entry_always_includes_the_skills_server() {
        // Not asserting entries.len() == 1: a real snapshotd daemon
        // happening to run on the test host makes snapshotd_mcp_server_
        // entry's liveness probe legitimately append a second "snapshotd"
        // entry (see that function's own doc comment) regardless of
        // provider -- this test only cares that "skills" is always
        // present, first, and correctly shaped.
        let entries = skills_mcp_servers_entry(None, "codex");
        assert!(!entries.is_empty());
        assert_eq!(entries[0]["name"], "skills");
        assert!(entries[0]["command"]
            .as_str()
            .unwrap()
            .contains("skills-mcp-server"));
        let args = entries[0]["args"].as_array().expect("args is an array");
        assert!(args.contains(&serde_json::Value::String("--global-dir".to_string())));
        assert!(
            !args.contains(&serde_json::Value::String("--project-dir".to_string())),
            "no project open -- args must not claim a --project-dir"
        );
    }

    #[test]
    fn skills_mcp_servers_entry_adds_project_dir_from_the_open_project_files_parent() {
        let project_file = std::path::Path::new("/tmp/my-project/timeline.mlt");
        let entries = skills_mcp_servers_entry(Some(project_file), "codex");
        let args = entries[0]["args"].as_array().expect("args is an array");
        let project_dir_idx = args
            .iter()
            .position(|a| a == "--project-dir")
            .expect("--project-dir must be present when a project is open");
        assert_eq!(
            args[project_dir_idx + 1],
            serde_json::Value::String("/tmp/my-project".to_string()),
            "--project-dir must be the project FILE's parent directory, not the file itself"
        );
    }

    /// `"type": "http"` was confirmed live to work with both real
    /// `codex-acp` (which otherwise hard-rejects `"type": "sse"`
    /// entirely) and real `claude-agent-acp` -- this entry is provider-
    /// agnostic by design now, so the shape must stay identical
    /// regardless of which provider string is passed in.
    #[test]
    fn snapshotd_mcp_server_entry_is_absent_when_no_daemon_answers_for_any_provider() {
        // SNAPSHOTD_MCP_SSE_ADDR pointed at a certainly-unbound loopback
        // port -- deterministic "nothing is listening" regardless of
        // what may or may not be running on the machine executing this
        // test. The entry is provider-agnostic (see the function's own
        // doc comment for why "http", not "sse", covers both real
        // adapters), so every provider string must behave identically
        // here.
        std::env::set_var("SNAPSHOTD_MCP_SSE_ADDR", "127.0.0.1:1");
        for provider in ["codex", "claude", ""] {
            assert!(
                snapshotd_mcp_server_entry(provider).is_empty(),
                "no daemon reachable -- must stay empty for provider {provider:?}"
            );
        }
        std::env::remove_var("SNAPSHOTD_MCP_SSE_ADDR");
    }
}
/// Refreshes `slot`'s trailer (`acp_session_id`/`updated_at`), taking
/// into account whether `history` currently holds the thread's *full*
/// cached content or only a bounded newest page (Phase 3 cold-start
/// paging, see `seed_thread_from_cache`/`AgentBridge::load_older_page`).
///
/// **Real bug this function's `older_available` check exists to
/// prevent**: [`JsonlStore::overwrite`] always replaces a thread's
/// *entire* on-disk jsonl content with whatever `messages` slice it is
/// given. Before bounded cold-start loading existed, `slot.history`
/// always held a thread's complete cached scrollback, so calling
/// `overwrite(thread_id, &history, ..)` here was a safe, if slightly
/// wasteful, way to refresh the trailer. Once cold start only loads the
/// newest page, calling `overwrite` with that partial `history` would
/// silently and permanently discard every older cached message still
/// sitting on disk the moment any thread that hasn't had `load_older_
/// page` called on it opens its session (caught by this exact scenario
/// in `agent_bridge::tests::cold_start_loads_only_the_newest_page_and_
/// load_older_page_walks_back_to_the_start` during development -- the
/// first `load_older_page` call came back with a page indistinguishable
/// from the already-loaded tail, because the file it was reading from
/// had already been truncated down to just that tail page by this exact
/// path). So: if `older_available` is true, only the small standalone
/// trailer file is touched ([`JsonlStore::update_trailer`], message
/// count computed as `history.len() + oldest_loaded_index` without
/// needing to read the index file at all); the jsonl file and its index
/// are left completely untouched. Only once the *entire* thread is
/// loaded into memory (`older_available: false` -- either it always fit
/// in one page, or `load_older_page` walked all the way back) is a full
/// `overwrite` safe again, matching this function's pre-paging
/// behavior exactly.
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
    let older_available = *slot
        .older_available
        .lock()
        .expect("older_available mutex poisoned");
    let real_message_count = if older_available {
        history.len()
            + *slot
                .oldest_loaded_index
                .lock()
                .expect("oldest_loaded_index mutex poisoned")
    } else {
        history.len()
    };
    let trailer = ThreadTrailer {
        acp_session_id: session_id,
        title: Some(slot.thread_id.clone()),
        updated_at: Some(updated_at),
        message_count: real_message_count,
    };
    let result = if older_available {
        store.update_trailer(&slot.thread_id, &trailer)
    } else {
        store.overwrite(&slot.thread_id, &history, &trailer)
    };
    if let Err(e) = result {
        eprintln!(
            "panel-rust: jsonl trailer persist failed for {}: {e}",
            slot.thread_id
        );
    }
}
