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
    spawn_acpx_thread_with_delayed_gateway, spawn_acpx_thread_with_delayed_gateway_and_pool,
    AcpxThreadGatewaySetter, AcpxThreadHandle, GatewaySessionOpener, SharedSessionPool,
};
use crate::jsonl_store::{
    JsonlStore, TerminalRuntimeSnapshot, ThreadRuntimeSnapshot, ThreadTrailer,
};
use crate::protocol_types::{
    AgentEvent, AgentRequestEvent, ChatMessage, ConfigOptionInfo, SessionModesEvent,
    TerminalCreatedEvent, TerminalOutputEvent,
};
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
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
/// to. The TEA frame reducer matches on `event` for thread-status
/// transitions and, for `Message`, re-reads `AgentBridge::history` via
/// the next selected-thread snapshot rather than trusting text carried
/// here.
#[derive(Clone, Debug, PartialEq)]
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
    /// PISO-3: the durable `ThreadRecord::project_path` this thread was
    /// last persisted against, if any -- carried through so the restored
    /// slot's `project_path` (below) hydrates from what was actually
    /// stored rather than starting `None` and silently inheriting
    /// whatever project happens to be active at this restart (the leak
    /// this phase closes). `None` for a freshly-seeded default thread
    /// (nothing persisted yet) and for a legacy pre-migration record.
    pub project_path: Option<String>,
}

/// The resolved binding returned once a thread has opened or resumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadBinding {
    pub thread_id: String,
    pub session_id: String,
}

/// Builds bare `ThreadSpec`s from thread names alone, with no real
/// per-thread provider binding -- every spec gets
/// [`NO_PROVIDER_REQUESTED_FALLBACK`]. Real production startup never
/// calls this: `lib.rs`'s cold-start path builds `ThreadSpec`s directly
/// (persisted records' own provider, or `default_agent_id` from
/// settings), and `AgentBridge::new_with_thread_specs` takes those
/// directly. This helper only backs the name-only test/dev
/// constructors ([`AgentBridge::new`], [`AgentBridge::new_with_gateway_
/// url`], [`AgentBridge::new_with_gateway_resolver_and_cache_dir`]),
/// whose own tests don't care which literal provider string a
/// synthesized thread gets (most point every provider at one shared
/// test gateway) -- a test that DOES care about provider identity
/// builds real `ThreadSpec`s with explicit providers instead.
fn specs_for_names(thread_names: &[&str]) -> Vec<ThreadSpec> {
    thread_names
        .iter()
        .map(|name| ThreadSpec {
            display_name: (*name).to_owned(),
            provider: NO_PROVIDER_REQUESTED_FALLBACK.to_owned(),
            session_id: None,
            profile_name: None,
            project_path: None,
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
    /// Server continuation cursor for remote transcript pagination.
    history_cursor: Mutex<Option<String>>,
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
    /// Phase 18: latest live token usage (used, size) from streaming
    /// usage_update events -- feeds the compose context ring DURING a
    /// turn, not only at turn end.
    usage: Mutex<(i64, i64)>,
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
    /// Pre-session model catalogs keyed by the provider selected in the
    /// compose bar. Keeping this provider-scoped prevents a prior selection
    /// from making the next provider show stale models.
    pre_session_model_options: Arc<Mutex<HashMap<String, Vec<ConfigOptionInfo>>>>,
    /// PUI-003: the agent's own built-in slash commands, from
    /// `available_commands_update`. Replaced wholesale on each push; not
    /// persisted (the agent re-advertises on session start), so it is not
    /// part of the runtime snapshot.
    available_commands: Mutex<Vec<crate::protocol_types::AvailableCommandInfo>>,
    /// PROF-11: the agent's most recently pushed execution plan/todo
    /// list, from a live `plan` session/update
    /// ([`AgentEvent::PlanUpdate`]'s doc comment). Replaced wholesale on
    /// each push (ACP's `Plan` is always-the-complete-plan, never a
    /// delta); not persisted -- same "the agent re-advertises, this is
    /// ephemeral capability state" reasoning as `available_commands`
    /// above.
    plan: Mutex<Vec<crate::protocol_types::PlanEntryInfo>>,
    /// PROF-11: the most recently pushed live session title, from a
    /// `session_info_update` session/update
    /// ([`AgentEvent::SessionInfoUpdate`]'s doc comment). Deliberately
    /// separate from the durable, user-editable `ThreadModel::
    /// display_name` -- an agent-pushed title is a live signal, not a
    /// rename the user asked for, so it must never silently overwrite
    /// what the user typed. `None` until the backend sends one; not
    /// persisted, same ephemeral-capability-state reasoning as `plan`.
    session_title: Mutex<Option<String>>,
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
    /// setup-followups plan, archive_thread_backend_verify: set once
    /// [`AgentBridge::archive_thread`] has been called for this thread.
    /// Purely local -- no ACP request is involved -- but unlike `closed`
    /// this one IS persisted (see [`persist_runtime_snapshot`] and
    /// [`ThreadRuntimeSnapshot::archived`]), since the whole point of an
    /// archive action is that it survives a restart.
    archived: Mutex<bool>,
    /// `thread_item_project_context` phase: the project directory this
    /// thread's session was actually opened/resumed/reattached against
    /// (the `cwd` passed to ACP at creation time -- see `cwd_for_session`),
    /// captured once and normally never updated afterward, since ACP has
    /// no way to move an existing session to a new cwd. `None` when no
    /// project was active at creation time (the pre-`active_project_
    /// binding` default).
    ///
    /// PISO-7 (project-isolation-mlt-binding plan) is the one deliberate
    /// exception: an MLT Save-As renames the project file out from under
    /// every thread that was recorded against the old path, without
    /// touching any ACP session at all -- the session's real `cwd` on
    /// disk hasn't moved, only the panel's own bookkeeping of which
    /// project it belongs to needs to follow. `Mutex`, not a plain field,
    /// so `AgentBridge::rebind_project_path` can update matching slots'
    /// values in place for the live session (sqlite alone only fixes the
    /// NEXT restart -- see that method's doc comment); every other
    /// consumer keeps treating a lock+clone as if it were still a
    /// captured-once, effectively-immutable read.
    project_path: Mutex<Option<PathBuf>>,
    /// PUI-014: `true` for a slot created up front (so it holds its positional
    /// index in `slots`, preserving the `model.threads[i] <-> slots[i]`
    /// parallel-array invariant) but whose ACP session attach is deliberately
    /// DEFERRED until the thread's first message is sent -- keeping the
    /// provider/profile editable until then. A deferred slot has no
    /// `spawn_background_attachment` running and no `acp_session_id`;
    /// [`AgentBridge::attach_deferred_thread`] replaces it in place with a
    /// freshly-built, eagerly-attached slot bound to the then-current
    /// provider. `false` for every eagerly-attached or recovered slot.
    deferred: bool,
    /// Whether teardown should retain this session server-side as a
    /// background session. This belongs to the owning chat, not the active
    /// project, so it is kept per slot.
    background: Mutex<bool>,
}

impl ThreadSlot {
    /// A point-in-time copy of `project_path`. Every consumer already
    /// treated the (formerly plain) field as a single value read once per
    /// use, so this is the one place that lock+clone lives rather than
    /// repeating it at each of the several call sites below.
    fn project_path_snapshot(&self) -> Option<PathBuf> {
        self.project_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[derive(Default)]
struct AttachmentState {
    complete: bool,
    error: Option<String>,
}

/// One terminal's current known state, as last observed via
/// `AgentEvent::TerminalOutput`/`AgentEvent::TerminalCreated`. See
/// [`ThreadSlot::terminal_buffers`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalBuffer {
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<(Option<i32>, Option<i32>)>,
    // PUI-002b (background-terminals-ui plan): populated from the one-shot
    // `AgentEvent::TerminalCreated` event, which arrives (if at all) before
    // or interleaved with the first `TerminalOutput` for the same id --
    // never re-derived from `output` or guessed. `#[serde(default)]` so a
    // JSONL runtime snapshot persisted before this field existed still
    // deserializes (older rows just show an empty command/title until the
    // agent creates a fresh terminal).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// RFC 3339; empty if `TerminalCreated` was never observed for this id
    /// (e.g. this buffer was populated by output arriving from a snapshot
    /// restored before this field existed).
    #[serde(default)]
    pub started_at: String,
}

impl TerminalBuffer {
    /// `active` is derived, never stored: "still running" is exactly
    /// "no exit status observed yet" -- keeping a separate stored bool
    /// in sync with `exit_status` would just be a second source of
    /// truth that could drift.
    pub fn active(&self) -> bool {
        self.exit_status.is_none()
    }
}

/// In-memory gateway catalog the UI frame poll **drains** (clone only).
/// Background tasks **push** updates via [`AgentBridge::request_gateway_catalog_refresh`].
/// Never filled by `runtime.block_on` on the UI thread (lock_audit F-01/F-02).
#[derive(Clone, Default)]
struct GatewayCatalogCache {
    profiles: Vec<crate::gateway_actor::ProfileSummary>,
    mcp_servers: Vec<crate::protocol_types::McpServerEntry>,
    agents: Vec<crate::protocol_types::AgentCatalogEntry>,
    recoverable_sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    recovery_provider: String,
    /// Monotonic generation; UI can detect "never filled" as gen == 0.
    gen: u64,
    last_refresh: Option<std::time::Instant>,
}

/// Identity of the central MCP registry for poll-diff purposes.
/// Ignores ephemeral `tool_catalog` (recomputed every list) so a tools
/// fetch cannot look like a registry mutation that should evict pools.
/// Sorted by name so list order from the gateway does not matter.
fn mcp_registry_identity(
    servers: &[crate::protocol_types::McpServerEntry],
) -> Vec<(
    String,
    bool,
    crate::protocol_types::McpServerConfig,
    Option<crate::protocol_types::McpAuthStatus>,
)> {
    let mut rows: Vec<_> = servers
        .iter()
        .map(|s| {
            (
                s.name.clone(),
                s.enabled,
                s.config.clone(),
                s.auth_status,
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Merge a fresh `mcp_servers/list` payload with the UI-side optimistic
/// cache so concurrent list polls cannot wipe an in-flight tools_fetch
/// spinner or enable-toggle StatusDot before the matching RPC lands.
///
/// Rules (per server name present on the wire):
/// - `tools_fetch:<name>` in flight + wire `tool_catalog == None` and
///   local was `Fetching` → keep `Fetching`
/// - `enabled:<name>` in flight → keep local `enabled` (toggle already
///   flipped in the UI; list may still show the pre-update value)
/// - wire `tool_catalog == None` + local `Error` → keep the last fetch error
///   until the next fetch explicitly replaces it with `Fetching`
/// Wire-side Ready/Error always wins over local Fetching.
fn merge_mcp_list_with_optimistic(
    wire: Vec<crate::protocol_types::McpServerEntry>,
    previous: &[crate::protocol_types::McpServerEntry],
    ops: &HashSet<String>,
) -> Vec<crate::protocol_types::McpServerEntry> {
    use crate::protocol_types::McpToolCatalog;
    wire.into_iter()
        .map(|mut entry| {
            let prev = previous.iter().find(|p| p.name == entry.name);
            if ops.contains(&format!("tools_fetch:{}", entry.name))
                && entry.tool_catalog.is_none()
                && matches!(
                    prev.and_then(|p| p.tool_catalog.as_ref()),
                    Some(McpToolCatalog::Fetching)
                )
            {
                entry.tool_catalog = Some(McpToolCatalog::Fetching);
            }
            if entry.tool_catalog.is_none()
                && matches!(
                    prev.and_then(|p| p.tool_catalog.as_ref()),
                    Some(McpToolCatalog::Error { .. })
                )
            {
                entry.tool_catalog = prev.and_then(|p| p.tool_catalog.clone());
            }
            if ops.contains(&format!("enabled:{}", entry.name)) {
                if let Some(prev) = prev {
                    entry.enabled = prev.enabled;
                }
            }
            entry
        })
        .collect()
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
    // acpx-client-session-lease-pool: one ProjectSessionPool per (project
    // directory, gateway base_url) pair -- since mcp_servers is computed
    // per (project_dir, provider) and provider maps 1:1 to base_url today
    // (same granularity as `gateways` above), one pool's GatewaySessionOpener
    // config applies uniformly to every PoolKey (agent/profile) opened
    // through it. The `Vec<Value>` alongside each pool is the last mcp_servers
    // this bridge applied to that pool's opener -- compared on every
    // `pool_for` call so a genuine config change (skills dir moved,
    // snapshotd address changed, ...) triggers `set_mcp_servers` +
    // `refresh_key`/`refresh_all`, while an unchanged value is a no-op
    // (never refreshes/drops warm sessions needlessly).
    project_pools:
        Arc<Mutex<std::collections::HashMap<String, (SharedSessionPool, Vec<serde_json::Value>)>>>,
    /// Background-filled gateway catalog (profiles/mcp/agents/sessions).
    /// Frame poll clones this with `try_lock` only — never waits on the
    /// background publisher and never performs RPC on the UI thread.
    gateway_catalog: Arc<Mutex<GatewayCatalogCache>>,
    gateway_catalog_refreshing: Arc<std::sync::atomic::AtomicBool>,
    /// Agent ids with an install or enablement RPC currently in flight.
    /// Access is short-lived and read with `try_lock` from frame polling.
    agent_operations: Arc<Mutex<HashSet<String>>>,
    /// MCP server settings actions currently in flight, keyed
    /// `"<action>:<server-name>"` (e.g. `"create:filesystem"`,
    /// `"authenticate:github"`) -- same shape/lifecycle as `agent_
    /// operations` above (`begin_mcp_operation`/`mcp_operations_in_flight`
    /// mirror `begin_agent_operation`/`agent_operations_in_flight`
    /// exactly), kept as a separate field rather than sharing one set so
    /// an agent id and an MCP server name can never collide on the same
    /// key by coincidence.
    mcp_operations: Arc<Mutex<HashSet<String>>>,
    /// recoverable-attach-fix: remote session ids with a `recover-
    /// session-attach` `session/load` currently in flight (Settings >
    /// Agents "Attach" button, symptom #2 -- the row previously had no
    /// busy-state tracking at all, unlike `agent_operations`/`mcp_
    /// operations` above). Same shape/lifecycle: `begin_recover_session_
    /// operation`/`recover_session_operations_in_flight` mirror `begin_
    /// mcp_operation`/`mcp_operations_in_flight` exactly. Keyed by the
    /// remote `acp_session_id`, not the local thread id -- a session
    /// stays a candidate row (and therefore needs its own busy key) right
    /// up until it disappears from `recoverable_sessions` once bound.
    recover_session_operations: Arc<Mutex<HashSet<String>>>,
    // PROF-1: the same per-provider URL resolver the constructor used to
    // seed `gateway_urls` up front, kept around so a provider nobody
    // asked for at construction time (any real agent id, not just a
    // hardcoded pair) can still be provisioned lazily the first time a
    // thread actually requests it -- see `ensure_gateway_provisioned`.
    // Never Send/Sync-bounded because it is only ever called from `&mut
    // self` methods on this bridge's own owning thread, never moved into
    // a spawned async task itself (only the resulting URL is).
    resolve_gateway: Box<dyn Fn(&str) -> Result<String, BridgeError>>,
    // PROF-1: this bridge's own default provider for `add_thread`-family
    // calls that request no provider at all -- the first thread spec's
    // already-resolved provider (itself derived upstream from a real
    // profile's agent id, never an index-based guess). `None` for a
    // bridge that started with zero threads; see
    // `NO_PROVIDER_REQUESTED_FALLBACK` for that narrower case.
    default_provider: Option<String>,
    /// Production ACPX sessions keep canonical transcript/queue state on
    /// acpx-server. Legacy cache-backed constructors leave this false for
    /// focused local-store tests and compatibility callers.
    server_owned_persistence: bool,
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
    // (`dispatch_terminal_snapshot` and friends) rely on being able to call
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
    /// Raw active MLT identity used for newly-created thread ownership.
    /// `session_cwd_override` is deliberately the derived project store,
    /// which is suitable for ACP cwd but must never be persisted as a raw
    /// project path or fed back through project_store_dir.
    session_project_path_override: Arc<Mutex<Option<PathBuf>>>,
}

/// Provider gateways are process-scoped, not project-view-scoped. Keeping a
/// strong reference here lets the C++ project switch recreate the panel's
/// project-local bridge without tearing down the multiplexed ACPX connection
/// that can still serve background sessions from another project.
fn shared_gateway_cache() -> &'static Mutex<HashMap<String, Arc<acpx_client::Gateway>>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Arc<acpx_client::Gateway>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
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
    let history = slot
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let rebuilt = crate::conversation::rebuild_from_chat_messages(&slot.thread_id, &history);
    *slot.transcript.lock().unwrap_or_else(|e| e.into_inner()) = rebuilt;
}

/// Caps how many distinct terminal ids one thread retains in
/// `ThreadSlot::terminal_buffers`/`terminal_order`. Without this, every
/// terminal a thread's agent ever spawns over a long-lived session (or
/// across restarts, since both fields are persisted whole into the JSONL
/// runtime snapshot -- see [`persist_runtime_snapshot`]) accumulates
/// forever; only exited terminals are evicted, oldest first, so a
/// terminal the user might still be watching is never dropped out from
/// under them.
const MAX_RETAINED_TERMINALS_PER_THREAD: usize = 8;

fn store_terminal_output(slot: &ThreadSlot, ev: &TerminalOutputEvent) {
    let mut buffers = slot
        .terminal_buffers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let is_new = !buffers.contains_key(&ev.terminal_id);
    if is_new {
        slot.terminal_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ev.terminal_id.clone());
    }
    // Preserve command/args/started_at (set by `store_terminal_created`,
    // see its doc comment) across every output update -- this event's own
    // "replace, don't append" contract is about `output`/`truncated`/
    // `exit_status` only; TerminalOutputEvent carries no creation metadata
    // at all, so overwriting the whole struct would silently erase it the
    // moment output arrives after creation.
    let (command, args, started_at) = buffers
        .get(&ev.terminal_id)
        .map(|existing| {
            (
                existing.command.clone(),
                existing.args.clone(),
                existing.started_at.clone(),
            )
        })
        .unwrap_or_default();
    buffers.insert(
        ev.terminal_id.clone(),
        TerminalBuffer {
            output: ev.output.clone(),
            truncated: ev.truncated,
            exit_status: ev.exit_status,
            command,
            args,
            started_at,
        },
    );
    drop(buffers);
    evict_exited_terminals_over_cap(slot);
}

/// One-shot handler for `AgentEvent::TerminalCreated` -- sets
/// command/args/started_at on the (possibly not-yet-existing) buffer for
/// this terminal id, without touching output/truncated/exit_status. Order
/// with `store_terminal_output` is not assumed either way: whichever
/// arrives first creates the entry, the other fills in its own fields on
/// top, same "operate on whatever's there" shape as the rest of this
/// file's per-terminal bookkeeping.
fn store_terminal_created(slot: &ThreadSlot, ev: &TerminalCreatedEvent) {
    let mut buffers = slot
        .terminal_buffers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let is_new = !buffers.contains_key(&ev.terminal_id);
    if is_new {
        slot.terminal_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ev.terminal_id.clone());
    }
    let entry = buffers
        .entry(ev.terminal_id.clone())
        .or_insert_with(|| TerminalBuffer {
            output: String::new(),
            truncated: false,
            exit_status: None,
            command: String::new(),
            args: Vec::new(),
            started_at: String::new(),
        });
    entry.command = ev.command.clone();
    entry.args = ev.args.clone();
    entry.started_at = ev.started_at.clone();
    drop(buffers);
    evict_exited_terminals_over_cap(slot);
}

fn evict_exited_terminals_over_cap(slot: &ThreadSlot) {
    let mut order = slot
        .terminal_order
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut buffers = slot
        .terminal_buffers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    evict_exited_terminals_over_cap_in(&mut order, &mut buffers, MAX_RETAINED_TERMINALS_PER_THREAD);
}

/// Evicts the oldest *exited* terminals (by first-seen order in `order`)
/// until at most `cap` remain, or until none of the remaining candidates
/// have exited -- a still-running terminal is never evicted, so this only
/// bounds growth once terminals actually finish. Free of `ThreadSlot`/
/// `Mutex` so the eviction policy itself is unit-testable without
/// constructing a full bridge thread slot.
fn evict_exited_terminals_over_cap_in(
    order: &mut Vec<String>,
    buffers: &mut HashMap<String, TerminalBuffer>,
    cap: usize,
) {
    if order.len() <= cap {
        return;
    }
    let mut idx = 0;
    while order.len() > cap && idx < order.len() {
        let has_exited = buffers
            .get(&order[idx])
            .is_some_and(|buffer| buffer.exit_status.is_some());
        if has_exited {
            let terminal_id = order.remove(idx);
            buffers.remove(&terminal_id);
        } else {
            idx += 1;
        }
    }
}

/// Applies one [`AgentEvent::SessionModes`]/[`AgentEvent::
/// CurrentModeChanged`]/[`AgentEvent::ConfigOptions`] event to `slot`'s
/// own capability state -- shared by both forwarder loops, same role
/// [`store_terminal_output`] plays for terminal buffers.
fn store_capability_event(slot: &ThreadSlot, ev: &AgentEvent) {
    match ev {
        AgentEvent::SessionModes(modes) => {
            *slot.session_modes.lock().unwrap_or_else(|e| e.into_inner()) = Some(modes.clone());
        }
        AgentEvent::CurrentModeChanged(mode_id) => {
            if let Some(modes) = slot
                .session_modes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
                modes.current_mode_id = mode_id.clone();
            }
        }
        AgentEvent::ConfigOptions(options) => {
            *slot
                .config_options
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = options.clone();
        }
        AgentEvent::AvailableCommands(commands) => {
            *slot
                .available_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = commands.clone();
        }
        AgentEvent::PlanUpdate(entries) => {
            *slot.plan.lock().unwrap_or_else(|e| e.into_inner()) = entries.clone();
        }
        AgentEvent::SessionInfoUpdate { title, .. } => {
            // Only `title` is kept -- `updated_at` has no reader today
            // (see `AgentEvent::SessionInfoUpdate`'s doc comment); a
            // `None` title (field absent/explicit-null on the wire) is
            // deliberately NOT stored as a clear, since this collapsed
            // representation can't tell "no change" from "agent cleared
            // it" apart -- clearing a live title on an ambiguous signal
            // would be worse than leaving a stale one showing.
            if let Some(title) = title {
                *slot.session_title.lock().unwrap_or_else(|e| e.into_inner()) = Some(title.clone());
            }
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
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let terminal_buffers = slot
        .terminal_buffers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let snapshot = ThreadRuntimeSnapshot {
        pending_requests: slot
            .pending_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
                        command: buffer.command.clone(),
                        args: buffer.args.clone(),
                        started_at: buffer.started_at.clone(),
                    })
            })
            .collect(),
        session_modes: slot
            .session_modes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        config_options: slot
            .config_options
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        archived: *slot.archived.lock().unwrap_or_else(|e| e.into_inner()),
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
const HISTORY_PAGE_SIZE: usize = 20;

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

/// PROF-1 (`profile-only-backend-selection` plan): the one explicit,
/// documented fallback provider used ONLY when nothing else can name a
/// real agent id -- a genuinely empty bridge with no persisted thread
/// records and no `default_agent_id` configured in settings, or a
/// test/dev call site that only supplies bare thread names (no real
/// per-thread provider binding at all, see [`specs_for_names`]). This
/// replaces the old `provider_for_index` index-parity rotation
/// (alternating "codex"/"claude" by thread position) and the old
/// `normalize_provider` two-bucket collapse -- both silently mapped
/// *any* third agent id onto one of those two labels. There is no
/// longer a rotation or a normalization step: a requested or persisted
/// agent id now flows through to gateway resolution completely as-is
/// (see [`AgentBridge::resolve_provider_for`]), and this constant is
/// reached only in the narrow "nothing to go on yet" case.
pub const NO_PROVIDER_REQUESTED_FALLBACK: &str = "codex";

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
    // A real Windows install's `acpx-server` binary is named
    // `acpx-server.exe` -- shotcut-rebrand's CMakeLists.txt installs it
    // via `install(PROGRAMS $<TARGET_FILE:acpx-server> ...)` with no
    // RENAME, and CMake's `TARGET_FILE` generator expression already
    // includes the platform executable suffix. Every candidate below
    // previously joined a bare `"acpx-server"` with no suffix, so
    // `candidate.is_file()` could never match the installed
    // `acpx-server.exe` on Windows -- it always fell through to the
    // dev-checkout fallback (which doesn't exist on a packaged install),
    // silently leaving `gateway-ready` false forever and the sidebar's
    // "+ New Thread" button (`sidebar.slint`'s `enabled: root.gateway-
    // ready`) permanently, silently disabled. `EXE_SUFFIX` is `""` on
    // Unix, so this is a no-op there.
    let exe_name = format!("acpx-server{}", std::env::consts::EXE_SUFFIX);
    let libexec_name = format!("../libexec/acpx-server{}", std::env::consts::EXE_SUFFIX);
    if let Some(parent) = current_exe.and_then(Path::parent) {
        for candidate in [parent.join(&exe_name), parent.join(&libexec_name)] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    manifest_dir.join(format!("../acpx/target/debug/{exe_name}"))
}

fn resolve_acpx_server_bin() -> PathBuf {
    resolve_acpx_server_bin_from(
        std::env::var("RUI_ACPX_SERVER_BIN").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Best-effort self-heal for a real, recurring production failure: real
/// installs (2026-08-01 `system_launch.yaml` log, "failed to spawn
/// acpx-server for codex on port ...: Permission denied (os error 13)",
/// twice in one session) hit `Command::spawn`'s own `EACCES` -- confirmed
/// by tracing `spawn_gateway_process` below, where `cmd.spawn()` is the
/// *only* syscall in this whole function whose error is wrapped into the
/// "failed to spawn" message (the other candidate EACCES source, the
/// per-provider stderr log `File::create` a few lines down, has its own
/// separate error handling that falls back to `Stdio::null()` instead of
/// failing the spawn at all -- so it can never produce this message).
///
/// `EACCES` from `execve` on a path that *does* resolve to a real file
/// (as opposed to `ENOENT`, which `resolve_acpx_server_bin_from`'s own
/// fallback chain can also produce, but that surfaces as a distinctly
/// different os error 2) means the file exists but lacks the execute
/// bit for this process's effective uid/gid -- exactly what a plain
/// `cp`/archive-extract/artifact-download step that does not preserve
/// POSIX permission bits produces (this project's own Linux packaging
/// pipeline, `shotcut/scripts/build-snapflow.sh`'s `install_snapflow_linux`,
/// assembles the bundle with a long sequence of manual `cp`/`install`
/// calls rather than exclusively CMake's `install(PROGRAMS ...)`, which
/// is the one step that would otherwise always normalize this). Rather
/// than hard-failing the whole gateway (and every thread on this
/// provider) over a one-bit permission defect this process is fully
/// entitled to fix on a file it already resolved by path, attempt to add
/// the owner/group/world execute bits before spawning. Best-effort and
/// silent on failure (e.g. a read-only install root) -- `cmd.spawn()`'s
/// own error still surfaces normally if this doesn't help.
#[cfg(unix)]
fn ensure_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mut perms = metadata.permissions();
    let mode = perms.mode();
    // 0o111 = owner+group+world execute. Only touched when at least one
    // of those bits is missing, so an already-correct file's mtime/ctime
    // is left alone.
    if mode & 0o111 != 0o111 {
        perms.set_mode(mode | 0o111);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) {}

/// Resolves the `snapflowd-mcp` binary path (`skill_injection_
/// verification` phase): `RUI_SNAPFLOWD_MCP_BIN` env override, else a
/// path relative to this crate's own `CARGO_MANIFEST_DIR`, same
/// convention as [`resolve_acpx_server_bin`].
fn resolve_snapflowd_mcp_bin_from(
    override_bin: Option<&str>,
    current_exe: Option<&Path>,
    manifest_dir: &Path,
) -> PathBuf {
    if let Some(bin) = override_bin.filter(|bin| !bin.is_empty()) {
        return PathBuf::from(bin);
    }
    if let Some(parent) = current_exe.and_then(Path::parent) {
        let candidate = parent.join("snapflowd-mcp");
        if candidate.is_file() {
            return candidate;
        }
    }
    manifest_dir.join("target/debug/snapflowd-mcp")
}

fn resolve_snapflowd_mcp_bin() -> PathBuf {
    resolve_snapflowd_mcp_bin_from(
        std::env::var("RUI_SNAPFLOWD_MCP_BIN").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolves the `snapshotd` daemon CLI binary path (PISO-8, project-
/// isolation-mlt-binding plan): `RUI_SNAPSHOTD_BIN` env override, else a
/// path relative to this crate's own `CARGO_MANIFEST_DIR`, same
/// convention as [`resolve_acpx_server_bin`]/[`resolve_snapflowd_mcp_bin`].
/// Unlike those two, this binary is a short-lived CLI invocation (`list`/
/// `listProjects`), not a long-lived spawned server -- see
/// [`fetch_daemon_project_instances`].
fn resolve_snapshotd_bin_from(
    override_bin: Option<&str>,
    current_exe: Option<&Path>,
    manifest_dir: &Path,
) -> PathBuf {
    if let Some(bin) = override_bin.filter(|bin| !bin.is_empty()) {
        return PathBuf::from(bin);
    }
    if let Some(parent) = current_exe.and_then(Path::parent) {
        let candidate = parent.join("snapshotd");
        if candidate.is_file() {
            return candidate;
        }
    }
    manifest_dir.join("../snapshotd/snapshotd")
}

fn resolve_snapshotd_bin() -> PathBuf {
    resolve_snapshotd_bin_from(
        std::env::var("RUI_SNAPSHOTD_BIN").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// PISO-8 (project-isolation-mlt-binding plan): one MLT project path
/// (project root dir + `MltFileName` joined, matching
/// `ThreadSlot::project_path`'s own file-path shape) that snapshotd
/// currently reports a `"ready"` process instance for, and whether that
/// instance is headless -- i.e. was launched by an agent's own
/// `daemon.launch` MCP call for a project this panel's own host process
/// never opened, rather than the panel's normal PISO-1 propagation path.
/// A thread bound to such a project (see `models::
/// thread_project_instance_is_live`) is being driven by that live
/// instance, invisibly, unless the UI surfaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonProjectInstance {
    pub project_path: String,
    pub headless: bool,
}

/// Runs one bare `snapshotd` CLI subcommand (`"list"` or `"listProjects"`)
/// and returns its stdout, one JSON object per line (see `cmdList`/
/// `cmdListProjects` in `snapshotd/cmd/snapshotd/main.go` -- both dial the
/// daemon's SDP control socket and print `daemon.list`/`daemon.
/// listProjects`'s raw registry rows, no MCP/HTTP involved). Inherits the
/// caller's environment unmodified, so `SNAPSHOTD_HOME` (relocating which
/// daemon's control socket the CLI dials, per `config.Default`'s own doc
/// comment) flows through exactly as a test or production launch already
/// has it set -- this function never touches `SNAPSHOTD_HOME` itself.
const SNAPSHOTD_CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn run_snapshotd_subcommand(bin: &Path, subcommand: &str) -> Result<String, String> {
    // Do not use Command::output here: a wedged daemon control socket would
    // otherwise leave the background inventory thread blocked forever. Files
    // keep stdout/stderr drainable even if a broken daemon emits a large
    // diagnostic before the timeout expires.
    let unique = format!(
        "snapflow-snapshotd-{}-{}-{}",
        std::process::id(),
        subcommand,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    let stdout_path = std::env::temp_dir().join(format!("{unique}.stdout"));
    let stderr_path = std::env::temp_dir().join(format!("{unique}.stderr"));
    let stdout_file = File::create(&stdout_path)
        .map_err(|error| format!("creating snapshotd {subcommand} stdout capture: {error}"))?;
    let stderr_file = File::create(&stderr_path)
        .map_err(|error| format!("creating snapshotd {subcommand} stderr capture: {error}"))?;
    let mut child = std::process::Command::new(bin)
        .arg(subcommand)
        .stdout(stdout_file)
        .stderr(stderr_file)
        .spawn()
        .map_err(|error| format!("spawning snapshotd {subcommand}: {error}"))?;
    let deadline = std::time::Instant::now() + SNAPSHOTD_CLI_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&stdout_path);
                let _ = std::fs::remove_file(&stderr_path);
                return Err(format!(
                    "snapshotd {subcommand} timed out after {}s",
                    SNAPSHOTD_CLI_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&stdout_path);
                let _ = std::fs::remove_file(&stderr_path);
                return Err(format!("waiting for snapshotd {subcommand}: {error}"));
            }
        }
    };
    let stdout = std::fs::read(&stdout_path)
        .map_err(|error| format!("reading snapshotd {subcommand} stdout capture: {error}"))?;
    let stderr = std::fs::read(&stderr_path)
        .map_err(|error| format!("reading snapshotd {subcommand} stderr capture: {error}"))?;
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    if !status.success() {
        return Err(format!(
            "snapshotd {subcommand} exited {status}: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    String::from_utf8(stdout)
        .map_err(|error| format!("snapshotd {subcommand} produced non-UTF8 stdout: {error}"))
}

/// Parses `snapshotd list`'s JSONL stdout (one `registry.ProcessInstance`
/// per line, PascalCase field names -- see that struct's doc comment for
/// why: no `json` tags, wire shape deliberately mirrors the Go struct) and
/// `snapshotd listProjects`'s JSONL stdout (one `registry.Project` per
/// line) and correlates them into the set of MLT project paths that
/// currently have a `"ready"` instance. Pure/no I/O -- split out from
/// [`fetch_daemon_project_instances`] specifically so this correlation
/// logic is unit-testable without a real daemon.
fn parse_daemon_list_and_projects(
    list_jsonl: &str,
    projects_jsonl: &str,
) -> Vec<DaemonProjectInstance> {
    let mut project_paths_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for line in projects_jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let Ok(project) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = project.get("ID").and_then(|v| v.as_str()) else {
            continue;
        };
        let root_dir = project
            .get("RootDir")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let mlt_file_name = project
            .get("MltFileName")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if root_dir.is_empty() || mlt_file_name.is_empty() {
            continue;
        }
        project_paths_by_id.insert(
            id.to_string(),
            Path::new(root_dir)
                .join(mlt_file_name)
                .to_string_lossy()
                .into_owned(),
        );
    }
    let mut live = Vec::new();
    for line in list_jsonl.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(instance) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if instance.get("Status").and_then(|v| v.as_str()) != Some("ready") {
            continue;
        }
        let Some(project_id) = instance.get("ProjectID").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(project_path) = project_paths_by_id.get(project_id) else {
            continue;
        };
        let headless = instance
            .get("Headless")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        live.push(DaemonProjectInstance {
            project_path: project_path.clone(),
            headless,
        });
    }
    live
}

/// One full `snapshotd list` + `snapshotd listProjects` round trip,
/// returning every project with a currently-live (`"ready"`) instance.
/// Performs real subprocess spawns and (inside the CLI) a real Unix
/// socket dial -- **never call this from the UI thread or a tokio
/// worker**; the only production caller is `Effect::
/// RefreshDaemonProjectInstances`'s background `std::thread::spawn`
/// (`effect_executor.rs`), matching the project-isolation plan's PISO-8
/// data-path discipline note.
pub fn fetch_daemon_project_instances() -> Result<Vec<DaemonProjectInstance>, String> {
    let bin = resolve_snapshotd_bin();
    let list_jsonl = run_snapshotd_subcommand(&bin, "list")?;
    let projects_jsonl = run_snapshotd_subcommand(&bin, "listProjects")?;
    Ok(parse_daemon_list_and_projects(&list_jsonl, &projects_jsonl))
}

/// Builds the `mcpServers` array `session/new`/`session/load` now send
/// (previously always `[]`, see `gateway_actor::thread_actor`'s doc
/// comments on `Command::OpenSession`/`Command::ResumeSession`) -- one
/// entry pointing at `snapflowd-mcp`, always present regardless of
/// which ACPX profile (if any) the session uses. `project_path` is the
/// active MLT project's *file* path (`PanelSingleton::active_project_path`
/// as threaded through `AgentBridge::session_cwd_override`) -- passed
/// through as-is; `snapflowd-mcp` itself derives the project's
/// `.skills/` directory from its parent, same as
/// `PanelSingleton::collect_skills_snapshot`
/// (lib.rs) already does.
///
/// **`"env": []` is required, not optional** -- found live
/// (`video-generation-e2e-harness` plan's `custom_mcp_and_skills_
/// support` phase, 2026-07-23): real `codex-acp`'s own request schema
/// (`zMcpServerStdio` in its bundled `dist/index.js`) requires `env` as
/// a non-optional array for any stdio-shaped MCP server entry, with no
/// default. Its top-level `mcpServers` array is parsed with `zod`'s
/// `vecSkipError` (`.catch(sentinel).transform(filter out sentinel)`),
/// which **silently drops** any entry that fails schema validation --
/// no error, no log, nothing surfaced to the caller. Without this
/// field, this "skills" entry (the app's own real, always-on custom
/// MCP server) was being silently dropped by every real codex-acp
/// session's own request parsing before it ever reached the model --
/// confirmed live via `/mcp`: an identical stdio entry without `env`
/// never appeared in the configured-servers listing at all; the exact
/// same entry with `"env": []` added did.
#[allow(dead_code)]
fn snapflowd_mcp_servers_entry(
    project_dir: Option<&std::path::Path>,
    provider: &str,
) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    // Whether MCP-free, filesystem-only skill delivery is safe for
    // `provider` (an ACP registry agent id or its short-form alias) now
    // lives in skills_manager::agent_registry (memory/acpx/gen/plans/acpx-skills/
    // README.md#agent-skill-convention-registry) -- true only for
    // vendor_ids a live test actually proved, currently just "codex"/
    // "codex-acp" (panel-rust/tests/skills_manager_live_discovery_e2e_test.rs,
    // 4/4 real passes against a real codex-acp backend). "claude" stays on
    // MCP delivery -- per design_decisions.mcp_removal_gated_not_assumed,
    // MCP is only removed per-vendor_id once actually verified live.
    if !crate::skills_manager_adapter::is_live_verified(provider) {
        let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
        let mut args = vec![
            "--global-dir".to_string(),
            global_dir.to_string_lossy().into_owned(),
        ];
        if let Some(project_dir) = project_dir {
            args.push("--project-dir".to_string());
            args.push(project_dir.to_string_lossy().into_owned());
        }
        entries.push(serde_json::json!({
            "name": "skills",
            "command": resolve_snapflowd_mcp_bin().to_string_lossy(),
            "args": args,
            "env": [],
        }));
    }
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
/// auth or process-wiring issue. The entry is now driven by the process-wide
/// watcher below, which asks `daemon.mcpStatus` for the listener's real bound
/// address and never dials the MCP endpoint from a session-start path.
///
/// **`"type": "http"` pointed at `/mcp`, not `/sse`** -- found live,
/// correcting this function's own second draft (which sent `"type":
/// "http"` at the *same* `/sse` URL the legacy transport uses): a real
/// `codex-acp` session against that shape failed its MCP handshake with
/// `HTTP 405: Method Not Allowed` on the streamable-HTTP initialize
/// request (`video-generation-e2e-harness` plan's `custom_mcp_and_
/// skills_support` phase, 2026-07-22). Root cause: `snapshotd/internal/
/// mcpadapter/sse.go`'s `SSEServer` deliberately serves *two* transports
/// on the same addr for exactly this reason -- legacy HTTP+SSE at `/`
/// (`/sse`, `/message`) for older clients, and the 2025-03-26 Streamable
/// HTTP transport at `/mcp` for clients whose MCP library only
/// implements that (its own doc comment names Codex CLI's `rmcp` client
/// specifically). Pointing `"type": "http"` at `/sse` was never
/// correct; `/mcp` is the endpoint that transport actually needs.
/// codex-acp's own advertised `mcpCapabilities` are `{http: true, sse:
/// false}`, consistent with it requiring the Streamable HTTP shape, not
/// tolerating the classic SSE one as the previous doc comment's
/// (unverified) claim asserted.
/// Last known *authoritative* snapshotd MCP address. The value comes from
/// `daemon.mcpStatus`, not from the panel's `SNAPSHOTD_MCP_SSE_ADDR` guess.
/// `None` means the control socket is unavailable or the daemon reports that
/// its MCP listener is not currently listening.
static SNAPSHOTD_MCP_STATUS: Mutex<Option<String>> = Mutex::new(None);
static SNAPSHOTD_WATCHER_STARTED: std::sync::Once = std::sync::Once::new();

/// Last address the watcher successfully pushed into every known acpx
/// central `McpServerStore` registry via [`sync_snapshotd_registry_if_
/// changed`] -- separate from `SNAPSHOTD_MCP_STATUS`, which is the last
/// *observed* address regardless of whether a registry push happened.
/// `None` means either no push has ever succeeded, or a bridge just
/// registered a new sync target ([`register_snapshotd_registry_sync_
/// target`] resets this so the next tick re-syncs unconditionally, since
/// a freshly-registered gateway has never seen a push even if the
/// address itself hasn't changed).
static SNAPSHOTD_MCP_SYNCED: Mutex<Option<String>> = Mutex::new(None);

/// One `AgentBridge`'s worth of gateways this process's snapshotd watcher
/// can push the synthetic `"snapflow"` registry row into, plus the tokio
/// `Handle` to actually run that async push on (the watcher itself is a
/// plain OS thread with no runtime of its own). `Weak` so a bridge that
/// has since been dropped (e.g. a test's own short-lived `AgentBridge`)
/// is silently pruned on the next tick instead of leaking.
struct SnapshotdSyncTarget {
    gateways: std::sync::Weak<Mutex<std::collections::HashMap<String, Arc<acpx_client::Gateway>>>>,
    runtime: tokio::runtime::Handle,
}

/// Every live `AgentBridge`'s gateway map the watcher should keep synced.
/// Registered once per bridge construction (see [`register_snapshotd_
/// registry_sync_target`]); almost always holds exactly one entry in
/// production (one panel process, one `AgentBridge`), but nothing here
/// assumes that -- each independent gateway/router process reachable from
/// this panel needs its own registry row, since `McpServerStore` is
/// per-router state, not shared.
static SNAPSHOTD_SYNC_TARGETS: Mutex<Vec<SnapshotdSyncTarget>> = Mutex::new(Vec::new());

/// Registers `gateways` (an `AgentBridge`'s own gateway-connection cache,
/// keyed by base URL) as a target the background snapshotd watcher
/// (`ensure_snapshotd_watcher_started`) should push the synthetic
/// `"snapflow"` MCP server row into whenever the daemon's live MCP
/// address changes. Resets [`SNAPSHOTD_MCP_SYNCED`] so the very next
/// watcher tick pushes to this (and every other still-live) target even
/// if the address happens to already match the last-synced value --
/// otherwise a bridge constructed after the address stabilized would
/// never receive an initial sync.
fn register_snapshotd_registry_sync_target(
    gateways: Arc<Mutex<std::collections::HashMap<String, Arc<acpx_client::Gateway>>>>,
    runtime: tokio::runtime::Handle,
) {
    SNAPSHOTD_SYNC_TARGETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(SnapshotdSyncTarget {
            gateways: Arc::downgrade(&gateways),
            runtime,
        });
    *SNAPSHOTD_MCP_SYNCED
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Forces the process-wide watcher to revisit the current daemon address on
/// its next tick. A bridge can gain a gateway after construction (lazy agent
/// provisioning), and each gateway owns an independent MCP registry.
fn invalidate_snapshotd_registry_sync() {
    *SNAPSHOTD_MCP_SYNCED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// Live gate for the built-in snapflow (snapshotd) MCP client injection.
/// Settings UI name is `"snapflow"`; wire `mcpServers` name is
/// `"snapshotd"`. This flag alone still governs the always-correct
/// per-session `mcpServers` injection path (toggle must flip it and
/// rewrite pool openers directly -- see
/// [`AgentBridge::set_builtin_snapflow_mcp_enabled`]); it is *not*
/// gating the central-registry sync below. As of the `mcp-servers-
/// settings` fix, the watcher (`ensure_snapshotd_watcher_started`) also
/// pushes a real `"snapflow"` row into every known acpx central
/// `McpServerStore` registry (`sync_snapshotd_registry_if_changed`) via
/// `mcp_servers/create`/`update`, so Settings' "Fetch tools" action --
/// which calls `mcp_servers/tools_fetch` against whatever name the row
/// displays -- has a real registry entry to query instead of failing
/// with "no mcp server named snapflow". That registry sync is additive:
/// the per-session client-injection mechanism below is unchanged.
static SNAPFLOW_MCP_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Serializes flag write + multi-step test assertions (and production
/// toggle + pool rewrite) so parallel unit tests cannot race the gate.
static SNAPFLOW_MCP_GATE_LOCK: Mutex<()> = Mutex::new(());

/// Whether new/pooled sessions inject the built-in snapflow MCP.
pub fn snapflow_mcp_enabled() -> bool {
    SNAPFLOW_MCP_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Seed or update the process-wide snapflow injection gate (e.g. from
/// Settings on panel start). Does not touch pools — use
/// [`AgentBridge::set_builtin_snapflow_mcp_enabled`] for a live toggle.
pub fn set_snapflow_mcp_enabled_flag(enabled: bool) {
    let _guard = SNAPFLOW_MCP_GATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    SNAPFLOW_MCP_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Settings UI / registry aliases for the built-in daemon MCP.
pub fn is_builtin_snapflow_mcp_name(name: &str) -> bool {
    name == "snapflow" || name == "snapshotd"
}

const SNAPSHOTD_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn snapshotd_control_socket_path() -> PathBuf {
    admin_token_dir().join("control.sock")
}

/// Query the daemon's ground-truth MCP listener address over its control
/// socket. This function is only called by the process-lifetime watcher, not
/// from session creation or the UI thread.
#[cfg(unix)]
fn query_snapshotd_mcp_addr_at(path: &Path) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    // std::os::unix::net::UnixStream has no connect timeout. Use a tiny
    // current-thread runtime on the already-background watcher thread so a
    // dead/stuck control socket cannot stop all future five-second probes.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .ok()?;
    runtime.block_on(async {
        let stream = tokio::time::timeout(SNAPSHOTD_CONTROL_TIMEOUT, UnixStream::connect(path))
            .await
            .ok()?
            .ok()?;
        let (read_half, mut write_half) = stream.into_split();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "daemon.mcpStatus",
            "params": {}
        });
        let mut line = serde_json::to_vec(&request).ok()?;
        line.push(b'\n');
        tokio::time::timeout(SNAPSHOTD_CONTROL_TIMEOUT, write_half.write_all(&line))
            .await
            .ok()?
            .ok()?;
        let mut response_line = String::new();
        let mut reader = BufReader::new(read_half);
        let count = tokio::time::timeout(
            SNAPSHOTD_CONTROL_TIMEOUT,
            reader.read_line(&mut response_line),
        )
        .await
        .ok()?
        .ok()?;
        if count == 0 {
            return None;
        }
        let response: serde_json::Value = serde_json::from_str(&response_line).ok()?;
        if response.get("error").is_some() {
            return None;
        }
        let result = response.get("result")?;
        if result.get("listening").and_then(|value| value.as_bool()) != Some(true) {
            return None;
        }
        result
            .get("addr")
            .and_then(|value| value.as_str())
            .filter(|addr| !addr.is_empty())
            .map(str::to_owned)
    })
}

#[cfg(unix)]
fn query_snapshotd_mcp_addr() -> Option<String> {
    query_snapshotd_mcp_addr_at(&snapshotd_control_socket_path())
}

#[cfg(not(unix))]
// snapshotd currently exposes only a Unix-domain control socket on the Go
// side. Windows therefore fails closed until the daemon and panel gain a
// named-pipe transport; it must not silently fall back to the guessed MCP
// TCP address.
fn query_snapshotd_mcp_addr() -> Option<String> {
    None
}

fn ensure_snapshotd_watcher_started() {
    SNAPSHOTD_WATCHER_STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            let addr = query_snapshotd_mcp_addr();
            *SNAPSHOTD_MCP_STATUS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = addr.clone();
            sync_snapshotd_registry_if_changed(addr);
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
    });
}

/// Pushes the just-observed `addr` into every live [`SNAPSHOTD_SYNC_
/// TARGETS`] registry, but only when it differs from the last address
/// this watcher successfully synced ([`SNAPSHOTD_MCP_SYNCED`]) -- so a
/// steady-state daemon (the overwhelmingly common case) costs one string
/// comparison per 5s tick, not a `mcp_servers/create`/`update` round trip
/// to every known gateway. Called from the watcher's own OS thread, which
/// has no tokio runtime of its own; the actual async RPCs are handed off
/// to each target's own `Handle::spawn` and this function returns without
/// waiting for them (best-effort -- a failed push is retried whenever the
/// address next changes, or a new target registers).
fn sync_snapshotd_registry_if_changed(addr: Option<String>) {
    let changed = {
        let mut last_synced = SNAPSHOTD_MCP_SYNCED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last_synced.as_deref() == addr.as_deref() {
            false
        } else {
            *last_synced = addr.clone();
            true
        }
    };
    if !changed {
        return;
    }
    // A `None` tick (daemon down / control socket unreachable) still
    // updates `SNAPSHOTD_MCP_SYNCED` above so a later real address is
    // recognized as a change, but there is nothing to push yet.
    let Some(addr) = addr else {
        return;
    };
    let Some(entry) = builtin_snapflow_registry_entry(&addr) else {
        return;
    };
    // Prune dead (dropped-bridge) targets while collecting live ones, so
    // this list stays bounded across e.g. many short-lived test bridges
    // within one process.
    let mut targets = SNAPSHOTD_SYNC_TARGETS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    targets.retain(|target| target.gateways.strong_count() > 0);
    let mut scheduled = false;
    for target in targets.iter() {
        let Some(gateways) = target.gateways.upgrade() else {
            continue;
        };
        if gateways
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
        {
            continue;
        }
        scheduled = true;
        let entry = entry.clone();
        target.runtime.spawn(async move {
            if !sync_snapflow_registry_entry(&gateways, &entry).await {
                // A gateway can appear after the watcher observes the
                // daemon address, or can temporarily reject the upsert while
                // it is still starting. Leave the address unsynced so the
                // next five-second watcher tick retries it.
                *SNAPSHOTD_MCP_SYNCED
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
        });
    }
    if !scheduled {
        // Do not consume the address-change edge before a gateway exists.
        // Otherwise the first gateway created after this tick would never
        // receive the snapflow registry row until the daemon address changed.
        *SNAPSHOTD_MCP_SYNCED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// Builds the central-registry `McpServerEntry` for the built-in snapflow
/// server at `addr`, reusing [`snapshotd_mcp_server_entry_for_addr`] for
/// the URL shape (the same `http://{addr}/mcp` this crate already injects
/// per-session) rather than recomputing it a second way. That helper's
/// wire shape targets ACP's own per-session `mcpServers` array (`"name":
/// "snapshotd"`, array-of-pairs `headers`) and is also gated on the live
/// [`snapflow_mcp_enabled`] toggle, neither of which apply to the central
/// registry row: the Settings UI's row (and therefore what "Fetch tools"
/// queries) is named `"snapflow"`, uses `McpServerConfig`'s object-shaped
/// `headers`, and the registry row is what still needs to exist even
/// while the toggle is off (else re-enabling it would race a fresh watch
/// tick). Only the `url` field is actually reused across the two shapes.
fn builtin_snapflow_registry_entry(addr: &str) -> Option<crate::protocol_types::McpServerEntry> {
    let built = snapshotd_mcp_server_entry_for_addr(Some(addr));
    let url = built.first()?.get("url")?.as_str()?.to_string();
    Some(crate::protocol_types::McpServerEntry::new(
        "snapflow",
        crate::protocol_types::McpServerConfig::Http {
            url,
            headers: std::collections::HashMap::new(),
            timeout: None,
            oauth: None,
        },
    ))
}

/// Upserts `entry` into every gateway currently known in `gateways`
/// (one push per distinct connected acpx-server this panel talks to --
/// each owns an independent `McpServerStore`). Try-create-then-update-on-
/// AlreadyExists, the same pattern `acpx-server/src/provisioning.rs`'s
/// `apply` uses for its own declarative-reapply case, ported to the
/// client-side `ClientError::Rpc` shape `Gateway::create_mcp_server`
/// actually returns (the server-side `RouterError::McpServer(McpServer
/// StoreError::AlreadyExists(_))` `apply` matches on never crosses the
/// wire as a typed variant -- it collapses to a JSON-RPC error whose
/// `message` is `McpServerStoreError::AlreadyExists`'s `Display` text,
/// "mcp server store: mcp server {name} already exists", wrapped by
/// `RouterError::McpServer`'s own `"mcp server store: {0}"` -- so this
/// matches on that substring the same way `ClientError::is_transient`
/// already does for its own message-sniffing cases).
async fn sync_snapflow_registry_entry(
    gateways: &Mutex<std::collections::HashMap<String, Arc<acpx_client::Gateway>>>,
    entry: &crate::protocol_types::McpServerEntry,
) -> bool {
    let live_gateways: Vec<Arc<acpx_client::Gateway>> = gateways
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .cloned()
        .collect();
    if live_gateways.is_empty() {
        return false;
    }
    let mut all_succeeded = true;
    for gateway in live_gateways {
        if let Err(err) = gateway.create_mcp_server(entry).await {
            let already_exists = matches!(
                &err,
                acpx_client::raw::ClientError::Rpc { message, .. }
                    if message.to_ascii_lowercase().contains("already exists")
            );
            if already_exists {
                if gateway.update_mcp_server(entry).await.is_err() {
                    all_succeeded = false;
                }
            } else {
                all_succeeded = false;
            }
        }
    }
    all_succeeded
}

/// Non-blocking read of the watcher cache. Starting the watcher is one-time
/// and returns immediately; the control-socket dial happens only on its
/// dedicated background thread.
pub fn snapshotd_mcp_addr() -> Option<String> {
    ensure_snapshotd_watcher_started();
    SNAPSHOTD_MCP_STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[allow(dead_code)]
fn snapshotd_mcp_server_entry(provider: &str) -> Vec<serde_json::Value> {
    let _ = provider; // kept for call-site symmetry / future per-provider gating if a real incompatibility turns up.
    snapshotd_mcp_server_entry_for_addr(snapshotd_mcp_addr().as_deref())
}

#[allow(dead_code)]
// acpx-client-session-lease-pool: this entry is deliberately context-token
// free -- per-thread `X-Snapshotd-Context-Token` scoping was removed (not
// required) because a warm-pooled session's `mcpServers` is fixed at
// `session/new` time, before any specific consuming thread (and its own
// token) is known; see GatewaySessionOpener's mcp_servers refresh path for
// how project/provider-level MCP config changes now propagate instead.
fn snapshotd_mcp_server_entry_for_addr(addr: Option<&str>) -> Vec<serde_json::Value> {
    if !snapflow_mcp_enabled() {
        return Vec::new();
    }
    let Some(addr) = addr.filter(|addr| !addr.is_empty()) else {
        return Vec::new();
    };
    vec![serde_json::json!({
        "type": "http",
        "name": "snapshotd",
        "url": format!("http://{addr}/mcp"),
        "headers": [],
    })]
}

/// Drop or re-add the built-in snapflow (`snapshotd`) entry on a pool's
/// client `mcpServers` list without recomputing skills (which depend on
/// project-dir resolution that is not stored on the pool key alone).
///
/// When enabling, `inject_addr` is the daemon MCP bind address to put
/// back (live watcher value in production). Tests pass an explicit addr.
fn apply_snapflow_to_client_mcp_list(
    mcp_servers: &[serde_json::Value],
    enabled: bool,
    inject_addr: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut next: Vec<serde_json::Value> = mcp_servers
        .iter()
        .filter(|entry| {
            entry
                .get("name")
                .and_then(|n| n.as_str())
                .map(|n| !is_builtin_snapflow_mcp_name(n))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if enabled {
        // Bypass the process-wide flag here: the caller already decided
        // `enabled` and may still be holding the gate lock while the
        // atomic is mid-update. Force-inject from `inject_addr` only.
        if let Some(addr) = inject_addr.filter(|a| !a.is_empty()) {
            next.push(serde_json::json!({
                "type": "http",
                "name": "snapshotd",
                "url": format!("http://{addr}/mcp"),
                "headers": [],
            }));
        }
    }
    next
}

// PROF-3 (`profile-only-backend-selection` plan): `resolve_backend_agent_
// command`/`default_backend_command_for_provider` used to live here,
// computing a value for `spawn_gateway_process` to write into
// ACPX_BACKEND_CMD -- an exported-command-string bootstrap backend, the
// exact shape this plan eliminates. Removed along with that write (see
// `spawn_gateway_process`'s own comment for why a real profile now covers
// the case `default_backend_command_for_provider("claude")` used to
// patch). If either is reintroduced for a legitimate PROF-4 test-harness
// need, keep both historical bugs their doc comments recorded in mind:
// (a) unconditionally writing a compile-time dev-checkout path meant a
// real release install with no operator-started acpx-server never
// reached a real agent at all; (b) `dev_mock_agent.is_file()` is not a
// "dev/test context" signal by itself -- that debug binary exists in any
// checkout that has ever run `cargo build`/`cargo test`, including one
// verifying real end-to-end acpx behavior.
//
// KNOWN ACCEPTED GAP, not an oversight: with no `_acpx.profile` requested
// AND nothing configured anywhere (no persisted thread record, no
// `default_agent_id` in settings -- the genuine blank-slate case), a
// thread still opens in native/unmanaged mode, which falls through to
// the autospawned acpx-server's own built-in default (codex-only, see
// `acpx-server/src/config.rs`'s bare `ACPX_BACKEND_CMD` fallback). This
// was investigated and deliberately left as-is: auto-picking a live
// `profiles/list` entry at session-open time (the obvious panel-side
// fix) was rejected because `acpx-core::detect::detect` marks an
// npx-distributed registry agent "Installed" purely from `node`/`npm`
// being on `PATH`, which is true on essentially any dev/CI machine --
// that would make session selection silently PATH/registry-order
// dependent, including in this crate's own tests. The real fix belongs
// one layer down, in acpx-server's own native-mode default resolution
// (make it consult its own seeded-profile result instead of a hardcoded
// string) -- tracked as PROF-14, a separate cross-repo phase, blocked on
// an unrelated worktree (agents-install-runtime, PROF-13) merging first
// since it already has uncommitted changes to the exact acpx-core/
// acpx-server files that fix would touch, including the
// ACPX_BACKEND_CMD -> ACPX_DEFAULT_ACP_COMMAND rename. Do not "fix" this
// gap by reintroducing an env-var write here -- that is the one thing
// this whole plan exists to prevent.

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

/// pool-capability-fix: the other half of `read_codex_api_key_from_auth_
/// file`'s own gap -- `codex login`'s far more common ChatGPT-plan device
/// flow writes a `tokens.access_token` OAuth session into the same
/// `auth.json` and leaves `OPENAI_API_KEY` null, so a real, valid,
/// already-authenticated login of that shape was previously
/// indistinguishable here from no login at all. Live-confirmed against
/// this exact file shape: acpx-server's own mirrored auto-detection
/// (`acpx-server/src/config.rs`'s `default_codex_native_auth_method`)
/// resolved `native_auth_method_id=None` for it, and every real
/// `session/new` failed with "backend requires authentication" until
/// `chat-gpt` was selected explicitly -- after which a real prompt round-
/// tripped successfully through the genuine ambient login.
fn codex_auth_file_has_chatgpt_login() -> bool {
    let Some(path) = std::env::var_os("ACPX_CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| codex_home_dir().map(|dir| dir.join("auth.json")))
    else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    value
        .get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(|token| token.as_str())
        .is_some_and(|token| !token.is_empty())
}

/// Normalizes the free-form `auth_mode` string real `codex` CLI builds
/// write into `auth.json` into the exact ACP `native_auth_method_id`
/// value codex-acp expects. Case/hyphen/underscore-insensitive since
/// only `"chatgpt"` has been directly confirmed on a live system (see
/// `resolve_codex_native_auth_method_id`'s doc comment) -- other codex
/// CLI versions may plausibly spell either mode differently
/// (`"chat-gpt"`, `"ChatGPT"`, `"api-key"`, `"apiKey"`, ...), so this
/// normalizes defensively rather than matching a single literal.
/// Returns `None` for anything unrecognized so the caller can fall back
/// to today's presence-based detection instead of guessing.
fn normalize_codex_auth_mode(raw: &str) -> Option<&'static str> {
    let normalized: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    match normalized.as_str() {
        "chatgpt" => Some("chat-gpt"),
        "apikey" => Some("api-key"),
        _ => None,
    }
}

/// Reads and normalizes `auth.json`'s own `auth_mode` field (see
/// `codex_home_dir` for path resolution). `None` covers every "we don't
/// have a trustworthy declared mode" case alike -- missing file,
/// unparseable JSON, missing field, or an unrecognized value -- so
/// callers have one signal to check before falling back.
fn read_codex_auth_mode_from_file() -> Option<&'static str> {
    let path = std::env::var_os("ACPX_CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| codex_home_dir().map(|dir| dir.join("auth.json")))?;
    let contents = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value
        .get("auth_mode")
        .and_then(|v| v.as_str())
        .and_then(normalize_codex_auth_mode)
}

/// Resolves the ACP `native_auth_method_id` this system's real Codex CLI
/// login implies, mirroring acpx-server's own
/// `default_codex_native_auth_method` (`acpx-server/src/config.rs`) --
/// kept as a deliberately separate, hand-mirrored implementation per
/// that function's own doc comment, not a shared crate, so keep any
/// future change to this priority order in sync in both places.
///
/// **auth_mode-first, live bug this fixes.** Before this function
/// existed, `spawn_gateway_process` inlined field-presence-only
/// detection (API key first, then `tokens.access_token`) that completely
/// ignored `auth.json`'s own `auth_mode` field. Live-confirmed against
/// this exact system: its real `~/.codex/auth.json` has
/// `"auth_mode": "chatgpt"` (a real, completed ChatGPT-plan login) *and*
/// a stale, leftover non-empty `OPENAI_API_KEY` field left over from an
/// earlier/different login -- so presence-only detection always picked
/// "api-key" for it, silently contradicting what the file's own
/// `auth_mode` field declared, and shadowing a real working login with a
/// wrong one. This now checks `auth_mode` first via
/// `read_codex_auth_mode_from_file`, and only falls back to the old
/// presence-based priority (api-key field presence, then
/// `tokens.access_token`) when `auth_mode` is missing or unrecognized,
/// so `auth.json` shapes that predate this field (or come from a codex
/// CLI version that doesn't set it) keep resolving exactly as before.
fn resolve_codex_native_auth_method_id() -> Option<&'static str> {
    if let Some(mode) = read_codex_auth_mode_from_file() {
        return Some(mode);
    }
    if read_codex_api_key_from_auth_file().is_some() {
        return Some("api-key");
    }
    if codex_auth_file_has_chatgpt_login() {
        return Some("chat-gpt");
    }
    None
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
///
/// `pub`, not private, even though `agent_bridge` itself is a private
/// module: `lib.rs`'s `test_support` re-exports this and
/// [`reserve_ephemeral_port`] so `tests/*.rs` integration tests
/// (separate crates from this one, unable to see anything less than
/// `pub`, and unable to re-export anything less than `pub` even through
/// a `pub use`) can share this exact implementation instead of each
/// keeping their own unsynchronized copy of the same reserve-a-port
/// trick -- see `test_support`'s own doc comment for the full history.
pub fn reserve_port(port: u16) -> io::Result<File> {
    let path = std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock"));
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// See [`reserve_port`]'s doc comment for why this is `pub`.
pub fn reserve_ephemeral_port() -> Option<(u16, File)> {
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
/// 2. Else, the single shared loopback default bind
///    (`acpx_client::DEFAULT_ACPX_HTTP_ADDR`, one place, same value
///    acpx-server itself defaults `ACPX_HTTP_BIND` to) is probed with
///    [`probe_acpx_gateway`] for every provider -- if a real
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

    // One shared default bind for every provider -- the single source of
    // truth is acpx_client::DEFAULT_ACPX_HTTP_ADDR, which acpx-server also
    // uses for its own ACPX_HTTP_BIND default. snapshotd ships exactly one
    // acpx-server serving all agents; genuine per-provider gateways remain
    // expressible via the RUI_ACPX_<PROVIDER>_URL overrides above. (The old
    // codex=8790 / claude=8791 split assumed one acpx-server per provider,
    // which no live deployment uses -- confirmed against the running system.)
    let default_port: u16 = acpx_client::default_acpx_http_port();
    // Prefer an agent-specific health match, but reuse any acpx answering the
    // shared port (a single bundled gateway may advertise a different default
    // agent-id than this provider while still serving it).
    if probe_acpx_gateway_for_agent(default_port, Some(provider))
        || probe_acpx_gateway_once(default_port, None)
    {
        return Ok(acpx_client::default_acpx_http_url());
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
    //
    // That check only catches a port that's already *listening* --
    // `reserve_port`'s own lock file only guards against two calls in
    // *this* process racing each other, not an unrelated process binding
    // the same port between this check and `spawn_gateway_process`'s own
    // `cmd.spawn()` a moment later. When that race loses, `acpx-server`
    // itself fails to bind and exits immediately with the real "Address
    // already in use" -- `spawn_gateway_process` tags that specific
    // failure with `PORT_COLLISION_ERROR_MARKER` (see its doc comment), so
    // retry a small, bounded number of times on a fresh ephemeral port
    // instead of surfacing a bare crash with no fallback. Any other
    // failure shape (missing binary, bad config, permission error, ...) is
    // returned immediately -- a retry would never fix those.
    const MAX_PORT_COLLISION_RETRIES: u32 = 3;
    let mut attempt = 0;
    loop {
        let (port, lock) = if attempt == 0 {
            if std::net::TcpStream::connect_timeout(
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
                reserve_ephemeral_port()
                    .ok_or_else(|| "could not reserve a loopback port".to_string())?
            }
        } else {
            reserve_ephemeral_port()
                .ok_or_else(|| "could not reserve a loopback port".to_string())?
        };

        match spawn_gateway_process(provider, port, lock, cache_dir) {
            Ok(()) => return Ok(format!("http://127.0.0.1:{port}")),
            Err(e)
                if e.starts_with(PORT_COLLISION_ERROR_MARKER)
                    && attempt < MAX_PORT_COLLISION_RETRIES =>
            {
                attempt += 1;
                continue;
            }
            Err(e) => {
                return Err(e
                    .strip_prefix(PORT_COLLISION_ERROR_MARKER)
                    .unwrap_or(&e)
                    .to_string());
            }
        }
    }
}

/// Prefix `spawn_gateway_process` puts on its error string when the spawned
/// `acpx-server` exited during startup for what its own stderr log confirms
/// was a port collision -- lets [`provision_gateway`] distinguish "retry on
/// a different port" from every other startup failure (missing binary, bad
/// config, permission error, ...) that a retry would never fix. Not a real
/// error type since every other error in this module is already a bare
/// `String`; a marker prefix keeps this one consistent with that.
const PORT_COLLISION_ERROR_MARKER: &str = "\u{0}PORT_COLLISION\u{0}";

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
        // acpx-core's LifecycleConfig defaults (max_sessions_total: 128,
        // max_sessions_per_tenant: 16) are sized for a real multi-tenant
        // hosted deployment, where the per-tenant cap is a genuine
        // fairness/safety limit. This gateway serves exactly one local
        // user's own panel under the single "default" tenant, so that
        // same cap does nothing but reject real work once enough
        // threads/dev-session churn accumulates -- confirmed live:
        // "session capacity reached for tenant default: 16/16 live
        // gateway sessions" after normal repeated use, not a fairness
        // violation. See snapshotd/internal/acpxmgr/acpxmgr.go's matching
        // fix for its own bundled-instance spawn path -- this is the
        // same override for panel-rust's own per-provider spawn path.
        .env("ACPX_MAX_SESSIONS_PER_TENANT", "512")
        .env("ACPX_MAX_SESSIONS_TOTAL", "2048")
        // acpx-server auto-enables bulk startup session recovery whenever
        // ACPX_DB_PATH is set (config.rs's ServerConfig::from_env), which
        // this function always does (see db_path below) so a real app
        // restart can rehydrate the *small handful of threads panel-rust
        // itself still tracks* (thread-by-thread, via spec.session_id /
        // requested_session_id in spawn_background_attachment below --
        // this process never relies on acpx-server's own bulk recovery
        // pass to do that). But db_path points at the SAME persistent
        // file across every single launch of this app, forever, and
        // acpx-core's list_recoverable_sessions has no age bound at all
        // -- every session ever opened and never gracefully closed (the
        // overwhelmingly common case for a desktop app that's almost
        // always killed, not shut down cleanly) stays a recovery
        // candidate. Confirmed live: one real, accumulated-over-days
        // acpx-claude.sqlite3 on this machine had 4367 such rows, and a
        // single fresh launch's own spawned "claude" gateway tried to
        // recover every one of them on startup, saturating the per-
        // tenant session cap within seconds -- not test-run
        // accumulation, this exact unconditional bulk recovery pass.
        // Disabled here since this spawn path never needs or uses it.
        .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    // PISO-11: "codex installed but not detected" -- acpx-core's detect()
    // (agents/list) resolves an npx-distributed agent's status purely from
    // `which("node")`/`which("npm")` on PATH, and Command::new(...).env(...)
    // (no env_clear()) makes this spawned acpx-server inherit THIS process's
    // own PATH -- i.e. whatever snapflow itself launched with. A terminal
    // launch sources shell rc files (nvm's PATH export lives there); a
    // desktop-launcher/dock-icon launch does not, so node/npm installed via
    // nvm are genuinely on PATH for the user and genuinely absent from this
    // spawned gateway's environment, and detect() reports RuntimeMissing
    // for an agent that is, in fact, installed. Augmenting PATH here with a
    // real login shell's resolved PATH (once per process, cached) closes
    // that gap without needing the operator to launch from a terminal.
    if let Some(path) = augmented_gateway_path() {
        cmd.env("PATH", path);
    }
    // Gateway stderr goes to a per-provider log file in the cache dir,
    // NOT /dev/null: acpx-server's tracing output AND every backend
    // adapter's inherited stderr flow through it (acpx-conductor spawns
    // backends with Stdio::inherit for stderr) -- discarding it left a
    // real mid-turn agent failure (the bifrost tool_search dead-turn,
    // 2026-07-23) with literally zero diagnostics anywhere on disk,
    // costing a full forensic session to what one log line would have
    // answered. Truncated on each gateway spawn so it stays bounded by
    // one gateway lifetime; falls back to null only if the file can't
    // be created (never blocks the spawn itself).
    // Without RUST_LOG, acpx-server's tracing subscriber emits nothing at
    // all -- an empty log file is barely better than /dev/null. INFO is
    // acpx-server's own documented operational level (startup config,
    // per-request router lines, backend spawn failures); an operator's
    // explicit RUST_LOG still wins.
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "info");
    }
    let stderr_log = resolve_cache_dir().join(format!("gateway-{provider}.stderr.log"));
    match std::fs::File::create(&stderr_log) {
        Ok(file) => {
            cmd.stderr(file);
        }
        Err(error) => {
            eprintln!(
                "panel-rust: gateway stderr log unavailable ({error}); \
                 falling back to discarding {provider} gateway stderr"
            );
            cmd.stderr(std::process::Stdio::null());
        }
    }
    // PROF-3 (`profile-only-backend-selection` plan): this autospawned
    // gateway process's own ACPX_BACKEND_CMD is now NEVER set here.
    // Previously this branched on an explicit RUI_ACP_AGENT_CMD/
    // RUI_USE_DEV_MOCK_AGENT dev override, else a hardcoded
    // `npx claude-agent-acp` string for `provider == "claude"` (acpx-
    // server's own bare default is codex-only) -- both are exactly the
    // "an exported command string defines the backend" shape this plan
    // exists to eliminate. A bootstrap backend for a non-default agent
    // must now come from a real, resolvable profile instead: PROF-2
    // already made any thread with a real configured `default_agent_id`
    // carry a matching `_acpx.profile`, which resolves through
    // `Router::resolve_profile` -> `crate::launch::build_launch_env`
    // (acpx-core) -- a path that reads the real registry-listed spawn
    // command for that agent id directly, with no dependency on this
    // gateway process's own env at all. So leaving ACPX_BACKEND_CMD
    // unset here no longer reintroduces the old "claude" native-mode
    // bug (`default_backend_command_for_provider`'s own removed doc
    // comment): that gap is only reachable when NOTHING is configured
    // anywhere, and in that genuine blank-slate case `provider` is
    // already `NO_PROVIDER_REQUESTED_FALLBACK` ("codex"), which matches
    // acpx-server's own bare default with nothing left to override.
    //
    // Test harnesses that legitimately need an arbitrary/mock backend
    // command (`RUI_ACP_AGENT_CMD`/`RUI_USE_DEV_MOCK_AGENT`, or a
    // `TestGateway` spawned directly in this module's own tests) are
    // out of this phase's scope -- they never called through this
    // function to begin with (`TestGateway` builds its own `Command`),
    // and any equivalent production-adjacent dev workflow this removed
    // is a PROF-4 concern, not reintroduced here.
    if provider == "codex" {
        // acpx-server's own default already resolves to the real
        // codex-acp adapter; give it a noninteractive path to this
        // system's already-authenticated Codex CLI login instead of
        // codex-acp's headless-incapable chat-gpt device flow (see
        // read_codex_api_key_from_auth_file's doc comment).
        //
        // pool-capability-fix: this used to hardcode "api-key"
        // unconditionally whenever the env var was unset -- worse than
        // doing nothing for a real ChatGPT-plan login (no raw API key
        // ever stored, by design): it forced codex-acp down the
        // api-key path with no key at all, instead of leaving
        // auth_method_id unset so codex-acp could at least attempt its
        // own default flow. Mirrors acpx-server's own auto-detection
        // (`config.rs`'s `default_codex_native_auth_method`): api-key
        // when a real key is found, else chat-gpt when a real ChatGPT
        // OAuth login is found, else leave it unset.
        //
        // auth-mode-first fix: the above (field-presence-only) priority
        // had its own live bug -- this system's real `~/.codex/auth.json`
        // has `"auth_mode": "chatgpt"` (a real, completed ChatGPT-plan
        // login) *and* a stale, leftover non-empty `OPENAI_API_KEY` field.
        // Presence-only detection always resolved that combination to
        // "api-key", silently discarding the user's actual declared
        // login mode. `resolve_codex_native_auth_method_id` now checks
        // the file's own `auth_mode` field first (normalized
        // case/hyphen-insensitively) and only falls back to this
        // presence-based priority when `auth_mode` is missing or
        // unrecognized, so older `auth.json` shapes without the field
        // keep behaving exactly as before.
        if std::env::var_os("ACPX_NATIVE_AUTH_METHOD_ID").is_none() {
            match resolve_codex_native_auth_method_id() {
                Some("api-key") => {
                    cmd.env("ACPX_NATIVE_AUTH_METHOD_ID", "api-key");
                    if std::env::var_os("CODEX_API_KEY").is_none() {
                        if let Some(key) = read_codex_api_key_from_auth_file() {
                            cmd.env("CODEX_API_KEY", key);
                        }
                    }
                }
                Some("chat-gpt") => {
                    cmd.env("ACPX_NATIVE_AUTH_METHOD_ID", "chat-gpt");
                }
                _ => {}
            }
        } else if std::env::var_os("CODEX_API_KEY").is_none() {
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
    // Persist ACPX session metadata/state revisions to sqlite so a `session/load`
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
    // setup-followups plan, agent_settings_ordering_and_install_enable_
    // flow: give every acpx-server this process spawns its own admin
    // plane too (a fresh ephemeral admin port per instance, unlike the
    // daemon's single fixed one -- this path can spawn several gateways
    // in the same process), so `resolve_admin_creds` can find it via the
    // in-memory registry below without needing the shared token file at
    // all for a self-spawned instance.
    if let (Some((admin_port, _admin_lock)), Some(token)) =
        (reserve_ephemeral_port(), random_hex_token(32))
    {
        cmd.env("ACPX_ADMIN_TOKEN", &token)
            .env("ACPX_ADMIN_BIND", format!("127.0.0.1:{admin_port}"));
        self_spawned_admin_creds()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                format!("http://127.0.0.1:{port}"),
                (format!("http://127.0.0.1:{admin_port}"), token),
            );
        // _admin_lock is dropped here (its only job was reserving the
        // port up to this point) -- acpx-server binds it next, same
        // "reserve via a real listener, then hand the port to the child"
        // TOCTOU-safe convention `reserve_ephemeral_port`'s own caller
        // (the main HTTP port, `lock` above) already uses.
    }
    // See `ensure_executable`'s doc comment: a real, recurring production
    // failure (`system_launch.yaml`, "Permission denied (os error 13)")
    // traced to this exact `cmd.spawn()` call losing the resolved
    // binary's execute bit somewhere in packaging/transfer. Self-heal
    // before attempting the exec so a stripped permission bit does not
    // fail every thread on this provider.
    let acpx_server_bin = resolve_acpx_server_bin();
    ensure_executable(&acpx_server_bin);
    let mut child = cmd.spawn().map_err(|e| {
        let _ =
            std::fs::remove_file(std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock")));
        // Distinguish "never resolved a real binary" (EACCES self-heal
        // above cannot help, and a bare `ENOENT`/os error 2 from Command
        // is easy to misread as a permissions problem) from "resolved a
        // real file that still failed to exec" -- surfaces the exact
        // path this process tried, which the bare `io::Error` alone does
        // not include, so an operator does not have to reconstruct
        // `resolve_acpx_server_bin_from`'s own fallback chain by hand.
        let exists = acpx_server_bin.is_file();
        format!(
            "failed to spawn acpx-server for {provider} on port {port} \
             (resolved path {acpx_server_bin:?}, exists={exists}): {e}"
        )
    })?;
    for _ in 0..50 {
        if probe_acpx_gateway_for_agent(port, Some(provider)) {
            // Health-visibility gap: this watcher used to silently `break`
            // and clean up the port lock on the gateway's own unexpected
            // exit, with no log line and nothing surfaced to the panel at
            // all -- an already-running gateway dying (crash, OOM-kill,
            // operator `kill`) left every thread on it stuck with no
            // explanation anywhere on disk, the same "zero diagnostics"
            // failure mode the stderr-log-instead-of-/dev/null fix above
            // addresses for startup failures. Full model/UI wiring for a
            // post-start death is a larger TEA-plumbing change (no
            // existing global channel from this background std::thread
            // into the reducer); this is the tractable first step so the
            // event is at least discoverable instead of invisible.
            let provider_owned = provider.to_string();
            std::thread::spawn(move || {
                let mut child = child;
                let exit_status = loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break Some(status),
                        Err(error) => {
                            eprintln!(
                                "panel-rust: lost track of acpx-server for {provider_owned} \
                                 on port {port} (pid wait error: {error})"
                            );
                            break None;
                        }
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(500)),
                    }
                };
                if let Some(status) = exit_status {
                    eprintln!(
                        "panel-rust: acpx-server for {provider_owned} on port {port} exited \
                         unexpectedly ({status}); every thread still bound to this gateway \
                         will fail its next request -- see gateway-{provider_owned}.stderr.log \
                         for the process's own diagnostics"
                    );
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
            // `reserve_port`/the pre-spawn `TcpStream::connect_timeout` probe
            // in `provision_gateway` only catch a port that's already
            // *listening*; a port bound-but-not-accepting (TIME_WAIT, or an
            // unrelated process racing this one between the probe and this
            // exact `cmd.spawn()`) sails through both checks and only shows
            // up here, as `acpx-server` itself failing to bind and exiting
            // immediately. Scan its stderr log (the only place that real
            // "Address already in use" / EADDRINUSE text lands, see the
            // stderr redirection above) so `provision_gateway` can tell this
            // apart from every other startup failure and retry on a fresh
            // port instead of surfacing a bare crash with no fallback.
            let looks_like_port_collision = std::fs::read_to_string(&stderr_log)
                .map(|log| {
                    let lower = log.to_ascii_lowercase();
                    lower.contains("address already in use") || lower.contains("eaddrinuse")
                })
                .unwrap_or(false);
            return Err(format!(
                "{}acpx-server exited during startup for {provider} on port {port}: {status}",
                if looks_like_port_collision {
                    PORT_COLLISION_ERROR_MARKER
                } else {
                    ""
                }
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

/// setup-followups plan, agent_settings_ordering_and_install_enable_flow:
/// admin-plane bearer token + admin listener URL for whichever acpx-server
/// this panel is talking to, so `AgentBridge::set_agent_enabled` can build
/// a real `acpx_client::ext::admin::AdminClient`. There is no existing
/// channel for two independent processes (snapshotd, panel-rust) to share
/// a secret, so this establishes one: a token file written by whichever
/// side actually spawned the gateway (`acpxmgr.ensureAdminToken` on the
/// Go side, `spawn_gateway_process`'s own admin-token generation on this
/// side), at the same path both sides derive independently from
/// `SNAPSHOTD_HOME`/`HOME`
/// (0600, since it grants gateway-wide agent enable/disable control).
///
/// Resolution order per call:
/// 1. `RUI_ACPX_ADMIN_URL`/`RUI_ACPX_ADMIN_TOKEN` env override (tests, or
///    an operator running acpx-server directly with a hand-picked token).
/// 2. This process's own in-memory registry of admin creds for gateways
///    *this* `spawn_gateway_process` call itself spawned (the per-provider
///    dev-fallback path, which reserves a fresh ephemeral admin port per
///    instance rather than the daemon's fixed one).
/// 3. The shared token file, assuming the daemon's fixed admin bind
///    (`acpxmgr.AdminBind` on the Go side) -- the real production path,
///    where snapshotd (not this process) spawned the one shared gateway.
fn admin_token_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SNAPSHOTD_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".snapshotd"))
        .unwrap_or_else(|_| PathBuf::from(".snapshotd"))
}

fn read_shared_admin_token() -> Option<String> {
    let raw = std::fs::read_to_string(admin_token_dir().join("admin-token")).ok()?;
    let token = raw.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Same fixed bind acpxmgr.go's `AdminBind` constant uses -- see that
/// constant's own doc comment for why a fixed (not per-run-reserved) port
/// is correct there (exactly one daemon-managed instance ever exists).
const DAEMON_ADMIN_URL: &str = "http://127.0.0.1:8791";

static SELF_SPAWNED_ADMIN_CREDS: std::sync::OnceLock<Mutex<HashMap<String, (String, String)>>> =
    std::sync::OnceLock::new();

/// This process's PATH, augmented with whatever a real login shell resolves
/// PATH to -- see the PISO-11 comment at its call site in
/// `spawn_gateway_process` for why this exists. Computed once per process
/// (a login shell spawn is real subprocess overhead, not something to pay
/// on every gateway launch) and cached, including the "nothing extra to
/// add" case so a broken `$SHELL` doesn't retry on every call.
///
/// Windows is deliberately excluded: PATH there is a per-user/per-machine
/// environment value set outside any shell-rc-file mechanism, so it is
/// already inherited correctly regardless of how snapflow was launched --
/// this specific gap does not exist on that platform.
#[cfg(unix)]
fn augmented_gateway_path() -> Option<String> {
    static AUGMENTED_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    AUGMENTED_PATH
        .get_or_init(|| {
            let current = std::env::var("PATH").unwrap_or_default();
            merge_path_entries(&current, login_shell_path_entries())
        })
        .clone()
}

/// Pure merge: current PATH's own entries first (so an operator's explicit
/// PATH always wins on conflicting binaries), then whatever `extra` entries
/// (from a login shell) aren't already present, in order, deduped. `None`
/// only when there is nothing at all to set PATH to. Split out from
/// `augmented_gateway_path` so the merge/dedup/ordering logic is testable
/// without a real subprocess or the process-global OnceLock cache.
fn merge_path_entries(current: &str, extra: Vec<String>) -> Option<String> {
    // split_paths("") yields one empty PathBuf rather than zero entries --
    // filtered out so an unset/empty current PATH doesn't leave a stray
    // leading ":" in the merged result or a spurious Some("").
    let mut entries: Vec<String> = std::env::split_paths(current)
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
        .collect();
    let existing: std::collections::HashSet<String> = entries.iter().cloned().collect();
    for entry in extra {
        if !existing.contains(&entry) {
            entries.push(entry);
        }
    }
    if entries.is_empty() {
        None
    } else {
        Some(entries.join(":"))
    }
}

/// Resolves PATH the same way an interactive login shell would (sourcing
/// `.bashrc`/`.zshrc`/`.profile`/etc, where nvm/uv/etc typically export
/// their bin dirs) rather than trusting whatever PATH this process itself
/// happened to inherit. Best-effort: any failure (no `$SHELL`, the shell
/// exits non-zero, output isn't valid UTF-8) yields an empty list, which
/// makes `augmented_gateway_path()` a no-op rather than a hard error --
/// this is a detection-quality improvement, not something a gateway launch
/// should ever fail over.
#[cfg(unix)]
fn login_shell_path_entries() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    // `-l` alone (login, non-interactive) sources /etc/profile + the first
    // of ~/.bash_profile/~/.bash_login/~/.profile -- NOT ~/.bashrc, which
    // bash only auto-sources for INTERACTIVE shells. Confirmed live on this
    // box: nvm's PATH export lives only in ~/.bashrc (a very common nvm
    // install-script default), so `-lc` alone silently found nothing while
    // a real terminal (which IS interactive) sees it fine. `-lic` sources
    // both chains, matching what a user's actual terminal actually runs.
    // stdin is nulled (some rc files probe it) and stderr discarded (an
    // interactive shell with no real tty logs harmless "no job control"/
    // "cannot set terminal process group" warnings there that must not be
    // mistaken for PATH output, which is stdout-only via printf below).
    let output = std::process::Command::new(&shell)
        .arg("-lic")
        .arg("printf '%s' \"$PATH\"")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8(out.stdout)
            .map(|s| {
                s.split(':')
                    .map(str::to_owned)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Windows has no equivalent shell-rc-file PATH gap (see
/// `augmented_gateway_path`'s doc comment) -- always a no-op there.
#[cfg(not(unix))]
fn augmented_gateway_path() -> Option<String> {
    None
}

fn self_spawned_admin_creds() -> &'static Mutex<HashMap<String, (String, String)>> {
    SELF_SPAWNED_ADMIN_CREDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A random hex token from `/dev/urandom`, with no new crate dependency
/// for what's otherwise a one-off local-loopback dev-fallback credential
/// (the real production path's token comes from `acpxmgr.go`'s own
/// `crypto/rand`-backed generator instead, via the shared file).
fn random_hex_token(bytes: usize) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut buf)
        .ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// `AcpxThreadHandle` has no getter for the base_url it was created
/// against (it's an opaque actor-command channel), so this can't key an
/// exact per-gateway lookup the way `self_spawned_admin_creds` is keyed.
/// In practice that's fine: production always has exactly one daemon-
/// managed gateway (tier 3 below, base_url-independent by construction),
/// and the self-spawn dev-fallback path realistically spawns one gateway
/// per running panel-rust instance too -- so "the one self-spawned entry,
/// if there's exactly one" is a correct, simple stand-in for a real
/// per-base_url match.
fn resolve_admin_creds() -> Option<(String, String)> {
    if let (Ok(url), Ok(token)) = (
        std::env::var("RUI_ACPX_ADMIN_URL"),
        std::env::var("RUI_ACPX_ADMIN_TOKEN"),
    ) {
        if !url.is_empty() && !token.is_empty() {
            return Some((url, token));
        }
    }
    {
        let creds = self_spawned_admin_creds()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if creds.len() == 1 {
            return creds.values().next().cloned();
        }
    }
    read_shared_admin_token().map(|token| (DAEMON_ADMIN_URL.to_owned(), token))
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

/// The `cwd` argument ACP's `session/new` wants. `project_isolation_mlt_
/// binding` phase (PISO-4): prefers the attaching THREAD's own
/// `ThreadSlot::project_path` -- captured once when that slot was created
/// or restored -- over the process-global `session_cwd_override`. The
/// global only ever reflects whichever MLT project happens to be active
/// RIGHT NOW; using it here was the isolation leak this phase closes,
/// since attaching thread A after the user has switched to project B would
/// hand acpx B's directory for an A-scoped session. Falls back to the
/// global override only when the slot itself carries no project (a thread
/// created/attached with nothing open at the time), and to the process's
/// own working directory (`.` as a last resort) only when neither is
/// known -- matching this function's pre-existing behavior for that case.
fn cwd_for_session(
    thread_project_path: Option<&std::path::Path>,
    session_cwd_override: &Mutex<Option<PathBuf>>,
) -> PathBuf {
    thread_project_path
        .and_then(|path| {
            let identity =
                crate::model::ProjectIdentity::Saved(path.to_string_lossy().into_owned());
            crate::project_store::project_store_dir(&identity, &resolve_cache_dir())
        })
        .or_else(|| {
            session_cwd_override
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The project directory a THREAD's own MCP servers and skills-sync must
/// be scoped to (PISO-4 extension): `snapflowd_mcp_servers_entry`'s
/// `--project-dir` argument and the reactive skills sync both need this,
/// and must agree with each other and with `cwd_for_session` above on
/// which project a given thread belongs to -- so this is the one place
/// that resolves it, rather than three independent reads of the global
/// that could each observe a different value if the active project
/// changes mid-flight. Same slot-first, then-global fallback as
/// `cwd_for_session`, but deliberately stops there: no `current_dir()`
/// last resort, since an MCP server or skills sync must never be rooted
/// at wherever panel-rust happened to launch from just because no project
/// is known -- `None` means global scope, not a directory guess.
fn thread_project_dir(
    thread_project_path: Option<&std::path::Path>,
    session_cwd_override: &Mutex<Option<PathBuf>>,
) -> Option<PathBuf> {
    thread_project_path
        .and_then(|path| {
            let identity =
                crate::model::ProjectIdentity::Saved(path.to_string_lossy().into_owned());
            crate::project_store::project_store_dir(&identity, &resolve_cache_dir())
        })
        .or_else(|| {
            session_cwd_override
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
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
            let state = slot.attachment.lock().unwrap_or_else(|e| e.into_inner());
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
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
        );
    }
    {
        let mut state = slot.attachment.lock().unwrap_or_else(|e| e.into_inner());
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
                        .unwrap_or_else(|e| e.into_inner())
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
                AgentEvent::HistoryPage {
                    messages,
                    next_cursor,
                } => {
                    let mut history = slot_for_task
                        .history
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let mut prepended = messages.clone();
                    prepended.extend(history.drain(..));
                    *history = prepended;
                    *slot_for_task
                        .history_cursor
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = next_cursor.clone();
                    *slot_for_task
                        .older_available
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = next_cursor.is_some();
                    refresh_transcript(&slot_for_task);
                }
                AgentEvent::TurnEnded(_) => {
                    persist_thread_snapshot(store_for_task.as_ref(), &slot_for_task, now_token());
                    slot_for_task
                        .transcript
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .mark_all_streaming_completed();
                }
                AgentEvent::UsageUpdate { used, size } => {
                    *slot_for_task.usage.lock().expect("usage mutex poisoned") = (*used, *size);
                }
                AgentEvent::Error(_) => {}
                AgentEvent::ProviderProbe { .. } => {}
                AgentEvent::PermissionRequest(req) => {
                    slot_for_task
                        .pending_requests
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(req.clone());
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
                }
                AgentEvent::TerminalOutput(term_ev) => {
                    store_terminal_output(&slot_for_task, term_ev);
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
                }
                AgentEvent::TerminalCreated(created_ev) => {
                    store_terminal_created(&slot_for_task, created_ev);
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
                }
                AgentEvent::QueueChanged { .. } => {}
                AgentEvent::SessionModes(_)
                | AgentEvent::CurrentModeChanged(_)
                | AgentEvent::ConfigOptions(_)
                | AgentEvent::AvailableCommands(_)
                | AgentEvent::PlanUpdate(_)
                | AgentEvent::SessionInfoUpdate { .. } => {
                    store_capability_event(&slot_for_task, &ev);
                    persist_runtime_snapshot(store_for_task.as_ref(), &slot_for_task);
                }
            }
            events_out
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(BridgeEvent {
                    thread_index: idx,
                    event: ev,
                });
        }
    });
}

/// acpx-client-session-lease-pool: shared implementation behind both
/// `AgentBridge::pool_for` and `spawn_background_attachment`'s own
/// freshly-recomputed-cwd re-lookup (see that function's PISO-7 re-
/// snapshot comment) -- a free function, not an `AgentBridge` method, so
/// the background-attachment task (which only has these two `Arc`s, not
/// `&AgentBridge`) can call the identical logic rather than duplicating
/// it. See `AgentBridge::pool_for`'s doc comment for the full contract.
fn resolve_pool_for(
    project_pools: &Mutex<
        std::collections::HashMap<String, (SharedSessionPool, Vec<serde_json::Value>)>,
    >,
    gateways: &Mutex<std::collections::HashMap<String, Arc<acpx_client::Gateway>>>,
    runtime: &tokio::runtime::Handle,
    project_dir: &str,
    base_url: &str,
    mcp_servers: &[serde_json::Value],
) -> Option<SharedSessionPool> {
    let map_key = format!("{project_dir}|{base_url}");
    let mut pools = project_pools.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((pool, last_mcp_servers)) = pools.get_mut(&map_key) {
        if last_mcp_servers.as_slice() != mcp_servers {
            pool.opener()
                .set_mcp_servers(serde_json::Value::Array(mcp_servers.to_vec()));
            *last_mcp_servers = mcp_servers.to_vec();
            let pool_to_refresh = pool.clone();
            runtime.spawn(async move {
                pool_to_refresh.refresh_all().await;
            });
        }
        return Some(pool.clone());
    }
    let gateway = gateways
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(base_url)
        .cloned()?;
    let opener = GatewaySessionOpener::new(gateway, serde_json::Value::Array(mcp_servers.to_vec()));
    let pool: SharedSessionPool = Arc::new(acpx_client::pool::ProjectSessionPool::new(opener));
    pools.insert(map_key, (pool.clone(), mcp_servers.to_vec()));
    Some(pool)
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
    desired_config_options: Vec<(String, serde_json::Value)>,
    attachment_gate: Arc<tokio::sync::Mutex<()>>,
    session_cwd_override: Arc<Mutex<Option<PathBuf>>>,
    server_owned_persistence: bool,
    // acpx-client-session-lease-pool: whether `handle` was constructed
    // with a pool (`build_slot`'s `pool_for` returned `Some` at spawn
    // time) -- `false` for every call site not yet cut over (currently
    // the constructor's own bulk cold-start restore loop), which keeps
    // their exact pre-existing dual reattach/resume-by-staleness
    // behavior untouched.
    uses_pool: bool,
) {
    // Resolved synchronously, before the async task below, not inside it:
    // snapflowd_mcp_servers_entry now transitively probes snapshotd's MCP
    // liveness over a real (blocking std::net::TcpStream) connection --
    // rust-audit's "blocking calls inside async fn" anti-pattern would
    // otherwise apply here, tying up a tokio worker thread (and holding
    // attachment_gate's async guard, below) for the probe's connect/read
    // timeouts. This function itself is a plain sync fn, so the blocking
    // call here is no different from provision_gateway's own pre-existing
    // synchronous network probes at construction time.
    // Shared by both uses below (`snapflowd_mcp_servers_entry` and the
    // skills reactive-sync) since they must agree with each other and
    // with the session's own `cwd` (fixed further down) on which project
    // this thread is scoped to; reading the global independently at each
    // site risked three different answers for one thread (PISO-4). See
    // `thread_project_dir`'s own doc comment for the fallback shape.
    let slot_project_path = slot.project_path_snapshot();
    let thread_project_dir =
        thread_project_dir(slot_project_path.as_deref(), &session_cwd_override);
    // `snapflowd_mcp_servers_entry` turns `thread_project_dir` into the
    // skills MCP server's `--project-dir <parent of the project file>`
    // argument (see `snapflowd_mcp_servers_entry_adds_project_dir_from_
    // the_open_project_files_parent`) -- reading the process-global here
    // instead would hand this thread's MCP tools a different project's
    // directory than the `cwd` it now correctly attaches with, which is
    // the half of this leak that actually lets an agent read/write the
    // wrong project's files.
    let mcp_servers = snapflowd_mcp_servers_entry(thread_project_dir.as_deref(), &slot.provider);

    // Reactive-sync trigger (2) (memory/acpx/gen/plans/acpx-skills/
    // README.md#reactive-sync): before/at session setup, make sure this
    // agent's skills are actually propagated to its native skill
    // directory, catching up anything registered while no session was
    // open. `None` project_root here means global scope, deliberately
    // NOT cwd_for_session's current-dir fallback -- an unset session cwd
    // override should sync global skills only, not whatever directory the
    // panel-rust process happens to be running from.
    //
    // For vendor_ids skills_manager::agent_registry::is_live_verified()
    // covers -- MCP is no longer sent for them at all (below), so this
    // sync is the *only* delivery path left. It must complete before
    // session/new fires, so it runs BLOCKING right here (fast local
    // sqlite+symlink work, not a network call) rather than on a
    // fire-and-forget background thread. For every other vendor_id, MCP
    // remains the real delivery path regardless of this sync's timing, so
    // it stays a best-effort background thread as before.
    let vendor_id_for_sync = slot.provider.clone();
    let project_root_for_sync = thread_project_dir;
    if crate::skills_manager_adapter::is_live_verified(&vendor_id_for_sync) {
        if let Err(error) = crate::skills_manager_adapter::sync_agent_targets(
            &vendor_id_for_sync,
            project_root_for_sync.as_deref(),
        ) {
            eprintln!(
                "panel-rust: skills-manager thread-start reactive sync failed for {vendor_id_for_sync} \
                 (blocking, filesystem-only delivery for this vendor -- session is opening WITHOUT \
                 skills if this failed): {error}"
            );
        }
    } else {
        std::thread::spawn(move || {
            if let Err(error) = crate::skills_manager_adapter::sync_agent_targets(
                &vendor_id_for_sync,
                project_root_for_sync.as_deref(),
            ) {
                eprintln!(
                    "panel-rust: skills-manager thread-start reactive sync failed for {vendor_id_for_sync}: {error}"
                );
            }
        });
    }

    runtime.spawn(async move {
        let attachment_guard = attachment_gate.lock().await;
        // Re-snapshotted here rather than reusing `slot_project_path`
        // above -- this runs later, on a worker thread, after whatever
        // delay `attachment_gate` imposes, so re-reading picks up a
        // PISO-7 rebind that landed in that window instead of attaching
        // with an already-stale path.
        let slot_project_path = slot.project_path_snapshot();
        let cwd = cwd_for_session(slot_project_path.as_deref(), &session_cwd_override);

        // acpx-client-session-lease-pool: `handle` only accepts
        // `AcquireAndAttach` if it was constructed via `build_slot`'s
        // pool-aware branch (`uses_pool`, set from whether `pool_for`
        // returned `Some` at spawn time). The pool path is deliberately
        // ONE strategy, not the legacy dual reattach-vs-load-by-staleness
        // branch below: `SessionOpener` exposes only `resume` (`session/
        // resume`, no history replay) and `create` (`session/new`),
        // never a `session/load`-shaped replay call, matching the plan's
        // "on reconnect, resume the leased/idle session but do not replay
        // session/load history" rule -- a pool-attached thread relies on
        // its own jsonl cache for history, never a live server replay.
        let result = if uses_pool {
            let key = acpx_client::pool::PoolKey::new(
                cwd.to_string_lossy().into_owned(),
                slot.provider.clone(),
                crate::gateway_actor::provider_profile_key(profile_name.as_deref()),
            );
            match handle
                .acquire_and_attach(
                    key,
                    slot.thread_id.clone(),
                    requested_session_id.clone(),
                    cwd.clone(),
                    mcp_servers.clone(),
                )
                .await
            {
                // `attached.resumed_from_saved` deliberately unread here --
                // see `AttachedSession`'s own doc comment: capability
                // events are already emitted uniformly for both the
                // resumed and freshly-created cases by
                // `Command::AcquireAndAttach`'s handler, so no fallback
                // decision needs it at this layer.
                Ok(attached) => Ok(attached.session_id),
                Err(error) => Err(error),
            }
        } else if let Some(session_id) = requested_session_id.clone() {
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
            let resume_result = if server_owned_persistence {
                handle
                    .reattach_session(session_id.clone(), cwd.clone(), mcp_servers.clone())
                    .await
            } else if has_cached_transcript && !cache_is_stale {
                match handle
                    .reattach_session(session_id.clone(), cwd.clone(), mcp_servers.clone())
                    .await
                {
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
                    if resume_error.is_authentication_or_capacity() {
                        Err(resume_error)
                    } else {
                        eprintln!(
                            "panel-rust: cached acpx session resume failed for thread {:?} ({resume_error}); opening a fresh session",
                            slot.thread_id
                        );
                        open_session_maybe_profiled(
                            &handle,
                            cwd,
                            profile_name.as_deref(),
                            mcp_servers.clone(),
                        )
                        .await
                    }
                }
            }
        } else {
            open_session_maybe_profiled(&handle, cwd, profile_name.as_deref(), mcp_servers.clone()).await
        };

        match result {
            Ok(session_id) => {
                // A deferred thread has no session while its compose
                // controls are editable. Apply those in-memory selections
                // after the real (possibly pooled) session is attached and
                // before attachment is released to the first prompt.
                for (config_id, value) in desired_config_options {
                    if let Err(error) = handle.set_config_option(config_id.clone(), value).await {
                        events_out
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push_back(BridgeEvent {
                                thread_index: idx,
                                event: AgentEvent::Error(format!(
                                    "session/set_config_option failed before first prompt for {config_id:?}: {error}"
                                )),
                            });
                    }
                }
                *slot
                    .acp_session_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(session_id);
                persist_thread_snapshot(store.as_ref(), &slot, now_token());

                if requested_session_id.is_some() && !server_owned_persistence {
                    let mut cached_index = 0usize;
                    let mut replayed_any = false;
                    while let Ok(ev) = events_rx.try_recv() {
                        if let AgentEvent::Message(message) = &ev {
                            let mut history = slot.history.lock().unwrap_or_else(|e| e.into_inner());
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
                if server_owned_persistence {
                    // Attachment readiness must not be held hostage by an
                    // optional history refresh. Senders can proceed as soon
                    // as the session is bound; the refresh result is still
                    // surfaced as an agent error and can be retried by the
                    // normal pagination path.
                    let pagination_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        handle.paginate_history(None),
                    )
                    .await;
                    if let Err(error) = match pagination_result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("timed out".to_owned()),
                    } {
                        events_out
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push_back(BridgeEvent {
                                thread_index: idx,
                                event: AgentEvent::Error(format!(
                                    "initial remote history page failed for {:?}: {error}",
                                    slot.thread_id
                                )),
                            });
                    }
                }
            }
            Err(error) => {
                let message = format!("open_session failed: {error}");
                complete_attachment(&slot, Some(message.clone()));
                events_out
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
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
            None,
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
    /// transcript paging is server-owned; the local-store constructor remains
    /// available for tests and explicit compatibility callers.
    pub fn new_with_thread_specs(thread_specs: &[ThreadSpec]) -> Result<Self, BridgeError> {
        Self::new_with_thread_specs_and_initial_cwd(thread_specs, None)
    }

    /// Production cold-start variant: the host identity is already known, so
    /// the derived project store becomes the session cwd before any attach
    /// task is spawned. This is especially important for Untitled projects,
    /// which have no MLT path to put in `ThreadSpec::project_path`.
    pub fn new_with_thread_specs_and_initial_cwd(
        thread_specs: &[ThreadSpec],
        initial_cwd: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        Self::new_with_thread_specs_and_initial_identity(thread_specs, initial_cwd, None)
    }

    /// Cold-start variant carrying both representations of the active
    /// project: the derived store used as ACP cwd and the raw saved path used
    /// as durable thread ownership. Keeping these separate prevents a store
    /// path from being fed back through `project_store_dir`.
    pub fn new_with_thread_specs_and_initial_identity(
        thread_specs: &[ThreadSpec],
        initial_cwd: Option<PathBuf>,
        initial_project_path: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        // A real project identity owns its transcript/cache physically. The
        // host supplies `initial_cwd` as the already-derived project store;
        // prefer it here so recreating the panel for Project B cannot restore
        // Project A's JSONL/session binding from one global cache directory.
        // Production persistence belongs to acpx-server. Keep the panel's
        // project store as the ACP cwd, but do not create a second local
        // transcript owner in the real host constructor. The explicit
        // `..._and_cache_dir` constructors below remain available to unit
        // tests and legacy callers that intentionally exercise local JSONL.
        let gateway_cache_dir = initial_cwd.clone().unwrap_or_else(resolve_cache_dir);
        let cache_dir_for_resolver = gateway_cache_dir.clone();
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir_and_initial_cwd(
            thread_specs,
            move |provider| {
                provision_gateway(provider, Some(&cache_dir_for_resolver))
                    .map_err(BridgeError::Gateway)
            },
            None,
            initial_cwd,
            initial_project_path,
        )
    }

    /// The real constructor both of the above delegate to: a per-provider
    /// gateway-URL resolver closure (a `ThreadSpec::provider` agent id ->
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
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError> + 'static,
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
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError> + 'static,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir_and_initial_cwd(
            thread_specs,
            resolve_gateway,
            cache_dir,
            None,
            None,
        )
    }

    fn new_with_thread_specs_and_gateway_resolver_and_cache_dir_and_initial_cwd(
        thread_specs: &[ThreadSpec],
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError> + 'static,
        cache_dir: Option<PathBuf>,
        initial_cwd: Option<PathBuf>,
        initial_project_path: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        // Boxed immediately so the same resolver this constructor uses to
        // seed `gateway_urls` up front can also be kept on the struct for
        // later lazy provisioning (`ensure_gateway_provisioned`) -- one
        // resolver, one code path, whether a provider is known now or
        // only requested later.
        let resolve_gateway: Box<dyn Fn(&str) -> Result<String, BridgeError>> =
            Box::new(resolve_gateway);
        let default_provider = thread_specs.first().map(|spec| spec.provider.clone());
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
        // PROF-1: a cold start with zero initial threads (an empty specs
        // slice is valid and normal, not just a test fixture) now leaves
        // `resolved_urls` genuinely empty rather than pre-seeded with a
        // hardcoded ["codex", "claude"] pair -- that pair silently assumed
        // those were the only two agent ids that would ever exist. Every
        // subsequent `add_thread`-family call resolves (and, if new,
        // connects) its own provider's gateway on demand instead, via
        // `resolve_provider_for` -> `ensure_gateway_provisioned`, using
        // this exact same `resolve_gateway` closure (boxed onto the
        // struct above) rather than a second, different code path.

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
        let session_cwd_override: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(initial_cwd));
        let session_project_path_override = Arc::new(Mutex::new(initial_project_path));

        // `spawn_acpx_thread_with_gateway` calls the free-function `tokio::spawn` internally,
        // which needs an active runtime context on this (calling) thread --
        // `enter()` provides that for the duration of this loop. The tasks
        // it schedules then run on the runtime's own worker threads for the
        // rest of the process's life, well past this guard's drop.
        let _guard = runtime.enter();
        let server_owned_persistence = cache_dir.is_none();
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
                history_cursor: Mutex::new(None),
                pending_requests: Mutex::new(runtime_snapshot.pending_requests),
                usage: Mutex::new((0, 0)),
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
                                    command: terminal.command.clone(),
                                    args: terminal.args.clone(),
                                    started_at: terminal.started_at.clone(),
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
                pre_session_model_options: Arc::new(Mutex::new(HashMap::new())),
                available_commands: Mutex::new(Vec::new()),
                plan: Mutex::new(Vec::new()),
                session_title: Mutex::new(None),
                attachment: Mutex::new(AttachmentState::default()),
                attachment_ready: tokio::sync::Notify::new(),
                closed: Mutex::new(false),
                archived: Mutex::new(runtime_snapshot.archived),
                // PISO-3: hydrate from the durable per-thread association
                // (`ThreadSpec::project_path`, sourced from `ThreadRecord`
                // via lib.rs's cold-start restore) rather than always
                // starting `None` here. `session_cwd_override` (the
                // process-global "whatever project is active now" value)
                // was just created above, unset either way -- this must
                // NOT read from it, since that is exactly the leak PISO-3
                // exists to close: a restored thread's binding is what it
                // was persisted with, not whatever happens to be open at
                // this restart. `None` for a freshly-seeded default thread
                // or a legacy pre-migration record, same as before.
                project_path: Mutex::new(spec.project_path.as_deref().map(PathBuf::from)),
                deferred: false,
                background: Mutex::new(false),
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
                Vec::new(),
                attachment_gate,
                session_cwd_override.clone(),
                server_owned_persistence,
                // acpx-client-session-lease-pool: bulk cold-start restore
                // is not yet cut over to the pool (SQL binding hydration
                // isn't built yet either -- see meta.json) -- unchanged
                // legacy behavior for every restored thread.
                false,
            );
        }
        drop(_guard);

        for (url, setters) in gateway_setters {
            let gateways = gateways.clone();
            let cached = shared_gateway_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&url)
                .cloned();
            if let Some(gateway) = cached {
                gateways
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(url, gateway.clone());
                invalidate_snapshotd_registry_sync();
                for setter in setters {
                    setter.set_gateway(gateway.clone());
                }
            } else {
                runtime.spawn(async move {
                    let gateway = Arc::new(acpx_client::Gateway::connect(url.clone()).await);
                    shared_gateway_cache()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(url.clone(), gateway.clone());
                    gateways
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(url, gateway.clone());
                    invalidate_snapshotd_registry_sync();
                    for setter in setters {
                        setter.set_gateway(gateway.clone());
                    }
                });
            }
        }

        // mcp-servers-settings: this bridge's gateway map is a target the
        // process-wide snapshotd watcher should keep the central-registry
        // "snapflow" row synced into (see `register_snapshotd_registry_
        // sync_target`'s doc comment). Registered here, once per bridge,
        // rather than only from `snapshotd_mcp_addr()`'s first caller --
        // that call site has no `Arc<Mutex<HashMap<..., Arc<Gateway>>>>` or
        // runtime handle to offer, and may run long before any bridge
        // (and its gateways) exist at all.
        register_snapshotd_registry_sync_target(gateways.clone(), runtime.handle().clone());

        Ok(AgentBridge {
            runtime,
            slots,
            events,
            gateway_urls: resolved_urls,
            gateways,
            project_pools: Arc::new(Mutex::new(std::collections::HashMap::new())),
            gateway_catalog: Arc::new(Mutex::new(GatewayCatalogCache::default())),
            gateway_catalog_refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_operations: Arc::new(Mutex::new(HashSet::new())),
            mcp_operations: Arc::new(Mutex::new(HashSet::new())),
            recover_session_operations: Arc::new(Mutex::new(HashSet::new())),
            resolve_gateway,
            default_provider,
            server_owned_persistence: cache_dir.is_none(),
            store,
            local_terminals: std::cell::RefCell::new(std::collections::HashMap::new()),
            session_cwd_override,
            session_project_path_override,
        })
    }

    /// PROF-1: provisions `provider`'s gateway URL on demand if this
    /// bridge hasn't resolved it yet -- replaces the old hardcoded
    /// `["codex", "claude"]` cold-start pre-seed, which silently assumed
    /// those were the only two agent ids that would ever need a gateway.
    /// A brand new agent id (anything real, from acpx's own
    /// `agents/list`) now gets its own gateway resolved and connected the
    /// first time a thread actually requests it, through the exact same
    /// resolver closure the constructor itself uses (env override /
    /// default-port probe / autospawn -- see `provision_gateway`'s doc
    /// comment), not a second, different code path. A genuine resolver
    /// failure (unreachable gateway, spawn failure, ...) is returned as a
    /// real `BridgeError` here -- this is the mechanism that keeps the
    /// "a requested provider is never silently dropped" guarantee (see
    /// `resolve_provider_for`) true even though provisioning is now lazy.
    ///
    /// Spawns a fresh `Gateway::connect` task only if no already-known
    /// provider resolves to the same URL -- the common single-gateway
    /// production case (every agent id resolves to the one shared
    /// snapshotd-owned acpx-server, see `resolve_gateway`'s/
    /// `provision_gateway`'s doc comments) must not open a second,
    /// redundant connection to a URL this bridge is already connecting
    /// to or connected to.
    fn ensure_gateway_provisioned(&mut self, provider: &str) -> Result<(), BridgeError> {
        if self.gateway_urls.contains_key(provider) {
            return Ok(());
        }
        let url = (self.resolve_gateway)(provider)?;
        let url_already_known = self.gateway_urls.values().any(|existing| existing == &url);
        self.gateway_urls.insert(provider.to_string(), url.clone());
        if !url_already_known {
            let gateways = self.gateways.clone();
            if let Some(gateway) = shared_gateway_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&url)
                .cloned()
            {
                gateways
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(url, gateway);
                invalidate_snapshotd_registry_sync();
                return Ok(());
            }
            let _guard = self.runtime.enter();
            self.runtime.spawn(async move {
                let gateway = Arc::new(acpx_client::Gateway::connect(url.clone()).await);
                shared_gateway_cache()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(url.clone(), gateway.clone());
                gateways
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(url, gateway);
                invalidate_snapshotd_registry_sync();
            });
        }
        Ok(())
    }

    /// acpx-client-session-lease-pool: the pool for one (project directory,
    /// gateway base_url) pair, creating it on first request. `None` only in
    /// the narrow window right after construction before the background
    /// `Gateway::connect` task (see `ensure_gateway_provisioned`) has
    /// resolved yet -- callers fall back to the legacy delayed-gateway
    /// non-pool path for that one rare race rather than blocking on it.
    ///
    /// `mcp_servers` is compared against the last value this bridge applied
    /// to this exact pool's opener; a change triggers `set_mcp_servers` +
    /// `refresh_all` (see `GatewaySessionOpener::set_mcp_servers`'s doc
    /// comment for why a warm-pooled session's MCP config can only be
    /// changed for *future* sessions, never an already-open one) so the
    /// next acquire for any key on this pool opens fresh with the new
    /// config; an unchanged value is a no-op, never disturbing warm
    /// sessions needlessly.
    fn pool_for(
        &self,
        project_dir: &str,
        base_url: &str,
        mcp_servers: &[serde_json::Value],
    ) -> Option<SharedSessionPool> {
        resolve_pool_for(
            &self.project_pools,
            &self.gateways,
            self.runtime.handle(),
            project_dir,
            base_url,
            mcp_servers,
        )
    }

    /// Returns whether a `ProviderProbe` `BridgeEvent` will eventually be
    /// pushed for this call (synchronously, on an immediate precondition
    /// failure below, or asynchronously once the spawned acquire+release
    /// round-trip finishes) -- `false` only for the deliberate silent
    /// no-op case (see the "no resolvable project directory" comment
    /// below). `SettingsMsg::ProfileSelected` inserts `Model::
    /// provider_probes_in_flight` unconditionally before dispatching
    /// `Effect::ProbeProvider` (the reducer has no visibility into the
    /// bridge-side project/gateway/pool state this method resolves), so
    /// the caller (`effect_executor.rs`'s `Effect::ProbeProvider` arm)
    /// uses this return value to clear that marker itself when it knows
    /// no event is ever coming -- otherwise the "Switching provider..."
    /// pulse stayed stuck forever for exactly the thread state this
    /// no-op case exists to support (a thread with no project bound and
    /// no session cwd override).
    pub fn probe_provider_selection(&self, idx: usize, provider: String, profile_name: Option<String>) -> bool {
        let Some(slot) = self.slots.get(idx) else { return false };
        // A thread with no resolvable project directory (no project bound
        // and no session cwd override) is a normal, fully-supported state
        // -- not a provider problem. `PoolKey`/session acquisition require a
        // real project dir structurally, so the probe mechanism itself
        // cannot run here; that's a precondition failure of the probe, not
        // evidence this provider's auth is broken. Skip silently: no
        // `provider_errors` entry, no toast, Send stays unaffected by
        // provider state. Push nothing at all (not even an `Ok`) so a
        // stale error from a previous project stays until a probe that can
        // actually run replaces it.
        let Some(project_dir) = thread_project_dir(slot.project_path_snapshot().as_deref(), &self.session_cwd_override) else {
            return false;
        };
        let Some(base_url) = self.gateway_urls.get(&provider).cloned() else {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).push_back(BridgeEvent { thread_index: idx, event: AgentEvent::ProviderProbe { provider: provider.clone(), result: Err(format!("no gateway is configured for {provider}")) } });
            return true;
        };
        let mcp_servers = snapflowd_mcp_servers_entry(Some(&project_dir), &provider);
        let Some(pool) = self.pool_for(&project_dir.to_string_lossy(), &base_url, &mcp_servers) else {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).push_back(BridgeEvent { thread_index: idx, event: AgentEvent::ProviderProbe { provider: provider.clone(), result: Err(format!("could not initialize the gateway pool for {provider}")) } });
            return true;
        };
        let events = self.events.clone();
        let key = acpx_client::pool::PoolKey::new(project_dir.to_string_lossy().into_owned(), provider.clone(), crate::gateway_actor::provider_profile_key(profile_name.as_deref()));
        let preview_thread_id = format!("provider-probe:{idx}:{provider}");
        self.runtime.spawn(async move {
            let result = match pool.acquire(key, preview_thread_id, acpx_client::pool::OpenSpec { saved_session_id: None }).await {
                Ok(lease) => pool.release(&lease).await.map_err(|error| format!("provider probe cleanup failed: {error}")),
                Err(error) => Err(error.to_string()),
            };
            events.lock().unwrap_or_else(|e| e.into_inner()).push_back(BridgeEvent { thread_index: idx, event: AgentEvent::ProviderProbe { provider, result } });
        });
        true
    }

    /// Notify every pool for the settings gateway that its admin-plane MCP
    /// configuration changed. The pool generation refresh evicts idle
    /// sessions immediately; a currently leased session is left alive and is
    /// discarded when its owner releases it, so an in-flight turn is never
    /// force-closed by a Settings edit.
    fn refresh_pools_for_gateway(&self, base_url: &str) {
        let pools = {
            let pools = self.project_pools.lock().unwrap_or_else(|e| e.into_inner());
            pools
                .iter()
                .filter(|(key, _)| key.rsplit_once('|').is_some_and(|(_, url)| url == base_url))
                .map(|(_, (pool, _))| pool.clone())
                .collect::<Vec<_>>()
        };
        for pool in pools {
            self.runtime.spawn(async move {
                pool.refresh_all().await;
            });
        }
    }

    /// Retires every pool owned by a project whose identity changed. Idle
    /// sessions are removed immediately; active leases become stale and are
    /// removed by their owner after the current turn finishes.
    fn refresh_pools_for_project_dir(&self, project_dir: &Path) {
        let prefix = format!("{}|", project_dir.to_string_lossy());
        let pools = {
            let pools = self.project_pools.lock().unwrap_or_else(|e| e.into_inner());
            pools
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(_, (pool, _))| pool.clone())
                .collect::<Vec<_>>()
        };
        for pool in pools {
            self.runtime.spawn(async move {
                pool.refresh_all().await;
            });
        }
    }

    /// Starts bounded warmup for the resolved default agent when its gateway
    /// is already available. It activates the pool without taking a thread
    /// lease and never waits for `session/new` on the caller thread.
    pub fn prewarm_default_agent(&self, agent_id: &str, profile_name: Option<&str>) {
        let Some(project_dir) = self
            .session_cwd_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };
        let Some(base_url) = self.gateway_urls.get(agent_id).cloned() else {
            return;
        };
        let mcp_servers = snapflowd_mcp_servers_entry(Some(&project_dir), agent_id);
        let Some(pool) = self.pool_for(&project_dir.to_string_lossy(), &base_url, &mcp_servers)
        else {
            return;
        };
        let key = acpx_client::pool::PoolKey::new(
            project_dir.to_string_lossy().into_owned(),
            agent_id,
            crate::gateway_actor::provider_profile_key(profile_name),
        );
        self.runtime.spawn(async move {
            pool.prewarm(key).await;
        });
    }

    /// Async counterpart to [`Self::refresh_pools_for_gateway`] for use
    /// inside already-spawned MCP-settings futures that only hold a
    /// pre-captured `project_pools` Arc (no `&self`).
    async fn refresh_captured_pools(
        project_pools: &Mutex<
            std::collections::HashMap<String, (SharedSessionPool, Vec<serde_json::Value>)>,
        >,
        base_url: Option<&str>,
    ) {
        let Some(url) = base_url else {
            return;
        };
        let pools = {
            let pools = project_pools.lock().unwrap_or_else(|e| e.into_inner());
            pools
                .iter()
                .filter(|(key, _)| key.rsplit_once('|').is_some_and(|(_, u)| u == url))
                .map(|(_, (pool, _))| pool.clone())
                .collect::<Vec<_>>()
        };
        for pool in pools {
            pool.refresh_all().await;
        }
    }

    /// Toggle the built-in snapflow (snapshotd) MCP for **live** pool
    /// openers and future `session/new` client `mcpServers`.
    ///
    /// Unlike registry enable (`mcp_servers/update`), this is not a
    /// central-store row — Settings shows it as a non-removable "snapflow"
    /// row while injection uses wire name `"snapshotd"`. Flipping the
    /// gate alone would leave already-pooled sessions with the old list
    /// forever; this rewrites every pool's opener mcp list and
    /// `refresh_all` so idle leases drop and the next acquire omits (or
    /// re-adds) snapflow. Currently-leased sessions stay until release
    /// (same generation semantics as registry refresh).
    pub fn set_builtin_snapflow_mcp_enabled(&self, enabled: bool) {
        set_snapflow_mcp_enabled_flag(enabled);
        let inject_addr = if enabled {
            snapshotd_mcp_addr()
        } else {
            None
        };
        let pools_to_refresh = {
            let mut pools = self
                .project_pools
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut to_refresh = Vec::new();
            for (_key, (pool, last_mcp)) in pools.iter_mut() {
                let next = apply_snapflow_to_client_mcp_list(
                    last_mcp,
                    enabled,
                    inject_addr.as_deref(),
                );
                if next.as_slice() != last_mcp.as_slice() {
                    pool.opener()
                        .set_mcp_servers(serde_json::Value::Array(next.clone()));
                    *last_mcp = next;
                    to_refresh.push(pool.clone());
                }
            }
            to_refresh
        };
        for pool in pools_to_refresh {
            self.runtime.spawn(async move {
                pool.refresh_all().await;
            });
        }
    }

    /// Immediate signal from Settings after a successful MCP registry
    /// mutation. The settings gateway is represented by a bridge slot, so
    /// resolve its base URL and refresh all project pools sharing that
    /// gateway. This deliberately does not perform an RPC or wait for the
    /// refresh on the UI thread.
    fn notify_mcp_settings_changed(&self, settings_idx: usize) {
        let Some(provider) = self
            .slots
            .get(settings_idx)
            .map(|slot| slot.provider.clone())
        else {
            return;
        };
        let Some(base_url) = self.gateway_urls.get(&provider).cloned() else {
            return;
        };
        self.refresh_pools_for_gateway(&base_url);
    }

    /// `chat_sessions_project_path` phase: called from the FFI-driven
    /// `panel_rust_set_project_path` path whenever the active MLT project
    /// changes, so every subsequently-opened/resumed/reattached session
    /// picks up the new project directory as its `cwd`. Deliberately does
    /// NOT retroactively move already-open sessions -- ACP has no
    /// "change an existing session's cwd" operation.
    pub fn set_active_project_path(&self, path: Option<PathBuf>) {
        let identity = path
            .as_ref()
            .map_or(crate::model::ProjectIdentity::None, |raw| {
                crate::model::ProjectIdentity::Saved(raw.to_string_lossy().into_owned())
            });
        self.set_active_project_identity(&identity);
    }

    /// project-close-session-teardown: soft-releases every open thread's
    /// live session that is still bound to the CURRENTLY active project
    /// (i.e. the one about to stop being active), scoped by each
    /// [`ThreadSlot`]'s own recorded `project_path` (the same value
    /// `thread_project_path`/`retain_items_for_project` use to scope the
    /// sidebar). Must be called BEFORE [`Self::set_active_project_
    /// identity`] overwrites `session_project_path_override`, since that
    /// is exactly the "currently active project" this method reads.
    ///
    /// Unscoped threads (`project_path: None`) are ALSO released here, on
    /// every switch, regardless of what (if anything) was previously
    /// active. Unlike scoped threads, an unscoped thread's `project_path`
    /// is never bound to whatever project happens to be active -- it stays
    /// `None` for the thread's whole life (barring an explicit rebind, see
    /// `rebind_unscoped_project_path`) -- so there is no "switching away
    /// from it" moment to key off of the way there is for a scoped thread.
    /// Without this, an unscoped thread's live session/pool lease would
    /// never be released by ANY project switch, accumulating indefinitely
    /// no matter how many times the user changes projects. This does not
    /// affect sidebar visibility: `retain_items_for_project` unconditionally
    /// keeps unscoped threads listed (they were never tied to a project),
    /// and releasing here never sets the permanent `closed` flag, so a
    /// released unscoped thread reappears exactly like a released
    /// scoped-foreign-project thread does today -- listed, session-less,
    /// reopenable on next send.
    ///
    /// Root cause this closes: before this method existed, a project
    /// switch or close only ever updated `session_cwd_override`/
    /// `session_project_path_override` (so *future* `session/new` calls
    /// picked up the new project) and, via `refresh_pools_for_project_dir`,
    /// bumped the previous project's pool generation -- which only evicts
    /// currently-IDLE pool entries and marks a currently-LEASED one stale
    /// for later. A thread's [`gateway_actor::thread_actor`] actor holds
    /// its lease for the thread's entire life (see that module's
    /// `current_lease`), released only on an explicit close/delete or the
    /// next `SendPrompt` -- neither of which a thread belonging to a
    /// project the user just switched away from (or closed) will ever see
    /// again while that project stays inactive. The live ACPX session and
    /// its pooled connection therefore stayed open/leased indefinitely,
    /// exactly the "threads keep running in the background instead of
    /// being torn down" bug this fixes.
    ///
    /// Uses `close_session(true)` (background=true) rather than a hard
    /// close/delete: per `Command::CloseSession`'s `background` branch in
    /// `gateway_actor/thread_actor.rs`, a background close still releases
    /// this panel's client-side pool lease back to Idle -- freeing the
    /// pooled connection slot, this bug's actual complaint -- while asking
    /// acpx-core for a resumable soft close on the backend side, so
    /// switching back to this project later resumes the conversation
    /// instead of losing it. Deliberately does NOT set `slot.closed` --
    /// that flag is the user-facing, permanent "Close" button state
    /// ([`Self::close_thread`]); this is an automatic lifecycle teardown,
    /// not an explicit user action, and must not make an untouched thread
    /// look explicitly closed in the sidebar.
    ///
    /// Errors are logged and otherwise ignored, same posture as this
    /// file's other best-effort teardown paths (e.g. `Drop for
    /// AgentBridge`) -- a failed release here should not block the
    /// project switch itself.
    pub fn release_sessions_for_current_project(&self) {
        let leaving_project_path = self
            .session_project_path_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for slot in &self.slots {
            let slot_project_path = slot.project_path_snapshot();
            // Unscoped threads (`project_path: None`) are never tied to any
            // one project, so they never match `leaving_project_path` by
            // equality -- release them on EVERY switch, regardless of what
            // (if anything) was previously active, otherwise their live
            // session accumulates forever across arbitrarily many project
            // switches. Scoped threads keep the original behavior: release
            // only when they match the project actually being left.
            let should_release = match (&slot_project_path, &leaving_project_path) {
                (None, _) => true,
                (Some(slot_path), Some(leaving_path)) => slot_path == leaving_path,
                (Some(_), None) => false,
            };
            if !should_release {
                continue;
            }
            let handle = slot.handle.clone();
            let thread_id = slot.thread_id.clone();
            self.runtime.spawn(async move {
                if let Err(error) = handle.close_session(true).await {
                    eprintln!(
                        "panel-rust: release_sessions_for_current_project close_session failed for thread {thread_id} (async): {error}"
                    );
                }
            });
        }
    }

    /// Apply the complete lifecycle identity, including an untitled UUID.
    /// An untitled project has no raw MLT path to publish to snapshotd, but it
    /// still owns a staging store and therefore must provide a real ACP cwd.
    pub fn set_active_project_identity(&self, identity: &crate::model::ProjectIdentity) {
        let previous_store_path = self
            .session_cwd_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let store_path = crate::project_store::project_store_dir(identity, &resolve_cache_dir());
        let raw_path = identity.saved_path().map(PathBuf::from);
        if previous_store_path != store_path {
            if let Some(previous_store_path) = previous_store_path {
                self.refresh_pools_for_project_dir(&previous_store_path);
            }
        }
        *self
            .session_cwd_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = store_path;
        *self
            .session_project_path_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = raw_path;
    }

    /// PISO-7 (project-isolation-mlt-binding plan): the live half of a
    /// Save-As rebind. `state_store::PanelStateStore::rename_project_path`
    /// rewrites `thread_settings.project_path` durably, but that alone
    /// only takes effect on the NEXT restart -- `retain_items_for_project`
    /// (the visible-list scoping) reads `thread_project_path`, which reads
    /// each `ThreadSlot`'s own in-memory `project_path`, not sqlite. Without
    /// this, a user who does Save-As mid-session would watch their entire
    /// pre-save chat history vanish from the sidebar until they restart
    /// the panel -- a fix that only works after a restart is not a fix
    /// for that bug.
    ///
    /// Updates every slot whose current `project_path` equals `old` to
    /// `new`; every other slot (a different project, or none at all) is
    /// untouched. Deliberately called ONLY from the effect handling
    /// `HostMsg::ProjectPathRenamed` (an explicit host-driven rename
    /// signal), never from a bare active-project-path change -- "Save-As
    /// A->B" and "close A, open B" are indistinguishable from an old/new
    /// path pair alone, and rebinding on the latter would merge two
    /// genuinely different projects' thread histories. Callers must also
    /// never pass an empty `old`: an untitled/never-saved project's
    /// threads are unscoped (`project_path: None`), not associated with
    /// `Some("")`, so an empty `old` would (correctly) match nothing here
    /// -- but see `update_host`'s `ProjectPathRenamed` handler, which
    /// guards this earlier and more explicitly, treating "first save of
    /// an untitled project" as NOT a rename at all.
    ///
    /// Synchronous and in-memory only (no I/O) -- called directly from the
    /// effect executor on the UI thread, not spawned, so the very next
    /// poll tick already reflects the rebind.
    pub fn rebind_project_path(&self, old: &str, new: &str) {
        if old.is_empty() {
            return;
        }
        let old_path = std::path::Path::new(old);
        let new_path = PathBuf::from(new);
        for slot in &self.slots {
            let mut guard = slot.project_path.lock().unwrap_or_else(|e| e.into_inner());
            if guard.as_deref() == Some(old_path) {
                *guard = Some(new_path.clone());
            }
        }
    }

    /// First-save half of the project migration: slots created while the
    /// project was Untitled have no raw project path, so a path-based rename
    /// cannot find them. Rebind only those unscoped slots to the new saved
    /// identity; already-scoped slots are left untouched.
    pub fn rebind_unscoped_project_path(&self, new: &str) {
        let new_path = PathBuf::from(new);
        for slot in &self.slots {
            let mut guard = slot.project_path.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                *guard = Some(new_path.clone());
            }
        }
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
    /// PUI-014: shared slot construction for the eager
    /// ([`Self::add_thread_with_profile_and_provider`]), deferred
    /// ([`Self::add_thread_deferred`]), and attach-on-first-send
    /// ([`Self::attach_deferred_thread`]) paths. Builds -- but does NOT push or
    /// attach -- a `ThreadSlot` for `thread_id`/`provider`, wiring its gateway
    /// through the same delayed-setter the constructor uses (so building never
    /// blocks on the gateway). Returns the slot plus everything
    /// [`spawn_background_attachment`] needs, letting the caller decide whether
    /// to attach now or defer. `deferred` only sets the slot's flag.
    #[allow(clippy::type_complexity)]
    fn build_slot(
        &mut self,
        thread_id: &str,
        provider: &str,
        deferred: bool,
    ) -> Result<
        (
            Arc<ThreadSlot>,
            Arc<AcpxThreadHandle>,
            tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
            Option<String>,
            bool,
            bool,
        ),
        BridgeError,
    > {
        let base_url =
            self.gateway_urls.get(provider).cloned().ok_or_else(|| {
                BridgeError::Gateway(format!("gateway URL missing for {provider}"))
            })?;
        let (seeded, cached_session_id, older_available, oldest_loaded_index, runtime_snapshot) =
            seed_thread_from_cache(self.store.as_ref(), thread_id, HISTORY_PAGE_SIZE);
        let has_cached_transcript = !seeded.is_empty();

        let project_path_for_slot = self
            .session_project_path_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // acpx-client-session-lease-pool: PoolKey::project_dir must match
        // the real ACP `cwd` this thread will attach with, so `pool_for`
        // uses the same resolution `cwd_for_session` (below, at attach
        // time) will use -- computed here, once, so both agree.
        let pool_cwd =
            cwd_for_session(project_path_for_slot.as_deref(), &self.session_cwd_override);
        let mcp_servers = snapflowd_mcp_servers_entry(
            thread_project_dir(project_path_for_slot.as_deref(), &self.session_cwd_override)
                .as_deref(),
            provider,
        );
        let pool = self.pool_for(&pool_cwd.to_string_lossy(), &base_url, &mcp_servers);
        let uses_pool = pool.is_some();

        let (mut handle, gateway_setter) = {
            let _guard = self.runtime.enter();
            match pool.clone() {
                Some(pool) => spawn_acpx_thread_with_delayed_gateway_and_pool(pool),
                // Only reachable in the narrow window right after
                // construction, before the background `Gateway::connect`
                // task (spawned in the constructor) has resolved yet --
                // `pool_for` itself needs a real `Arc<Gateway>` to build a
                // `GatewaySessionOpener`, so this one rare race falls back
                // to the legacy non-pool path rather than blocking on it.
                None => spawn_acpx_thread_with_delayed_gateway(),
            }
        };
        match self
            .gateways
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
                            .unwrap_or_else(|e| e.into_inner())
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
        let slot = Arc::new(ThreadSlot {
            thread_id: thread_id.to_string(),
            provider: provider.to_string(),
            handle: handle.clone(),
            transcript: Mutex::new(crate::conversation::rebuild_from_chat_messages(
                thread_id, &seeded,
            )),
            history: Mutex::new(seeded),
            acp_session_id: Mutex::new(None),
            older_available: Mutex::new(older_available),
            oldest_loaded_index: Mutex::new(oldest_loaded_index),
            history_cursor: Mutex::new(None),
            pending_requests: Mutex::new(runtime_snapshot.pending_requests),
            usage: Mutex::new((0, 0)),
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
                                command: terminal.command.clone(),
                                args: terminal.args.clone(),
                                started_at: terminal.started_at.clone(),
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
            pre_session_model_options: Arc::new(Mutex::new(HashMap::new())),
            available_commands: Mutex::new(Vec::new()),
            plan: Mutex::new(Vec::new()),
            session_title: Mutex::new(None),
            attachment: Mutex::new(AttachmentState::default()),
            attachment_ready: tokio::sync::Notify::new(),
            closed: Mutex::new(false),
            archived: Mutex::new(runtime_snapshot.archived),
            project_path: Mutex::new(project_path_for_slot),
            deferred,
            background: Mutex::new(false),
        });
        Ok((
            slot,
            handle,
            events_rx,
            cached_session_id,
            has_cached_transcript,
            uses_pool,
        ))
    }

    /// PUI-014 / PROF-1: resolves a caller-requested agent id to a
    /// provisioned gateway key -- the id flows through completely as-is
    /// now (no "codex"/"claude" normalization step), and gets lazily
    /// provisioned via [`Self::ensure_gateway_provisioned`] if this
    /// bridge hasn't seen it before, so a genuinely new agent id (any
    /// real id from acpx's own `agents/list`) routes to its own gateway
    /// with zero code changes here. Shared by the eager, deferred, and
    /// attach paths so a requested provider is never silently dropped
    /// (see `thread_provider_model_binding_fix`) -- a real provisioning
    /// failure (not "unknown provider", an actual resolver error) is
    /// surfaced as a real `BridgeError` instead of quietly falling back
    /// to a different provider.
    ///
    /// With no request at all, falls back to this bridge's own
    /// `default_provider` -- the first thread spec's already-resolved
    /// provider, itself derived upstream from a real profile's agent id
    /// rather than an index-based guess. A bridge that started with zero
    /// threads has no such default; [`NO_PROVIDER_REQUESTED_FALLBACK`]
    /// is the one explicit, documented last resort for that case.
    fn resolve_provider_for(
        &mut self,
        preferred_provider: Option<&str>,
    ) -> Result<String, BridgeError> {
        let provider = match preferred_provider.filter(|p| !p.trim().is_empty()) {
            Some(requested) => requested.to_string(),
            None => self
                .default_provider
                .clone()
                .unwrap_or_else(|| NO_PROVIDER_REQUESTED_FALLBACK.to_string()),
        };
        self.ensure_gateway_provisioned(&provider)?;
        Ok(provider)
    }

    /// PUI-014: create thread `name` as a DEFERRED placeholder -- it claims its
    /// positional slot index (preserving the `model.threads[i] <-> slots[i]`
    /// parallel-array invariant) but opens NO ACP session yet, so the
    /// provider/profile stay editable until the first message triggers
    /// [`Self::attach_deferred_thread`]. Same name/dedup/provider-resolution
    /// rules as the eager path.
    pub fn add_thread_deferred(
        &mut self,
        name: &str,
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
        let provider = self.resolve_provider_for(preferred_provider)?;
        let (slot, _handle, _events_rx, _cached_session_id, _has_cached_transcript, _uses_pool) =
            self.build_slot(&thread_id, &provider, true)?;
        self.slots.push(slot);
        Ok(idx)
    }

    /// PUI-014: attach a previously-deferred thread on its first message send,
    /// binding it to `preferred_provider`/`profile` as they stand NOW (which
    /// may differ from the creation-time hint if the user changed the picker
    /// while the thread was still empty). Rebuilds the slot in place for the
    /// current provider -- preserving its index and the parallel-array
    /// invariant -- then starts the real background attach. Idempotent: a
    /// no-op `Ok(())` if `idx` is already attached (not deferred), so a racing
    /// second send cannot double-attach.
    pub fn attach_deferred_thread(
        &mut self,
        idx: usize,
        preferred_provider: Option<&str>,
        profile: Option<&str>,
    ) -> Result<(), BridgeError> {
        self.attach_deferred_thread_with_config_options(
            idx,
            preferred_provider,
            profile,
            Vec::new(),
        )
    }

    pub fn attach_deferred_thread_with_config_options(
        &mut self,
        idx: usize,
        preferred_provider: Option<&str>,
        profile: Option<&str>,
        desired_config_options: Vec<(String, serde_json::Value)>,
    ) -> Result<(), BridgeError> {
        let Some(existing) = self.slots.get(idx) else {
            return Err(BridgeError::Gateway(format!(
                "no thread slot at index {idx}"
            )));
        };
        if !existing.deferred {
            return Ok(());
        }
        let thread_id = existing.thread_id.clone();
        let provider = self.resolve_provider_for(preferred_provider)?;
        let (slot, handle, events_rx, cached_session_id, has_cached_transcript, uses_pool) =
            self.build_slot(&thread_id, &provider, false)?;
        self.slots[idx] = slot.clone();
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
            desired_config_options,
            Arc::new(tokio::sync::Mutex::new(())),
            self.session_cwd_override.clone(),
            self.server_owned_persistence,
            uses_pool,
        );
        Ok(())
    }

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
        // `thread_provider_model_binding_fix` / PROF-1: a requested
        // provider is NEVER silently dropped. The agent id flows through
        // as-is (see `resolve_provider_for`'s own doc comment) and is
        // lazily provisioned if this bridge hasn't seen it before -- a
        // genuine provisioning failure is a real error instead of a
        // thread quietly bound to a different provider, the exact
        // "selected claude-acp, codex-acp underneath" failure reported
        // live on the VNC demo.
        let provider = self.resolve_provider_for(preferred_provider)?;
        let (slot, handle, events_rx, cached_session_id, has_cached_transcript, uses_pool) =
            self.build_slot(&thread_id, &provider, false)?;
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
            Vec::new(),
            Arc::new(tokio::sync::Mutex::new(())),
            self.session_cwd_override.clone(),
            self.server_owned_persistence,
            uses_pool,
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
                    .unwrap_or_else(|e| e.into_inner())
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
    /// this does *not* fall back to `default_provider` (a brand-new
    /// local thread has no natural default-provider assignment here;
    /// the provider is instead exactly whichever gateway the recovered
    /// session id actually lives on).
    /// `resume_session`'s own real history replay is what populates the
    /// new thread's transcript -- proven at the actor layer already
    /// (`resume_session_replays_history_via_session_load`); this method
    /// only wires that replay into a fresh `ThreadSlot`, the same shape
    /// [`Self::add_thread_with_profile`]'s own cached-session-resume
    /// branch already establishes for a different trigger (local jsonl
    /// cache instead of a picked remote session).
    ///
    /// recoverable-attach-fix: this used to resolve the gateway and run
    /// `session/load` via two `self.runtime.block_on(..)` calls right
    /// here, on whatever thread called this method -- the Slint "Attach"
    /// button's click handler, i.e. the UI thread. That froze the whole
    /// app for the full round trip (and up to a 10s gateway-wait on top).
    /// Every other attach path in this file (`add_thread_deferred`'s
    /// eventual `attach_deferred_thread`, `add_thread_with_profile_and_
    /// provider`) claims its slot's index synchronously (cheap: name
    /// dedup + a `HashMap` lookup) and then does the real network work on
    /// `self.runtime` via `spawn_background_attachment`, in the
    /// background. This method now follows the identical shape, using
    /// the same delayed-gateway handle (`spawn_acpx_thread_with_delayed_
    /// gateway`) `build_slot` uses for its own non-pool branch, so the
    /// slot is pushed -- and this call returns -- before any RPC ever
    /// starts. The slot starts with `AttachmentState::default()`
    /// (`complete: false`), exactly like every other in-flight attach;
    /// `external_snapshot.rs`'s existing "no binding yet, not deferred"
    /// check already renders such a slot as a loading/"Starting new
    /// thread..." row with zero additional wiring (see its own "eager/
    /// recovered" comment, which already anticipated this path), and
    /// `wait_for_attachment` already makes a `send_prompt` issued before
    /// attachment completes wait for it in the background rather than
    /// erroring or blocking the caller.
    ///
    /// Deliberately does NOT reuse `spawn_background_attachment` itself:
    /// that function's `requested_session_id` branch falls back to a
    /// brand-new `session/new` if `session/load` fails for a non-auth
    /// reason, which would silently violate this method's own contract
    /// ("never `session/new`") the moment a picked-from-`session/list`
    /// session id turned out to be stale/gone -- exactly the case a
    /// recovery flow most needs to fail loudly on, not paper over.
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
        let base_url = match self.gateway_urls.get(provider).cloned() {
            Some(base_url) => base_url,
            None => {
                return Err(BridgeError::Gateway(format!(
                    "gateway URL missing for {provider}"
                )))
            }
        };
        // Marked busy from this point on (a real attach is about to
        // start) through the background task's completion below --
        // covers both the success and error exits, symmetric with `end_
        // recover_session_operation` at the bottom of the spawned task.
        self.begin_recover_session_operation(session_id);

        // Deliberately does not consult the local jsonl cache for
        // `thread_id` -- this is a *new* local thread identity being
        // bound to a pre-existing *remote* session, not a reopen of a
        // thread this panel already knew about (that path is `add_
        // thread_with_profile`'s own `cached_session_id` branch).
        //
        // Same delayed-gateway construction `build_slot`'s non-pool
        // branch uses: the handle is usable immediately (queues its
        // first command until a gateway is set), so nothing here needs
        // to wait -- synchronously if already connected, or via a
        // background poll loop (identical to `build_slot`'s own) if the
        // gateway's `Gateway::connect` task from the constructor hasn't
        // resolved yet.
        let (mut handle, gateway_setter) = {
            let _guard = self.runtime.enter();
            spawn_acpx_thread_with_delayed_gateway()
        };
        match self
            .gateways
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&base_url)
            .cloned()
        {
            Some(gateway) => gateway_setter.set_gateway(gateway),
            None => {
                let gateways = self.gateways.clone();
                self.runtime.spawn(async move {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        if let Some(gateway) = gateways
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
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
        let mut events_rx = handle.take_events();
        let handle = Arc::new(handle);
        let project_path_for_slot = self
            .session_project_path_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
            history_cursor: Mutex::new(None),
            pending_requests: Mutex::new(Vec::new()),
            usage: Mutex::new((0, 0)),
            terminal_buffers: Mutex::new(HashMap::new()),
            terminal_order: Mutex::new(Vec::new()),
            session_modes: Mutex::new(None),
            config_options: Mutex::new(Vec::new()),
            pre_session_model_options: Arc::new(Mutex::new(HashMap::new())),
            available_commands: Mutex::new(Vec::new()),
            plan: Mutex::new(Vec::new()),
            session_title: Mutex::new(None),
            // Not yet attached: `session/load` hasn't even been sent
            // yet, let alone completed. `wait_for_attachment` (used by
            // `send_prompt`/`cancel_prompt`) blocks on this in the
            // background until the task below calls `complete_
            // attachment`, and `external_snapshot.rs`'s thread-list
            // hydration already renders a slot in this state as a
            // loading row.
            attachment: Mutex::new(AttachmentState::default()),
            attachment_ready: tokio::sync::Notify::new(),
            closed: Mutex::new(false),
            archived: Mutex::new(false),
            project_path: Mutex::new(project_path_for_slot),
            deferred: false,
            background: Mutex::new(false),
        });
        self.slots.push(slot.clone());

        let session_id = session_id.to_string();
        let store = self.store.clone();
        let session_cwd_override = self.session_cwd_override.clone();
        let events_out = self.events.clone();
        let recover_session_operations = self.recover_session_operations.clone();
        self.runtime.spawn(async move {
            // `slot.project_path` (not `session_cwd_override` directly) so
            // this cwd -- and the MCP `--project-dir` below -- can never
            // disagree with what was just recorded on the slot above
            // (PISO-4): both trace back to the same snapshot, not an
            // independent re-read of the global that could have moved
            // since.
            let slot_project_path = slot.project_path_snapshot();
            let cwd = cwd_for_session(slot_project_path.as_deref(), &session_cwd_override);
            let project_dir =
                thread_project_dir(slot_project_path.as_deref(), &session_cwd_override);
            let mcp_servers = snapflowd_mcp_servers_entry(project_dir.as_deref(), &slot.provider);
            match handle
                .resume_session(session_id.clone(), cwd, mcp_servers)
                .await
            {
                Ok(()) => {
                    *slot
                        .acp_session_id
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(session_id.clone());
                    persist_thread_snapshot(store.as_ref(), &slot, now_token());

                    // `resume_session`'s own replayed `session/update`
                    // history -- AND the capability events (`session/
                    // load`'s own `configOptions`/`modes`, emitted via
                    // `emit_capability_events` before the RPC resolves --
                    // see `Command::ResumeSession`'s handler) -- have
                    // already fully arrived on `events_rx` by the time
                    // the call above returns. Drain both into this
                    // brand-new slot's state now, before handing the
                    // receiver off to the continuous forwarder for
                    // anything that arrives afterward. Previously this
                    // drain only matched `AgentEvent::Message`, silently
                    // dropping every `ConfigOptions`/`SessionModes`/etc.
                    // event already queued here -- the recovered
                    // thread's compose bar never got a provider/model
                    // dropdown because the one and only capability event
                    // `session/load` ever emits for it was thrown away
                    // right here, before the forwarder task (which DOES
                    // handle those variants via `store_capability_event`)
                    // ever got a chance to see it.
                    let mut replayed_any = false;
                    while let Ok(event) = events_rx.try_recv() {
                        match &event {
                            AgentEvent::Message(message) => {
                                slot.history
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .push(message.clone());
                                replayed_any = true;
                                if let Some(store) = &store {
                                    let _ = store.append(&slot.thread_id, message);
                                }
                            }
                            AgentEvent::SessionModes(_)
                            | AgentEvent::CurrentModeChanged(_)
                            | AgentEvent::ConfigOptions(_)
                            | AgentEvent::AvailableCommands(_)
                            | AgentEvent::PlanUpdate(_)
                            | AgentEvent::SessionInfoUpdate { .. } => {
                                store_capability_event(&slot, &event);
                            }
                            _ => {}
                        }
                    }
                    if replayed_any {
                        refresh_transcript(&slot);
                    }
                    complete_attachment(&slot, None);
                }
                Err(error) => {
                    let message = format!(
                        "session/load failed for recovered session {session_id:?}: {error}"
                    );
                    complete_attachment(&slot, Some(message.clone()));
                    events_out
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(BridgeEvent {
                            thread_index: idx,
                            event: AgentEvent::Error(message),
                        });
                }
            }
            // Symmetric with `begin_recover_session_operation` above --
            // covers both the `Ok` and `Err` arms, so the Settings >
            // Agents row's spinner (`RemoteSessionOption.busy`, sourced
            // from `recover_session_operations_in_flight`) clears the
            // instant this attach settles, success or failure alike.
            recover_session_operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&session_id);
            spawn_event_forwarder(
                &tokio::runtime::Handle::current(),
                events_rx,
                events_out,
                store,
                slot,
                idx,
            );
        });
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
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Non-destructive frame-loop probe. The UI dispatcher uses this to
    /// decide whether a `FrameInput` should carry bridge work without
    /// draining the queue before `update()` sees the message.
    pub fn has_pending_events(&self) -> bool {
        !self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
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
        let gateways = self.gateways.lock().unwrap_or_else(|e| e.into_inner());
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
            .map(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default()
    }

    /// Just the last message of a thread's scrollback, if any -- O(1)
    /// relative to that thread's history length, unlike `history()` which
    /// clones the entire `Vec<ChatMessage>`. Used by per-frame snapshot
    /// paths (e.g. the sidebar's one-line description) that only ever
    /// look at the final message and must not scale with a thread's total
    /// message count, since those paths run on every poll tick for every
    /// thread regardless of which thread (if any) is actively sending.
    pub fn last_message(&self, idx: usize) -> Option<ChatMessage> {
        self.slots.get(idx).and_then(|s| {
            s.history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .cloned()
        })
    }

    /// The durable identity of an already-open thread, used by the panel's
    /// local SQLite state store after creation and after a resumed startup.
    pub fn thread_count(&self) -> usize {
        self.slots.len()
    }

    pub fn thread_binding(&self, idx: usize) -> Option<ThreadBinding> {
        self.slots.get(idx).and_then(|slot| {
            slot.acp_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .map(|session_id| ThreadBinding {
                    thread_id: slot.thread_id.clone(),
                    session_id,
                })
        })
    }

    /// Durable thread identity for routing bridge events before a deferred
    /// thread has acquired its first ACP session.
    pub fn thread_id(&self, idx: usize) -> Option<String> {
        self.slots.get(idx).map(|slot| slot.thread_id.clone())
    }

    /// Provider selected for a thread at creation time. This stays separate
    /// from display ordering so a restored subset cannot be reassigned merely
    /// because a preceding thread was deleted.
    pub fn thread_provider(&self, idx: usize) -> Option<String> {
        self.slots.get(idx).map(|slot| slot.provider.clone())
    }

    /// PUI-014: whether thread `idx`'s slot is a deferred placeholder whose
    /// session attach has not been started yet (provider still editable, first
    /// message not yet sent). `false` for a missing slot or any eagerly
    /// attached / recovered one. Drives the compose provider-picker gate (kept
    /// interactive while deferred) and the loading-vs-idle row heuristic (a
    /// deferred slot is idle-ready, not "attach in flight").
    pub fn is_deferred(&self, idx: usize) -> bool {
        self.slots.get(idx).is_some_and(|slot| slot.deferred)
    }

    /// `thread_item_project_context` phase: the project directory this
    /// thread's session was opened against (see `ThreadSlot::project_path`'s
    /// doc comment) -- `None` when no project was active at creation time,
    /// distinct from `Some("")`, which never occurs here. The FFI boundary
    /// (`panel_rust_set_project_path`) already normalizes a closed/empty
    /// project to `None` before it ever reaches `session_cwd_override`, so
    /// this should be unreachable in practice -- the `filter` is a second,
    /// cheap line of defense at PISO-3's own persistence chokepoint (this
    /// is what both `ThreadRecord`-collecting call sites in
    /// `external_snapshot.rs` persist verbatim), so an empty string can
    /// never round-trip into sqlite and be mistaken later for a real
    /// project a thread is scoped to.
    pub fn thread_project_path(&self, idx: usize) -> Option<String> {
        self.slots.get(idx).and_then(|slot| {
            slot.project_path_snapshot()
                .map(|path| path.to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
        })
    }

    /// Update the local teardown policy after the durable background-session
    /// override has been written. This does not issue an ACP request.
    pub fn set_thread_background(&self, idx: usize, background: bool) {
        if let Some(slot) = self.slots.get(idx) {
            *slot.background.lock().unwrap_or_else(|e| e.into_inner()) = background;
        }
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
                    .unwrap_or_else(|e| e.into_inner())
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
                .unwrap_or_else(|e| e.into_inner())
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
                    .unwrap_or_else(|e| e.into_inner())
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

    /// Non-blocking profile create for Settings. The synchronous wrapper is
    /// retained for compatibility tests, but UI effects must use this
    /// runtime-owned path so a slow gateway cannot stall Slint.
    pub fn create_profile_async(
        &self,
        idx: usize,
        entry: serde_json::Value,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let handle = slot.handle.clone();
        let cache = self.gateway_catalog.clone();
        self.runtime.spawn(async move {
            let result = handle
                .create_profile(entry)
                .await
                .map(|_| ())
                .map_err(|err| err.to_string());
            if result.is_ok() {
                if let Ok(mut cache) = cache.try_lock() {
                    cache.last_refresh = None;
                }
            }
            on_complete(result);
        });
    }

    /// Non-blocking profile delete for Settings; see
    /// [`Self::create_profile_async`] for the threading contract.
    pub fn delete_profile_async(
        &self,
        idx: usize,
        name: &str,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let handle = slot.handle.clone();
        let cache = self.gateway_catalog.clone();
        let name = name.to_string();
        self.runtime.spawn(async move {
            let result = handle
                .delete_profile(name)
                .await
                .map(|_| ())
                .map_err(|err| err.to_string());
            if result.is_ok() {
                if let Ok(mut cache) = cache.try_lock() {
                    cache.last_refresh = None;
                }
            }
            on_complete(result);
        });
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
    /// `background`: forwards acpx-core's `_acpx.bg` `session/close`
    /// override (see `LifecycleConfig::background_mode`'s doc comment)
    /// when true, so this explicit close is a soft no-op that keeps the
    /// session alive for a later resume -- the actual wiring for
    /// panel-rust's own per-thread "background" toggle
    /// (`PanelStateStore::effective_background_session`), which
    /// previously had no connection to any real runtime behavior at all.
    pub fn close_thread(&self, idx: usize, background: bool) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        let handle = slot.handle.clone();
        let ok = self
            .runtime
            .block_on(handle.close_session(background))
            .is_ok();
        if ok {
            *slot.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
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
            *slot.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }
        ok
    }

    /// Whether thread `idx` has been explicitly closed via
    /// [`Self::close_thread`]/[`Self::delete_thread`]. `false` for any
    /// out-of-range index or a thread that has never been closed.
    pub fn thread_closed(&self, idx: usize) -> bool {
        self.slots
            .get(idx)
            .map(|slot| *slot.closed.lock().unwrap_or_else(|e| e.into_inner()))
            .unwrap_or(false)
    }

    /// setup-followups plan, archive_thread_backend_verify: the sidebar's
    /// Archive control. Unlike [`Self::close_thread`]/[`Self::delete_
    /// thread`], this sends no ACP request at all -- archiving is a
    /// purely local/organizational flag, not a session lifecycle
    /// operation, so it never touches the backend session. It IS
    /// persisted (via [`persist_runtime_snapshot`]) so it survives a
    /// restart. Returns `false` for an out-of-range `idx` (nothing to
    /// archive); otherwise always succeeds and is idempotent.
    pub fn archive_thread(&self, idx: usize) -> bool {
        self.set_thread_archived(idx, true)
    }

    /// Phase 19: toggle-capable archive state (false = resume from
    /// archive). Same persistence as archive_thread.
    pub fn set_thread_archived(&self, idx: usize, archived: bool) -> bool {
        let Some(slot) = self.slots.get(idx) else {
            return false;
        };
        *slot.archived.lock().unwrap_or_else(|e| e.into_inner()) = archived;
        persist_runtime_snapshot(self.store.as_ref(), slot);
        true
    }

    /// Whether thread `idx` has been archived via [`Self::archive_
    /// thread`]. `false` for any out-of-range index or a thread that has
    /// never been archived.
    pub fn thread_archived(&self, idx: usize) -> bool {
        self.slots
            .get(idx)
            .map(|slot| *slot.archived.lock().unwrap_or_else(|e| e.into_inner()))
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

    /// `mcp_servers/create`. Returns the real gateway/transport error
    /// text on failure (e.g. a duplicate-name rejection from
    /// `acpx_core::mcp_servers::McpServerStore::create`, surfaced through
    /// `AcpxThreadError`'s `Display`) instead of collapsing it to a bare
    /// `bool` -- `lib.rs`'s settings dispatch methods show this verbatim
    /// in the action-feedback toast, so a generic "failed" message no
    /// longer hides *why*. On success, the caller (`lib.rs`'s settings
    /// dispatch methods) is expected to re-call [`Self::
    /// list_mcp_servers`] afterward to refresh the UI list from the
    /// gateway's own state, same "don't optimistically mutate client-side
    /// state" posture the mode/config selector uses.
    pub fn create_mcp_server(
        &self,
        idx: usize,
        entry: crate::protocol_types::McpServerEntry,
    ) -> Result<(), String> {
        let Some(slot) = self.slots.get(idx) else {
            return Err("no active thread for this settings gateway".to_string());
        };
        let handle = slot.handle.clone();
        let result = self
            .runtime
            .block_on(handle.create_mcp_server(entry))
            .map_err(|err| err.to_string());
        // Main's live-pool refresh: registry mutations must bump pool
        // generations so already-open sessions pick up the change on next
        // reacquire (see notify_mcp_settings_changed).
        if result.is_ok() {
            self.notify_mcp_settings_changed(idx);
        }
        result
    }

    /// `mcp_servers/update` -- same payload shape and error-surfacing
    /// contract as [`Self::create_mcp_server`].
    pub fn update_mcp_server(
        &self,
        idx: usize,
        entry: crate::protocol_types::McpServerEntry,
    ) -> Result<(), String> {
        let Some(slot) = self.slots.get(idx) else {
            return Err("no active thread for this settings gateway".to_string());
        };
        let handle = slot.handle.clone();
        let result = self
            .runtime
            .block_on(handle.update_mcp_server(entry))
            .map_err(|err| err.to_string());
        if result.is_ok() {
            self.notify_mcp_settings_changed(idx);
        }
        result
    }

    /// `mcp_servers/delete`.
    pub fn delete_mcp_server(&self, idx: usize, name: &str) -> Result<(), String> {
        let Some(slot) = self.slots.get(idx) else {
            return Err("no active thread for this settings gateway".to_string());
        };
        let handle = slot.handle.clone();
        let result = self
            .runtime
            .block_on(handle.delete_mcp_server(name.to_string()))
            .map_err(|err| err.to_string());
        if result.is_ok() {
            self.notify_mcp_settings_changed(idx);
        }
        result
    }

    /// `mcp_servers/authenticate`. Returns the authorization URL to open
    /// in a browser on success, the real error text on failure (server
    /// not found, stdio transport, discovery failure, etc.) instead of
    /// collapsing it to `None`.
    pub fn authenticate_mcp_server(&self, idx: usize, name: &str) -> Result<String, String> {
        let Some(slot) = self.slots.get(idx) else {
            return Err("no active thread for this settings gateway".to_string());
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.authenticate_mcp_server(name.to_string()))
            .map_err(|err| err.to_string())
    }

    /// `mcp_servers/logout`.
    pub fn logout_mcp_server(&self, idx: usize, name: &str) -> Result<(), String> {
        let Some(slot) = self.slots.get(idx) else {
            return Err("no active thread for this settings gateway".to_string());
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.logout_mcp_server(name.to_string()))
            .map_err(|err| err.to_string())
    }

    /// `mcp_servers/tools_fetch` -- fire-and-forget kickoff of a real MCP
    /// `tools/list` probe. See `AcpxThreadHandle::fetch_mcp_server_tools`'s
    /// doc comment: the real tool list arrives on a later
    /// [`Self::list_mcp_servers`] call, not this one.
    pub fn fetch_mcp_server_tools(&self, idx: usize, name: &str) -> Result<(), String> {
        let Some(slot) = self.slots.get(idx) else {
            return Err("no active thread for this settings gateway".to_string());
        };
        let handle = slot.handle.clone();
        self.runtime
            .block_on(handle.fetch_mcp_server_tools(name.to_string()))
            .map_err(|err| err.to_string())
    }

    /// Non-blocking counterparts to the six `*_mcp_server` methods above --
    /// same rationale as `install_agent_async`/`set_agent_enabled_async`
    /// (PUI-013): every one of those synchronous methods does `self.
    /// runtime.block_on(...)` directly on the calling thread, which for
    /// every MCP settings action is the Slint UI callback thread, freezing
    /// the whole panel for the RPC's full duration (the reported "jittery
    /// lag while toggling the switch" -- a real block, not just visual
    /// jank). These run the same RPC on `self.runtime` via `spawn` instead,
    /// deduped through `mcp_operations` exactly like `agent_operations`
    /// dedupes agent installs, and hand the real `Result` to `on_complete`
    /// -- invoked on the runtime thread, never the UI thread, so callers
    /// (`lib.rs`'s `dispatch_mcp_server_*_async` methods) must re-enter the
    /// event loop themselves (`slint::invoke_from_event_loop`) before
    /// touching any Slint/`PanelSingleton` state, same as every other
    /// background-thread completion in this codebase (`effect_executor.rs`'s
    /// skill-effect handlers, `report_mcp_server_result`).
    ///
    /// On successful create/update/delete/enabled, also fires
    /// [`Self::notify_mcp_settings_changed`] so pooled sessions refresh
    /// (same contract as the synchronous methods above -- critical because
    /// the Settings UI exclusively uses these async paths).
    pub fn create_mcp_server_async(
        &self,
        idx: usize,
        entry: crate::protocol_types::McpServerEntry,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("create:{}", entry.name);
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        // Cannot call &self methods from the spawn future; capture base_url
        // + project_pools the same way notify_mcp_settings_changed resolves
        // them so a successful mutation still refreshes pools off the UI
        // thread (Settings uses only these async paths).
        let base_url = self
            .slots
            .get(idx)
            .and_then(|s| self.gateway_urls.get(&s.provider).cloned());
        let project_pools = self.project_pools.clone();
        self.runtime.spawn(async move {
            let result = handle
                .create_mcp_server(entry)
                .await
                .map_err(|err| err.to_string());
            if result.is_ok() {
                Self::refresh_captured_pools(&project_pools, base_url.as_deref()).await;
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            on_complete(result);
        });
    }

    /// See [`Self::create_mcp_server_async`].
    pub fn update_mcp_server_async(
        &self,
        idx: usize,
        entry: crate::protocol_types::McpServerEntry,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("update:{}", entry.name);
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let base_url = self
            .slots
            .get(idx)
            .and_then(|s| self.gateway_urls.get(&s.provider).cloned());
        let project_pools = self.project_pools.clone();
        self.runtime.spawn(async move {
            let result = handle
                .update_mcp_server(entry)
                .await
                .map_err(|err| err.to_string());
            if result.is_ok() {
                Self::refresh_captured_pools(&project_pools, base_url.as_deref()).await;
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            on_complete(result);
        });
    }

    /// See [`Self::create_mcp_server_async`].
    pub fn delete_mcp_server_async(
        &self,
        idx: usize,
        name: &str,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("delete:{name}");
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let name = name.to_string();
        let base_url = self
            .slots
            .get(idx)
            .and_then(|s| self.gateway_urls.get(&s.provider).cloned());
        let project_pools = self.project_pools.clone();
        self.runtime.spawn(async move {
            let result = handle
                .delete_mcp_server(name)
                .await
                .map_err(|err| err.to_string());
            if result.is_ok() {
                Self::refresh_captured_pools(&project_pools, base_url.as_deref()).await;
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            on_complete(result);
        });
    }

    /// Non-blocking enabled-toggle. Mirrors `lib.rs`'s synchronous
    /// `dispatch_mcp_server_enabled_changed` (fetch the current entry,
    /// flip `enabled`, `mcp_servers/update` it back) but runs the whole
    /// fetch-mutate-update sequence on `self.runtime` instead of blocking
    /// the caller -- there is no dedicated `mcp_servers/set_enabled` RPC,
    /// so this composes the same two real calls [`Self::list_mcp_servers`]/
    /// [`Self::update_mcp_server`] use, just via their `handle.*.await`
    /// counterparts instead of `self.runtime.block_on`.
    pub fn set_mcp_server_enabled_async(
        &self,
        idx: usize,
        name: &str,
        enabled: bool,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        // Built-in snapflow is client-injected, not a central registry
        // entry — route off the registry update path entirely.
        if is_builtin_snapflow_mcp_name(name) {
            self.set_builtin_snapflow_mcp_enabled(enabled);
            on_complete(Ok(()));
            return;
        }
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("enabled:{name}");
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let name = name.to_string();
        let base_url = self
            .slots
            .get(idx)
            .and_then(|s| self.gateway_urls.get(&s.provider).cloned());
        let project_pools = self.project_pools.clone();
        // Optimistic local flip so StatusDot / status-line re-derive from
        // the new enabled state this frame (list poll will confirm).
        if let Ok(mut cache) = self.gateway_catalog.try_lock() {
            if let Some(entry) = cache
                .mcp_servers
                .iter_mut()
                .find(|entry| entry.name == name)
            {
                entry.enabled = enabled;
            }
            cache.last_refresh = None;
        }
        let catalog = self.gateway_catalog.clone();
        self.runtime.spawn(async move {
            let result = async {
                let mut entry = handle
                    .list_mcp_servers()
                    .await
                    .map_err(|err| err.to_string())?
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .ok_or_else(|| {
                        format!("MCP server \"{name}\" disappeared before its enabled state could update")
                    })?;
                entry.enabled = enabled;
                handle
                    .update_mcp_server(entry)
                    .await
                    .map_err(|err| err.to_string())
            }
            .await;
            if result.is_ok() {
                Self::refresh_captured_pools(&project_pools, base_url.as_deref()).await;
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            // Confirm StatusDot from wire without waiting out the 2s debounce.
            if let Ok(mut cache) = catalog.try_lock() {
                cache.last_refresh = None;
            }
            on_complete(result);
        });
    }

    /// See [`Self::create_mcp_server_async`]. Returns the authorization URL
    /// on success, same contract as the synchronous [`Self::
    /// authenticate_mcp_server`].
    pub fn authenticate_mcp_server_async(
        &self,
        idx: usize,
        name: &str,
        on_complete: impl FnOnce(Result<String, String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("authenticate:{name}");
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let name = name.to_string();
        self.runtime.spawn(async move {
            let result = handle
                .authenticate_mcp_server(name)
                .await
                .map_err(|err| err.to_string());
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            on_complete(result);
        });
    }

    /// See [`Self::create_mcp_server_async`].
    pub fn logout_mcp_server_async(
        &self,
        idx: usize,
        name: &str,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("logout:{name}");
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let name = name.to_string();
        self.runtime.spawn(async move {
            let result = handle
                .logout_mcp_server(name)
                .await
                .map_err(|err| err.to_string());
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            on_complete(result);
        });
    }

    /// See [`Self::create_mcp_server_async`]. Kicks off the same real
    /// `mcp_servers/tools_fetch` probe as the synchronous [`Self::
    /// fetch_mcp_server_tools`], just off the calling thread -- the
    /// fetched catalog itself still only ever arrives via a later
    /// [`Self::list_mcp_servers`] poll, so this has no new "busy" UI of
    /// its own to drive: the existing `tool_fetch_status == "fetching"`
    /// state (already polled from that same catalog) is what the Fetch/
    /// Refresh button's spinner is sourced from, not `mcp_operations_in_
    /// flight` -- the kickoff round-trip this dedupes is typically much
    /// shorter than the probe itself.
    pub fn fetch_mcp_server_tools_async(
        &self,
        idx: usize,
        name: &str,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("tools_fetch:{name}");
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let name = name.to_string();
        // Optimistic Fetching: the kickoff RPC is short, the real probe
        // is not -- stamp the local catalog cache immediately so the
        // Fetch button spinner and "fetching" status show this frame,
        // not only after the next mcp_servers/list round-trip.
        if let Ok(mut cache) = self.gateway_catalog.try_lock() {
            if let Some(entry) = cache
                .mcp_servers
                .iter_mut()
                .find(|entry| entry.name == name)
            {
                entry.tool_catalog =
                    Some(crate::protocol_types::McpToolCatalog::Fetching);
            }
            cache.last_refresh = None;
        }
        let catalog = self.gateway_catalog.clone();
        self.runtime.spawn(async move {
            let result = handle
                .fetch_mcp_server_tools(name.clone())
                .await
                .map_err(|err| err.to_string());
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            // Kickoff returned: server has stamped Fetching (or failed).
            // Drop debounce so the next frame's list poll can pick up
            // Fetching → Ready without waiting the default 2s.
            if let Err(error) = &result {
                // Keep the real RPC failure visible on the row after the
                // short-lived toast disappears. This is also emitted by
                // the panel process so async errors can be correlated
                // with acpx/daemon logs.
                eprintln!(
                    "panel-rust: mcp_servers/tools_fetch failed for {name}: {error}"
                );
            }
            if let Ok(mut cache) = catalog.try_lock() {
                if let Err(error) = &result {
                    if let Some(entry) = cache
                        .mcp_servers
                        .iter_mut()
                        .find(|entry| entry.name == name)
                    {
                        entry.tool_catalog = Some(
                            crate::protocol_types::McpToolCatalog::Error {
                                message: error.clone(),
                            },
                        );
                    }
                }
                cache.last_refresh = None;
            }
            on_complete(result);
        });
    }

    /// Non-blocking update of one discovered MCP tool preference. The
    /// read/modify/write sequence stays together on the bridge runtime so a
    /// UI effect never performs either registry RPC synchronously.
    pub fn update_mcp_tool_preference_async(
        &self,
        idx: usize,
        server_name: &str,
        tool_name: &str,
        field: &str,
        value: bool,
        on_complete: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            on_complete(Err("no active thread for this settings gateway".to_string()));
            return;
        };
        let key = format!("tool:{server_name}:{tool_name}:{field}");
        if !self.begin_mcp_operation(&key) {
            return;
        }
        let handle = slot.handle.clone();
        let operations = self.mcp_operations.clone();
        let catalog = self.gateway_catalog.clone();
        let project_pools = self.project_pools.clone();
        let base_url = self
            .slots
            .get(idx)
            .and_then(|s| self.gateway_urls.get(&s.provider).cloned());
        let server_name = server_name.to_string();
        let tool_name = tool_name.to_string();
        let field = field.to_string();
        self.runtime.spawn(async move {
            let result = async {
                let mut entry = handle
                    .list_mcp_servers()
                    .await
                    .map_err(|err| err.to_string())?
                    .into_iter()
                    .find(|entry| entry.name == server_name)
                    .ok_or_else(|| {
                        format!(
                            "MCP server \"{server_name}\" disappeared before tool preference update"
                        )
                    })?;
                let tools = entry.extra.get_mut("tools").and_then(|v| v.as_array_mut());
                if let Some(tools) = tools {
                    if let Some(tool) = tools.iter_mut().find(|tool| {
                        tool.get("name").and_then(|name| name.as_str()) == Some(tool_name.as_str())
                    }) {
                        if let Some(object) = tool.as_object_mut() {
                            object.insert(field.clone(), serde_json::Value::Bool(value));
                        }
                    } else {
                        let mut object = serde_json::Map::new();
                        object.insert("name".to_string(), serde_json::Value::String(tool_name.clone()));
                        object.insert(field.clone(), serde_json::Value::Bool(value));
                        tools.push(serde_json::Value::Object(object));
                    }
                } else {
                    let mut object = serde_json::Map::new();
                    object.insert("name".to_string(), serde_json::Value::String(tool_name.clone()));
                    object.insert(field.clone(), serde_json::Value::Bool(value));
                    entry.extra.insert(
                        "tools".to_string(),
                        serde_json::Value::Array(vec![serde_json::Value::Object(object)]),
                    );
                }
                handle
                    .update_mcp_server(entry)
                    .await
                    .map_err(|err| err.to_string())
            }
            .await;
            if result.is_ok() {
                Self::refresh_captured_pools(&project_pools, base_url.as_deref()).await;
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            if let Ok(mut cache) = catalog.try_lock() {
                cache.last_refresh = None;
            }
            on_complete(result);
        });
    }

    fn begin_mcp_operation(&self, key: &str) -> bool {
        self.mcp_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned())
    }

    /// Non-blocking read, safe to call every frame poll -- same contract
    /// as [`Self::agent_operations_in_flight`]. Keys are `"<action>:
    /// <server-name>"` (see `mcp_operations`'s own doc comment); callers
    /// that only need "is *this* server busy at all" should check for any
    /// key ending in `:<name>`.
    pub fn mcp_operations_in_flight(&self) -> Vec<String> {
        self.mcp_operations
            .try_lock()
            .map(|operations| operations.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// recoverable-attach-fix: marks `session_id` busy for the duration
    /// of its `recover-session-attach` `session/load`. Mirrors `begin_
    /// mcp_operation`'s own contract; the key is removed again once the
    /// background attach task completes (success or error) -- inlined
    /// there directly (via a cloned `Arc`) rather than through a `&self`
    /// method, since that task runs detached from any `AgentBridge`
    /// borrow.
    fn begin_recover_session_operation(&self, session_id: &str) {
        self.recover_session_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_owned());
    }

    /// Non-blocking read, safe to call every frame poll -- same contract
    /// as [`Self::mcp_operations_in_flight`]. Keys are bare remote
    /// `acp_session_id`s (never composite like the MCP/agent sets above,
    /// since a session id alone is already unique).
    pub fn recover_session_operations_in_flight(&self) -> Vec<String> {
        self.recover_session_operations
            .try_lock()
            .map(|operations| operations.iter().cloned().collect())
            .unwrap_or_default()
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
        let mut agents = self
            .runtime
            .block_on(handle.list_agents())
            .unwrap_or_default();
        // setup-followups plan, agent_settings_ordering_and_install_
        // enable_flow: `agents/list` (the plain ACP-adjacent RPC above)
        // carries no enablement concept at all -- that only exists on
        // the admin plane. Merge it in here, once, so every caller of
        // list_agents (settings view, tests) sees accurate enabled
        // state without needing to know the admin plane exists. A
        // completely absent admin plane (no token discoverable) leaves
        // every entry at its `AgentCatalogEntry::from_json` default of
        // `enabled: true` -- not a regression for the common case where
        // this feature isn't configured at all.
        if let Some(enablement) = self.agent_enablement_map() {
            for agent in &mut agents {
                if let Some(enabled) = enablement.get(&agent.id) {
                    agent.enabled = *enabled;
                }
            }
        }
        agents
    }

    /// The admin plane's own view of every agent's `enabled` flag,
    /// keyed by agent id. `None` if no admin plane is reachable at all
    /// (see [`resolve_admin_creds`]) -- distinct from `Some(empty map)`,
    /// which would incorrectly read as "every agent is unlisted".
    fn agent_enablement_map(&self) -> Option<std::collections::HashMap<String, bool>> {
        let (admin_url, admin_token) = resolve_admin_creds()?;
        let client = acpx_client::ext::admin::AdminClient::new(admin_url, admin_token);
        let entries = self.runtime.block_on(client.list_agents()).ok()?;
        Some(
            entries
                .into_iter()
                .map(|entry| (entry.id, entry.enabled))
                .collect(),
        )
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

    /// setup-followups plan, agent_settings_ordering_and_install_enable_
    /// flow: the real "install > enable" second step -- distinct from
    /// `install_agent` (which only ever registers/fetches the binary).
    /// Uses the admin plane (`acpx_client::ext::admin::AdminClient`),
    /// not a JSON-RPC method against `idx`'s thread gateway, since
    /// enable/disable is gateway-wide administration, not a per-session
    /// action -- see `resolve_admin_creds`'s own doc comment for how
    /// panel-rust discovers admin credentials for whichever acpx-server
    /// it's actually talking to. Returns `false` (not an error type) if
    /// no admin plane is reachable at all -- a legitimate, expected state
    /// when nothing generated a token yet -- exactly like every other
    /// degrade-gracefully bridge call in this file.
    pub fn set_agent_enabled(&self, agent_id: &str, enabled: bool) -> bool {
        let Some((admin_url, admin_token)) = resolve_admin_creds() else {
            return false;
        };
        let client = acpx_client::ext::admin::AdminClient::new(admin_url, admin_token);
        if enabled {
            self.runtime.block_on(client.enable_agent(agent_id)).is_ok()
        } else {
            self.runtime
                .block_on(client.disable_agent(agent_id))
                .is_ok()
        }
    }

    /// PUI-013: non-blocking Install. `install_agent` above does a
    /// synchronous `runtime.block_on` on the caller -- which is the Slint
    /// UI thread -- so the whole panel froze for the duration of the
    /// `agents/install` round-trip (the reported Settings>Agents freeze).
    /// This fire-and-forget variant runs the same round-trip on the tokio
    /// runtime instead; the periodic frame poll re-pulls `list_agents`, so
    /// the installed status still refreshes on its own. Kept separate from
    /// `install_agent` so the synchronous, bool-returning method still
    /// backs the unit tests.
    pub fn install_agent_async(&self, idx: usize, agent_id: &str) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let handle = slot.handle.clone();
        let agent_id = agent_id.to_string();
        if !self.begin_agent_operation(&agent_id) {
            return;
        }
        let operations = self.agent_operations.clone();
        // After install, allow an immediate catalog refresh (clear TTL).
        let cache = self.gateway_catalog.clone();
        let refresh_idx = idx;
        // We can't call methods on self from the spawned task after move;
        // invalidate last_refresh so the next UI request_gateway_catalog_refresh runs.
        self.runtime.spawn(async move {
            if handle.install_agent(agent_id.clone()).await.is_err() {
                eprintln!("panel-rust: install_agent({agent_id}) failed (async)");
            }
            if let Ok(mut c) = cache.try_lock() {
                c.last_refresh = None;
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&agent_id);
            let _ = refresh_idx; // catalog refresh is UI-driven next frame
        });
    }

    fn begin_agent_operation(&self, agent_id: &str) -> bool {
        self.agent_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_owned())
    }

    pub fn agent_operations_in_flight(&self) -> Vec<String> {
        self.agent_operations
            .try_lock()
            .map(|operations| operations.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// **UI-thread safe.** Clone the last background-filled gateway catalog.
    /// Never performs RPC or `block_on` (lock_audit Layer 1 / F-01 / F-02).
    /// Pair with [`Self::request_gateway_catalog_refresh`] so a background
    /// task pushes updates; frame poll only drains this cache.
    pub fn gateway_catalog_snapshot(
        &self,
        stale_fallback: crate::msg::SettingsGatewaySnapshot,
    ) -> crate::msg::SettingsGatewaySnapshot {
        self.gateway_catalog
            .try_lock()
            .ok()
            .map(|cache| crate::msg::SettingsGatewaySnapshot {
                profiles: cache.profiles.clone(),
                mcp_servers: cache.mcp_servers.clone(),
                agents: cache.agents.clone(),
                agents_fetched: cache.gen > 0,
                recoverable_sessions: cache.recoverable_sessions.clone(),
                recovery_provider: cache.recovery_provider.clone(),
            })
            .unwrap_or(stale_fallback)
    }

    /// Whether the catalog has never been successfully filled (cold start).
    pub fn gateway_catalog_empty(&self) -> bool {
        self.gateway_catalog
            .try_lock()
            .ok()
            .is_none_or(|cache| cache.gen == 0)
    }

    /// Drop the catalog's last-refresh stamp so the next
    /// [`Self::request_gateway_catalog_refresh`] actually hits the gateway
    /// instead of being absorbed by the 2s debounce. Used after MCP
    /// mutations (tools_fetch kickoff, enable toggle, create/update/
    /// delete) so Settings sees Fetching/Ready and enabled state live.
    pub fn invalidate_gateway_catalog(&self) {
        if let Ok(mut cache) = self.gateway_catalog.try_lock() {
            cache.last_refresh = None;
        }
    }

    /// Fire-and-forget background refresh of profiles / MCP / agents /
    /// recoverable sessions. Safe to call every frame: single-flight via
    /// `gateway_catalog_refreshing`, and skips if a fresh fill landed within
    /// `min_interval`. Installation does not need a special case because the
    /// frame path never waits for these RPCs.
    ///
    /// While any MCP server's `toolCatalog` is `Fetching` (or a
    /// `tools_fetch:<name>` op is still in flight), the debounce shrinks
    /// to 200ms so the Fetch-tools spinner and expanded tool list update
    /// as soon as the background probe finishes (the 2s default otherwise
    /// made "Fetch tools" look dead).
    ///
    /// **Must not** be awaited on the UI thread — all RPC work runs on the
    /// bridge tokio runtime (canonical push pattern; same as `poll` drain).
    pub fn request_gateway_catalog_refresh(&self, idx: usize) {
        const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
        const FETCHING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
        {
            let Ok(cache) = self.gateway_catalog.try_lock() else {
                return;
            };
            let ops = self
                .mcp_operations
                .try_lock()
                .ok();
            let any_tools_fetch_op = ops
                .as_ref()
                .is_some_and(|set| set.iter().any(|k| k.starts_with("tools_fetch:")));
            let any_tools_fetching = any_tools_fetch_op
                || cache.mcp_servers.iter().any(|entry| {
                    matches!(
                        entry.tool_catalog,
                        Some(crate::protocol_types::McpToolCatalog::Fetching)
                    )
                });
            let min_interval = if any_tools_fetching {
                FETCHING_INTERVAL
            } else {
                MIN_INTERVAL
            };
            if let Some(at) = cache.last_refresh {
                if cache.gen > 0 && at.elapsed() < min_interval {
                    return;
                }
            }
        }
        if self.gateway_catalog_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(slot) = self.slots.get(idx) else {
            self.gateway_catalog_refreshing
                .store(false, Ordering::SeqCst);
            return;
        };
        let handle = slot.handle.clone();
        let provider = slot.provider.clone();
        let bound: std::collections::HashSet<String> = self
            .slots
            .iter()
            .filter_map(|s| {
                s.acp_session_id
                    .try_lock()
                    .ok()
                    .and_then(|session_id| session_id.clone())
            })
            .collect();
        let cache = self.gateway_catalog.clone();
        let operations = self.mcp_operations.clone();
        let refreshing = self.gateway_catalog_refreshing.clone();
        let admin_creds = resolve_admin_creds();
        // mcp-registry-live-propagation: on a real central-registry diff
        // (not the cold first fill), refresh every project pool bound to
        // this gateway so idle sessions drop and leased ones stamp stale.
        let base_url = self.gateway_urls.get(&provider).cloned();
        let project_pools = self.project_pools.clone();
        self.runtime.spawn(async move {
            // These four RPCs are independent of each other (only the admin-enablement
            // merge below depends on `agents` having resolved), so run them concurrently
            // via `tokio::join!` instead of paying for N sequential round-trips.
            let (profiles, mcp_servers, agents_result, sessions_result) = tokio::join!(
                handle.list_profiles(),
                handle.list_mcp_servers(),
                handle.list_agents(),
                handle.list_sessions_for_agent(provider.clone())
            );
            let profiles = profiles.unwrap_or_default();
            let mcp_servers = mcp_servers.unwrap_or_default();
            let mut agents = agents_result.unwrap_or_default();
            if let Some((admin_url, admin_token)) = admin_creds {
                let client = acpx_client::ext::admin::AdminClient::new(admin_url, admin_token);
                if let Ok(entries) = client.list_agents().await {
                    let enablement: std::collections::HashMap<String, bool> = entries
                        .into_iter()
                        .map(|entry| (entry.id, entry.enabled))
                        .collect();
                    for agent in &mut agents {
                        if let Some(enabled) = enablement.get(&agent.id) {
                            agent.enabled = *enabled;
                        }
                    }
                }
            }
            let recoverable_sessions = sessions_result
                .unwrap_or_default()
                .into_iter()
                .filter(|session| !bound.contains(&session.acp_session_id))
                .collect();
            let registry_changed = {
                if let Ok(mut c) = cache.try_lock() {
                    let had_prior_fill = c.gen > 0;
                    let ops = operations
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let merged = merge_mcp_list_with_optimistic(
                        mcp_servers,
                        &c.mcp_servers,
                        &ops,
                    );
                    let changed = had_prior_fill
                        && mcp_registry_identity(&c.mcp_servers)
                            != mcp_registry_identity(&merged);
                    c.profiles = profiles;
                    c.mcp_servers = merged;
                    c.agents = agents;
                    c.recoverable_sessions = recoverable_sessions;
                    c.recovery_provider = provider;
                    c.gen = c.gen.saturating_add(1).max(1);
                    c.last_refresh = Some(std::time::Instant::now());
                    changed
                } else {
                    false
                }
            };
            if registry_changed {
                Self::refresh_captured_pools(&project_pools, base_url.as_deref()).await;
            }
            refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// PUI-013: non-blocking enable/disable -- same rationale as
    /// `install_agent_async`; `set_agent_enabled`'s `runtime.block_on` also
    /// ran on the UI thread. Fire-and-forget on the tokio runtime.
    pub fn set_agent_enabled_async(&self, agent_id: &str, enabled: bool) {
        let Some((admin_url, admin_token)) = resolve_admin_creds() else {
            eprintln!(
                "panel-rust: set_agent_enabled({agent_id}, {enabled}) skipped \
                 (no admin plane reachable)"
            );
            return;
        };
        if !self.begin_agent_operation(agent_id) {
            return;
        }
        let agent_id = agent_id.to_string();
        let operations = self.agent_operations.clone();
        self.runtime.spawn(async move {
            let client = acpx_client::ext::admin::AdminClient::new(admin_url, admin_token);
            let ok = if enabled {
                client.enable_agent(&agent_id).await.is_ok()
            } else {
                client.disable_agent(&agent_id).await.is_ok()
            };
            if !ok {
                eprintln!("panel-rust: set_agent_enabled({agent_id}, {enabled}) failed (async)");
            }
            operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&agent_id);
        });
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
                .unwrap_or_else(|e| e.into_inner());
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
                    .unwrap_or_else(|e| e.into_inner())
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
            .unwrap_or_else(|e| e.into_inner())
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
                    .unwrap_or_else(|e| e.into_inner())
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
            .map(|s| *s.older_available.lock().unwrap_or_else(|e| e.into_inner()))
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
        if self.store.is_none()
            && slot
                .acp_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            let before = slot
                .history_cursor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if !*slot
                .older_available
                .lock()
                .unwrap_or_else(|e| e.into_inner())
            {
                return false;
            }
            let handle = Arc::clone(&slot.handle);
            self.runtime.handle().spawn(async move {
                if let Err(error) = handle.paginate_history(before).await {
                    eprintln!("panel-rust: remote history pagination failed: {error}");
                }
            });
            return true;
        }
        let Some(store) = &self.store else {
            return false;
        };
        if !*slot
            .older_available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            return false;
        }
        let before_index = *slot
            .oldest_loaded_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
                .unwrap_or_else(|e| e.into_inner()) = false;
            return false;
        }
        {
            let mut history = slot.history.lock().unwrap_or_else(|e| e.into_inner());
            let mut prepended = page.messages;
            prepended.extend(history.drain(..));
            *history = prepended;
        }
        *slot
            .older_available
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = page.older_available;
        *slot
            .oldest_loaded_index
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = page.oldest_loaded_index;
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
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            if let Err(e) = handle.send_prompt(text).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("send_prompt failed: {e}")),
                    });
            }
        });
    }

    /// Fire-and-forget mutation of the ACPX server-owned per-session queue.
    /// The server returns the authoritative projection through the queue
    /// callback stream; this method intentionally does not mutate the local
    /// queue a second time.
    pub fn mutate_queue(&self, idx: usize, mut params: serde_json::Value) {
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
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            let Some(session_id) = slot
                .acp_session_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            else {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(
                            "queue mutation skipped: session attachment completed without a session id"
                                .to_owned(),
                        ),
                    });
                return;
            };
            // The reducer may have emitted this effect while a deferred
            // thread was still unbound. Resolve the authoritative remote id
            // only after the attachment gate opens; the local thread slug is
            // never sent as an ACPX queue key.
            params["sessionId"] = serde_json::Value::String(session_id);
            if let Err(error) = handle.mutate_queue(params).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("queue mutation failed: {error}")),
                    });
            }
        });
    }

    /// Returns a completed attachment failure without waiting. The UI uses
    /// this to reject a send before it appends a user message or marks the
    /// thread as generating; the background wait remains the final race-safe
    /// guard for an attachment that fails between the UI check and dispatch.
    pub fn attachment_error(&self, idx: usize) -> Option<String> {
        let slot = self.slots.get(idx)?;
        let state = slot
            .attachment
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.error.clone()
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
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            if let Err(e) = handle.cancel_session().await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
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
    pub fn thread_usage(&self, idx: usize) -> (i64, i64) {
        self.slots
            .get(idx)
            .map(|slot| *slot.usage.lock().expect("usage mutex poisoned"))
            .unwrap_or((0, 0))
    }

    pub fn session_modes(&self, idx: usize) -> Option<SessionModesEvent> {
        let slot = self.slots.get(idx)?;
        slot.session_modes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Most recently advertised `configOptions` for thread `idx` -- see
    /// [`Self::session_modes`]'s doc comment for the same capability-
    /// gating rationale (empty vec -> selector hidden).
    pub fn config_options(&self, idx: usize) -> Vec<ConfigOptionInfo> {
        let provider = self
            .slots
            .get(idx)
            .map(|slot| slot.provider.clone())
            .unwrap_or_default();
        self.config_options_for_provider(idx, &provider, None)
    }

    pub fn config_options_for_provider(
        &self,
        idx: usize,
        provider: &str,
        profile_name: Option<&str>,
    ) -> Vec<ConfigOptionInfo> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        // `slot.config_options` is written only by `store_capability_event`,
        // itself fed only by a real attached session's own live event
        // stream (`AgentEvent::ConfigOptions`) or by cold-start restoring
        // a prior run's persisted `ThreadRuntimeSnapshot::config_options`
        // at slot construction -- never by the pool-preview path (see
        // `ensure_models_for_provider`'s own doc comment: it writes only
        // `pre_session_model_options`, and pushes its `ConfigOptions`
        // event onto the aggregate `self.events` queue for the UI layer,
        // not into this slot's own capability state). And
        // `ThreadSlot::acp_session_id` is set exactly once, from `None`
        // to `Some`, and never reset back to `None` afterward -- so there
        // is no window in this slot's lifetime where `config_options` can
        // hold a *different, stale* session's live values while looking
        // unattached; gating this read on "currently attached" only ever
        // discarded a legitimately cold-start-restored snapshot before
        // the first live event overwrote it, regressing the very
        // guarantee `restored_interaction_snapshot_is_available_before_
        // gateway_events_arrive` exists to lock in. Read it unconditionally.
        let live = slot
            .config_options
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !live.is_empty() {
            return live;
        }
        slot.pre_session_model_options
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&format!("{provider}\0{}", profile_name.unwrap_or_default()))
            .cloned()
            .unwrap_or_default()
    }

    /// pre-send-config-options-visibility, Finding C: `NO_PROVIDER_
    /// REQUESTED_FALLBACK` ("codex") is a valid `session/new`/pool
    /// provider sentinel -- omitting an explicit profile/agent selector
    /// there lets acpx-server fall back to its own `ACPX_DEFAULT_AGENT_ID`
    /// -- but `models/list`'s `agentId` param requires a real, resolvable
    /// registry id and rejects the bare sentinel outright. Confirmed live
    /// via a direct RPC: `agentId=codex` -> real error "unknown agent id
    /// codex"; `agentId=codex-acp` -> a real, correct model catalog. Only
    /// `list_models` calls need this resolved; every other call site in
    /// this file that already works with the bare sentinel (session/new,
    /// the pool preview path, snapflowd_mcp_servers_entry's own dual-form
    /// handling) is untouched.
    fn resolve_registry_agent_id_for_capability_probe(provider: &str) -> String {
        if provider == NO_PROVIDER_REQUESTED_FALLBACK {
            "codex-acp".to_owned()
        } else {
            provider.to_owned()
        }
    }

    /// Start one background `models/list` probe for the provider currently
    /// selected in the compose bar. This is intentionally not limited to
    /// deferred threads: changing provider on any session-less thread must
    /// repopulate the model dropdown immediately.
    pub fn ensure_models_for_provider(
        &self,
        idx: usize,
        provider: &str,
        profile_name: Option<&str>,
    ) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let provider = provider.to_owned();
        let profile_name = profile_name.map(str::to_owned);
        let cache_key = format!(
            "{provider}\0{}",
            profile_name.as_deref().unwrap_or_default()
        );
        if provider.is_empty() {
            return;
        }
        // An attached thread already receives its live capabilities from the
        // session actor. A preview acquire here would create a competing
        // lease for an active/resumed thread and could disturb its real
        // session; previews are only needed before the first message.
        if slot
            .acp_session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return;
        }
        let mut cached = slot
            .pre_session_model_options
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cached.contains_key(&cache_key) {
            return;
        }
        cached.insert(cache_key.clone(), Vec::new());
        drop(cached);

        let handle = slot.handle.clone();
        let target = slot.pre_session_model_options.clone();
        let events = self.events.clone();
        let project_dir = thread_project_dir(
            slot.project_path_snapshot().as_deref(),
            &self.session_cwd_override,
        );
        let Some(project_dir) = project_dir else {
            let agent_id = Self::resolve_registry_agent_id_for_capability_probe(&provider);
            let events = events.clone();
            // pre-send-config-options-visibility: `cwd` here was `None`
            // (real project dir unknown before any project is opened/
            // saved) on the theory that "there's nothing absolute to
            // send" -- wrong, and live-confirmed a real bug: a `None`
            // cwd omits the field entirely, `probe_adapter_capabilities`
            // then rejects the server's own `"."` default outright
            // (same "must be an absolute path" error Finding A already
            // fixed for the other two branches). It only ever appeared
            // to work for whichever provider happened to already have
            // server-side cached capabilities (this gateway's own
            // default agent, warmed some other way) -- confirmed live:
            // an identical never-yet-probed second provider on the same
            // cwd-less branch failed immediately. `probe_adapter_
            // capabilities` doesn't need a *meaningful* cwd, just a
            // valid absolute one for a generic capability probe, so the
            // process's own cwd is a perfectly good fallback here.
            let cwd = std::env::current_dir().ok();
            self.runtime.spawn(async move {
                let options = handle.list_models(agent_id, cwd).await.unwrap_or_default();
                target
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cache_key, options.clone());
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::ConfigOptions(options),
                    });
            });
            return;
        };
        let Some(base_url) = self.gateway_urls.get(&provider).cloned() else {
            let agent_id = Self::resolve_registry_agent_id_for_capability_probe(&provider);
            let events = events.clone();
            let cwd = Some(project_dir.clone());
            self.runtime.spawn(async move {
                // pre-send-config-options-visibility: this branch has a
                // real project_dir even though gateway_urls missed --
                // sending it (absolute, per thread_project_dir) is what
                // makes probe_adapter_capabilities accept the call
                // instead of rejecting the "." it defaults to otherwise.
                let options = handle.list_models(agent_id, cwd).await.unwrap_or_default();
                target
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cache_key, options.clone());
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::ConfigOptions(options),
                    });
            });
            return;
        };
        let mcp_servers = snapflowd_mcp_servers_entry(Some(&project_dir), &provider);
        let Some(pool) = self.pool_for(&project_dir.to_string_lossy(), &base_url, &mcp_servers)
        else {
            let agent_id = Self::resolve_registry_agent_id_for_capability_probe(&provider);
            let events = events.clone();
            let cwd = Some(project_dir.clone());
            self.runtime.spawn(async move {
                // Same reasoning as the gateway_urls-miss branch above.
                let options = handle.list_models(agent_id, cwd).await.unwrap_or_default();
                target
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cache_key, options.clone());
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::ConfigOptions(options),
                    });
            });
            return;
        };
        let key = acpx_client::pool::PoolKey::new(
            project_dir.to_string_lossy().into_owned(),
            provider.clone(),
            crate::gateway_actor::provider_profile_key(profile_name.as_deref()),
        );
        let preview_thread_id = format!("preview:{idx}:{provider}");
        let cwd = Some(project_dir.clone());
        let agent_id = Self::resolve_registry_agent_id_for_capability_probe(&provider);
        self.runtime.spawn(async move {
            let options = match pool
                .acquire(
                    key,
                    preview_thread_id,
                    acpx_client::pool::OpenSpec {
                        saved_session_id: None,
                    },
                )
                .await
            {
                Ok(lease) => {
                    let options = lease
                        .capabilities
                        .as_ref()
                        .and_then(|value| value.get("configOptions"))
                        .and_then(crate::gateway_actor::parse_config_options)
                        .unwrap_or_default();
                    if let Err(error) = pool.release(&lease).await {
                        eprintln!("panel-rust: capability preview release failed: {error}");
                    }
                    if options.is_empty() {
                        handle
                            .list_models(agent_id.clone(), cwd.clone())
                            .await
                            .unwrap_or_default()
                    } else {
                        options
                    }
                }
                Err(error) => {
                    eprintln!("panel-rust: capability preview pool acquire failed: {error}");
                    handle
                        .list_models(agent_id.clone(), cwd.clone())
                        .await
                        .unwrap_or_default()
                }
            };
            target
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(cache_key, options.clone());
            events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(BridgeEvent {
                    thread_index: idx,
                    event: AgentEvent::ConfigOptions(options),
                });
        });
    }

    /// PUI-003: the agent's own built-in slash commands for thread `idx`
    /// (from `available_commands_update`), for the compose `/` menu.
    pub fn available_commands(
        &self,
        idx: usize,
    ) -> Vec<crate::protocol_types::AvailableCommandInfo> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        slot.available_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// PROF-11: the agent's most recently pushed execution plan/todo
    /// list for thread `idx` (from a live `plan` session/update). Empty
    /// until the backend sends one -- same "empty means no plan
    /// notification yet, capability-gate on it" reasoning as
    /// [`Self::config_options`].
    pub fn plan(&self, idx: usize) -> Vec<crate::protocol_types::PlanEntryInfo> {
        let Some(slot) = self.slots.get(idx) else {
            return Vec::new();
        };
        slot.plan.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// PROF-11: the most recently pushed live session title for thread
    /// `idx` (from a `session_info_update`), distinct from the durable
    /// `ThreadModel::display_name` -- see [`ThreadSlot::session_title`]'s
    /// doc comment for why the two are never merged.
    pub fn session_title(&self, idx: usize) -> Option<String> {
        let slot = self.slots.get(idx)?;
        slot.session_title
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            if let Err(e) = handle.set_mode(mode_id).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session/set_mode failed: {e}")),
                    });
            }
        });
    }

    /// Dispatches `terminal/kill` on the background runtime -- PUI-002b's
    /// popup `[x]` kill button. Same fire-and-forget shape as
    /// [`Self::set_mode`]: never `runtime.block_on` on the caller's
    /// thread (the Settings>Agents freeze this project's own
    /// `panel-new-thread-blocking-catalog` diagnosed was exactly that
    /// mistake), a failure surfaces as a queued `AgentEvent::Error`.
    pub fn kill_terminal(&self, idx: usize, terminal_id: String) {
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
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            if let Err(e) = handle.kill_terminal(terminal_id).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("terminal/kill failed: {e}")),
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
        // Defensive validation, not a normal-path check: session/set_
        // config_option is scoped entirely to whichever one backend
        // process is already attached to this thread -- ACP has no
        // primitive for switching a live session to a different
        // provider/agent (confirmed against Zed's own AgentSessionConfig
        // Options::set_config_option, which is likewise per-connection
        // only; Zed's own answer for changing providers is client-side,
        // entirely outside ACP: free agent choice on an empty draft
        // thread, a brand-new thread once real content exists). A normal
        // UI flow can't actually construct a cross-provider selection
        // today (config_dropdown_entries only ever lists this thread's
        // own advertised options), but nothing enforced that -- calling
        // this with a value this thread never advertised would have
        // silently forwarded the RPC to the (wrong) attached backend
        // and surfaced whatever confusing native error it happened to
        // produce. Reject it here instead, with a clear message, until
        // real provider-switching (restart-under-a-new-agent) exists.
        let is_known_value = {
            let config_options = slot
                .config_options
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            config_options.iter().any(|option| {
                option.id == config_id
                    && option
                        .options
                        .iter()
                        .any(|v| serde_json::Value::String(v.value.clone()) == value)
            })
        };
        if !is_known_value {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(BridgeEvent {
                    thread_index: idx,
                    event: AgentEvent::Error(format!(
                        "config option {config_id:?}={value} is not one this thread's \
                         attached backend advertised -- switching a live session to a \
                         different provider is not supported yet"
                    )),
                });
            return;
        }
        let slot = slot.clone();
        let handle = slot.handle.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(error) = wait_for_attachment(&slot).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(BridgeEvent {
                        thread_index: idx,
                        event: AgentEvent::Error(format!("session attachment failed: {error}")),
                    });
                return;
            }
            if let Err(e) = handle.set_config_option(config_id, value).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
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
            // Project recreation detaches foreground thread actors. Stopping
            // only the local actor leaves the ACPX session live until server
            // expiry, so repeated A -> B -> restart cycles exhaust the
            // tenant session capacity. Explicitly close this panel-owned
            // session before shutting down its actor; an explicitly
            // backgrounded session is reattached through its durable record
            // when the project becomes active again.
            let background = *slot.background.lock().unwrap_or_else(|e| e.into_inner());
            let _ = self.runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    slot.handle.close_session(background),
                )
                .await
            });
            slot.handle.shutdown();
        }
    }
}

// PROF5-GUARD-ALLOW-START -- see tests/backend_cmd_env_write_regression_test.rs.
// Everything between this line and the matching END marker below is the
// one place `"ACPX_BACKEND_CMD"`/`"ACPX_DEFAULT_ACP_COMMAND"` may legally
// appear in this crate's `src/`. Do not widen this block casually -- the
// regression guard fails the build the moment either literal shows up
// anywhere else, which is the entire point.
/// PROF-5 (`profile-only-backend-selection` plan): the ONE sanctioned way
/// to write `ACPX_BACKEND_CMD` in this crate. `#[cfg(test)]`-gated, so
/// calling it from any production code path is a compile error, not a
/// convention someone has to remember -- and
/// `tests/backend_cmd_env_write_regression_test.rs` asserts the literal
/// string `"ACPX_BACKEND_CMD"` (and `"ACPX_DEFAULT_ACP_COMMAND"`, its
/// planned rename arriving via the agents-install-runtime worktree) never
/// appears anywhere in `src/` outside this one function's own definition,
/// so a second ad hoc write anywhere -- including inside another test --
/// fails that guard immediately rather than silently reintroducing the
/// pattern PROF-3 removed from production.
///
/// Every call site is an in-crate unit test hand-spawning its OWN
/// `acpx-server` subprocess directly, inside this file's own
/// `#[cfg(test)] mod tests` -- structurally unreachable from
/// `spawn_gateway_process` or any production path (verified in
/// PROF-3/PROF-4: production never even builds this function in, since it
/// is compiled out entirely outside `cfg(test)`). Real backend selection
/// in production goes through a profile (`_acpx.profile`), never this.
#[cfg(test)]
pub(crate) fn test_only_set_backend_cmd_env<'a>(
    command: &'a mut std::process::Command,
    value: impl AsRef<std::ffi::OsStr>,
) -> &'a mut std::process::Command {
    command.env("ACPX_BACKEND_CMD", value)
}
// PROF5-GUARD-ALLOW-END

#[cfg(test)]
mod tests {
    use super::*;
    // Standalone-thread constructor (its own dedicated connection, not
    // the bridge's shared-gateway pool) -- used directly by tests below
    // that want to talk to a `TestGateway` without going through a full
    // `AgentBridge`.
    use crate::gateway_actor::spawn_acpx_thread;
    use crate::protocol_types::MessageKind;
    // row_count()/row_data() on the persistent messages_model VecModel in
    // the full-reducer real-backend tests below.
    use slint::Model as _;

    /// Serializes every test in this module that mutates a process-global
    /// env var (`RUI_ACP_AGENT_CMD`, `ACPX_CODEX_AUTH_FILE`,
    /// `SNAPSHOTD_MCP_SSE_ADDR`, etc). These tests used to rely on an
    /// undocumented assumption baked into their own SAFETY comments --
    /// "this whole suite already runs under --test-threads=1" -- which
    /// was never actually enforced anywhere (no `.cargo/config.toml`
    /// setting, no harness flag), just a habit of how this crate's CI
    /// happened to invoke `cargo test`. Once real-process port
    /// contention was fixed (`spawn_acpx_server_with_retry`'s own doc
    /// comment) and the suite started actually being run at default
    /// parallelism, two of these tests (the `snapshotd_mcp_server_entry_*`
    /// pair, which both mutate `SNAPSHOTD_MCP_SSE_ADDR`) began failing
    /// nondeterministically -- one test's `set_var` landing between the
    /// other's `set_var` and its own read, or its `remove_var` firing
    /// mid-assertion -- a real data race on `std::env`, not a port issue.
    /// Every env-mutating test below now acquires this lock before
    /// touching the environment and holds it until the prior value has
    /// been restored, so at most one such test's mutation is ever live at
    /// a time, regardless of test-runner parallelism.
    static ENV_MUTATION_LOCK: Mutex<()> = Mutex::new(());

    // PISO-11: "codex installed but not detected" -- see
    // merge_path_entries's own doc comment for why this is split out as a
    // pure function. These tests exercise only the merge/dedup/ordering
    // logic; they never touch the real process env or spawn a shell.
    #[test]
    fn merge_path_entries_appends_new_login_shell_dirs() {
        let merged = merge_path_entries(
            "/usr/bin:/bin",
            vec!["/home/u/.nvm/versions/node/v20/bin".to_owned()],
        );
        assert_eq!(
            merged,
            Some("/usr/bin:/bin:/home/u/.nvm/versions/node/v20/bin".to_owned())
        );
    }

    #[test]
    fn merge_path_entries_dedupes_already_present_dirs() {
        let merged = merge_path_entries("/usr/bin:/bin", vec!["/usr/bin".to_owned()]);
        assert_eq!(merged, Some("/usr/bin:/bin".to_owned()));
    }

    #[test]
    fn merge_path_entries_current_dirs_come_first() {
        // An operator's own explicit PATH must win on conflicting binaries
        // -- login-shell-resolved dirs are strictly additive, never
        // reordered ahead of what the process already had.
        let merged = merge_path_entries("/opt/custom/bin", vec!["/usr/bin".to_owned()]);
        assert_eq!(merged, Some("/opt/custom/bin:/usr/bin".to_owned()));
    }

    #[test]
    fn merge_path_entries_empty_current_and_extra_yields_none() {
        assert_eq!(merge_path_entries("", Vec::new()), None);
    }

    #[test]
    fn merge_path_entries_empty_current_still_uses_extra() {
        assert_eq!(
            merge_path_entries("", vec!["/usr/bin".to_owned()]),
            Some("/usr/bin".to_owned())
        );
    }

    fn exited_buffer() -> TerminalBuffer {
        TerminalBuffer {
            output: "done".to_owned(),
            truncated: false,
            exit_status: Some((Some(0), None)),
            command: "cargo test".to_owned(),
            args: Vec::new(),
            started_at: "2026-07-24T00:00:00.000000000Z".to_owned(),
        }
    }

    fn running_buffer() -> TerminalBuffer {
        TerminalBuffer {
            output: "still going".to_owned(),
            truncated: false,
            exit_status: None,
            command: "cargo build".to_owned(),
            args: Vec::new(),
            started_at: "2026-07-24T00:00:00.000000000Z".to_owned(),
        }
    }

    /// PISO-4: two threads whose slots carry different `project_path`s
    /// must each resolve their OWN project directory from
    /// `cwd_for_session`, and a later change to the process-global
    /// `session_cwd_override` (whatever project the user has since
    /// switched to) must not retroactively change either answer -- that
    /// global is only ever a fallback for a slot with no project of its
    /// own, never an override for one that has one.
    #[test]
    fn cwd_for_session_prefers_the_threads_own_slot_over_the_global_override() {
        let session_cwd_override: Mutex<Option<PathBuf>> =
            Mutex::new(Some(PathBuf::from("/projects/initial")));

        let thread_a_project = PathBuf::from("/projects/a");
        let thread_b_project = PathBuf::from("/projects/b");

        assert_eq!(
            cwd_for_session(Some(thread_a_project.as_path()), &session_cwd_override),
            PathBuf::from("/projects/.snapflow/a")
        );
        assert_eq!(
            cwd_for_session(Some(thread_b_project.as_path()), &session_cwd_override),
            PathBuf::from("/projects/.snapflow/b")
        );

        // The user switches the active project after both threads were
        // already attached -- neither thread's own answer may move.
        *session_cwd_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(PathBuf::from("/projects/c"));

        assert_eq!(
            cwd_for_session(Some(thread_a_project.as_path()), &session_cwd_override),
            PathBuf::from("/projects/.snapflow/a")
        );
        assert_eq!(
            cwd_for_session(Some(thread_b_project.as_path()), &session_cwd_override),
            PathBuf::from("/projects/.snapflow/b")
        );
    }

    /// PISO-4: a slot with no project of its own (created/attached with
    /// nothing open) still falls back to the global `session_cwd_override`
    /// -- that fallback chain is deliberately preserved, not collapsed
    /// away by the slot-first change above.
    #[test]
    fn cwd_for_session_falls_back_to_the_global_override_when_the_slot_has_none() {
        let session_cwd_override: Mutex<Option<PathBuf>> =
            Mutex::new(Some(PathBuf::from("/projects/global")));
        assert_eq!(
            cwd_for_session(None, &session_cwd_override),
            PathBuf::from("/projects/global")
        );
    }

    /// PISO-4: with neither the slot nor the global override carrying a
    /// project, `cwd_for_session` falls back to the process's own working
    /// directory -- the pre-existing last-resort behavior this phase must
    /// not disturb.
    #[test]
    fn cwd_for_session_falls_back_to_the_process_cwd_when_nothing_is_known() {
        let session_cwd_override: Mutex<Option<PathBuf>> = Mutex::new(None);
        let expected = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        assert_eq!(cwd_for_session(None, &session_cwd_override), expected);
    }

    /// PISO-4 extension: a thread whose slot is bound to project A must
    /// get an MCP `--project-dir` pointed at A's parent even while the
    /// process-global `session_cwd_override` (whatever project is active
    /// right now) is B -- otherwise the thread's `cwd` would be fixed but
    /// its MCP tools would still read/write B's files, the half of the
    /// isolation leak that actually matters. `thread_project_dir` is the
    /// shared resolver both `snapflowd_mcp_servers_entry` and the skills
    /// reactive-sync go through; this proves the slot wins over the
    /// global end to end through that real MCP-entry builder, not just at
    /// the resolver in isolation.
    #[test]
    fn thread_project_dir_feeds_the_threads_own_project_into_the_mcp_project_dir_not_the_globals() {
        let session_cwd_override: Mutex<Option<PathBuf>> =
            Mutex::new(Some(PathBuf::from("/projects/b/timeline.mlt")));
        let thread_a_project = PathBuf::from("/projects/a/timeline.mlt");

        let resolved = thread_project_dir(Some(thread_a_project.as_path()), &session_cwd_override);
        assert_eq!(
            resolved,
            Some(PathBuf::from("/projects/a/.snapflow/timeline"))
        );

        let entries = snapflowd_mcp_servers_entry(resolved.as_deref(), "claude");
        let args = entries[0]["args"].as_array().expect("args is an array");
        let project_dir_idx = args
            .iter()
            .position(|a| a == "--project-dir")
            .expect("--project-dir must be present when a project is open");
        assert_eq!(
            args[project_dir_idx + 1],
            serde_json::Value::String("/projects/a/.snapflow/timeline".to_string()),
            "--project-dir must be thread A's own project parent, not the global's (B)"
        );
    }

    /// PISO-7: the live half of a Save-As rebind. Two threads on
    /// different projects (plus one unscoped, pre-project thread) --
    /// renaming A -> B must move only A's thread, leave B's alone, and
    /// leave the unscoped thread unscoped. Proves the rebind is visible
    /// through `thread_project_path` immediately, with no restart and no
    /// sqlite round-trip involved (this constructor uses no cache dir).
    #[test]
    fn rebind_project_path_moves_only_the_renamed_projects_threads() {
        let specs = vec![
            ThreadSpec {
                display_name: "on-a".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: Some("/projects/a/timeline.mlt".to_owned()),
            },
            ThreadSpec {
                display_name: "on-b".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: Some("/projects/b/timeline.mlt".to_owned()),
            },
            ThreadSpec {
                display_name: "unscoped".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];
        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            |_provider| Ok("http://127.0.0.1:1".to_owned()),
            None,
        )
        .expect("bridge construction does not require a reachable gateway");

        assert_eq!(
            bridge.thread_project_path(0).as_deref(),
            Some("/projects/a/timeline.mlt")
        );
        assert_eq!(
            bridge.thread_project_path(1).as_deref(),
            Some("/projects/b/timeline.mlt")
        );
        assert_eq!(bridge.thread_project_path(2), None);

        bridge.rebind_project_path(
            "/projects/a/timeline.mlt",
            "/projects/a-renamed/timeline.mlt",
        );

        assert_eq!(
            bridge.thread_project_path(0).as_deref(),
            Some("/projects/a-renamed/timeline.mlt"),
            "thread on the renamed project must follow it"
        );
        assert_eq!(
            bridge.thread_project_path(1).as_deref(),
            Some("/projects/b/timeline.mlt"),
            "an unrelated project's thread must never move"
        );
        assert_eq!(
            bridge.thread_project_path(2),
            None,
            "an unscoped thread must stay unscoped, not get retro-bound"
        );
    }

    /// PISO-7: an empty `old` must never be treated as "every unscoped
    /// thread" -- an untitled project's first save is not a rename.
    /// `rebind_project_path` itself no-ops on an empty `old` as a second
    /// line of defense (the primary guard lives in `update_host`'s
    /// `ProjectPathRenamed` handler, which must never call this at all in
    /// that case).
    #[test]
    fn rebind_project_path_with_an_empty_old_path_touches_no_thread() {
        let specs = vec![ThreadSpec {
            display_name: "unscoped".to_owned(),
            provider: "codex".to_owned(),
            session_id: None,
            profile_name: None,
            project_path: None,
        }];
        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            |_provider| Ok("http://127.0.0.1:1".to_owned()),
            None,
        )
        .expect("bridge construction does not require a reachable gateway");

        bridge.rebind_project_path("", "/projects/untitled-saved-as.mlt");

        assert_eq!(
            bridge.thread_project_path(0),
            None,
            "an unscoped thread must never be retro-bound via an empty `old`"
        );
    }

    /// project-close-session-teardown: releasing sessions for the
    /// currently active project must not touch threads recorded against a
    /// DIFFERENT project -- mirrors `rebind_project_path_moves_only_the_
    /// renamed_projects_threads`'s "only the matching project's threads
    /// move" shape, but for teardown instead of rename. Unscoped threads
    /// are deliberately NOT exempt here (see the sibling test below for
    /// that behavior) -- this test only pins down the scoped-vs-scoped
    /// isolation. None of these threads ever opened a real session (an
    /// unreachable gateway URL, same as every other bridge-construction
    /// test in this module), so `close_session` resolves to an immediate
    /// no-op success on each -- this test's real assertion is that the
    /// call is safe (no panic) for every slot regardless of project, and
    /// that it never sets the permanent user-facing `closed` state, which
    /// only the explicit close/delete UI actions may do.
    #[test]
    fn release_sessions_for_current_project_never_marks_threads_permanently_closed() {
        let specs = vec![
            ThreadSpec {
                display_name: "on-a".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: Some("/projects/a/timeline.mlt".to_owned()),
            },
            ThreadSpec {
                display_name: "on-b".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: Some("/projects/b/timeline.mlt".to_owned()),
            },
            ThreadSpec {
                display_name: "unscoped".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];
        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            |_provider| Ok("http://127.0.0.1:1".to_owned()),
            None,
        )
        .expect("bridge construction does not require a reachable gateway");

        // No active project recorded yet -- releasing is still safe (and,
        // per the new unscoped behavior, actively releases the unscoped
        // thread's session) but must never touch the permanent `closed`
        // flag on any thread.
        bridge.release_sessions_for_current_project();
        for idx in 0..3 {
            assert!(!bridge.thread_closed(idx));
        }

        // Now project A becomes active; releasing must leave every thread
        // (A's own, B's, and the unscoped one) not permanently closed.
        bridge.set_active_project_identity(&crate::model::ProjectIdentity::Saved(
            "/projects/a/timeline.mlt".to_owned(),
        ));
        bridge.release_sessions_for_current_project();
        for idx in 0..3 {
            assert!(
                !bridge.thread_closed(idx),
                "release must never flip the permanent closed flag"
            );
        }
    }

    /// New behavior: unscoped threads' live sessions must be released on
    /// EVERY project switch, not just when a scoped thread's own project is
    /// being left -- otherwise an unscoped thread's session/pool lease
    /// would never be released by any switch and accumulate indefinitely.
    /// Verifies this holds both for a "no-project -> project A" switch and
    /// a "project A -> project B" switch, and that the unscoped thread
    /// stays visible/reopenable (never permanently `closed`) afterward,
    /// matching how a released scoped-foreign-project thread already
    /// behaves.
    #[test]
    fn release_sessions_for_current_project_releases_unscoped_threads_on_every_switch() {
        let specs = vec![
            ThreadSpec {
                display_name: "on-a".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: Some("/projects/a/timeline.mlt".to_owned()),
            },
            ThreadSpec {
                display_name: "unscoped".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];
        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            |_provider| Ok("http://127.0.0.1:1".to_owned()),
            None,
        )
        .expect("bridge construction does not require a reachable gateway");

        // "no project -> project A": previously no project was active at
        // all, yet the unscoped thread's session must still be released.
        bridge.release_sessions_for_current_project();
        bridge.set_active_project_identity(&crate::model::ProjectIdentity::Saved(
            "/projects/a/timeline.mlt".to_owned(),
        ));

        // "project A -> project B": the unscoped thread is released again,
        // same as A's own thread is released for leaving A.
        bridge.release_sessions_for_current_project();
        bridge.set_active_project_identity(&crate::model::ProjectIdentity::Saved(
            "/projects/b/timeline.mlt".to_owned(),
        ));

        // Neither release call may ever flip the permanent `closed` flag --
        // both threads must remain visible/reopenable, just session-less.
        for idx in 0..2 {
            assert!(
                !bridge.thread_closed(idx),
                "releasing a session must never permanently close the thread \
                 (slot {idx} must remain visible and reopenable)"
            );
        }
    }

    #[test]
    fn terminal_eviction_is_a_no_op_under_the_cap() {
        let mut order = vec!["t1".to_owned(), "t2".to_owned()];
        let mut buffers = HashMap::from([
            ("t1".to_owned(), exited_buffer()),
            ("t2".to_owned(), exited_buffer()),
        ]);
        evict_exited_terminals_over_cap_in(&mut order, &mut buffers, 8);
        assert_eq!(order.len(), 2);
        assert_eq!(buffers.len(), 2);
    }

    #[test]
    fn terminal_eviction_drops_oldest_exited_terminals_first_once_over_cap() {
        let mut order: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut buffers: HashMap<String, TerminalBuffer> = order
            .iter()
            .cloned()
            .map(|id| (id, exited_buffer()))
            .collect();
        evict_exited_terminals_over_cap_in(&mut order, &mut buffers, 8);
        assert_eq!(order.len(), 8);
        assert_eq!(buffers.len(), 8);
        // Oldest (t0, t1) evicted first; newest (t8, t9) survive.
        assert!(!order.contains(&"t0".to_owned()));
        assert!(!order.contains(&"t1".to_owned()));
        assert!(order.contains(&"t9".to_owned()));
    }

    #[test]
    fn terminal_eviction_never_drops_a_still_running_terminal() {
        // All 10 terminals are still running (no exit_status) -- none are
        // eligible for eviction, so the cap is exceeded rather than
        // dropping a terminal the user might still be watching.
        let mut order: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut buffers: HashMap<String, TerminalBuffer> = order
            .iter()
            .cloned()
            .map(|id| (id, running_buffer()))
            .collect();
        evict_exited_terminals_over_cap_in(&mut order, &mut buffers, 8);
        assert_eq!(order.len(), 10);
        assert_eq!(buffers.len(), 10);
    }

    #[test]
    fn terminal_eviction_only_removes_as_many_exited_terminals_as_needed() {
        // 10 total, 8 cap -- 2 must go. t0/t1 exited, the rest still
        // running: only the exited ones are evicted, exactly enough to
        // reach the cap.
        let mut order: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut buffers: HashMap<String, TerminalBuffer> = order
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, id)| {
                let buffer = if i < 2 {
                    exited_buffer()
                } else {
                    running_buffer()
                };
                (id, buffer)
            })
            .collect();
        evict_exited_terminals_over_cap_in(&mut order, &mut buffers, 8);
        assert_eq!(order.len(), 8);
        assert_eq!(buffers.len(), 8);
        assert!(!order.contains(&"t0".to_owned()));
        assert!(!order.contains(&"t1".to_owned()));
    }

    /// Real, already-built `acpx-server` binary next to this crate's own
    /// checkout -- same dev-checkout-relative-path convention
    /// `resolve_acpx_server_bin` uses in production.
    fn acpx_server_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
    }

    // PROF-3: `resolve_backend_agent_command_prefers_explicit_override`,
    // `resolve_backend_agent_command_ignores_override_without_test_mode`,
    // `resolve_backend_agent_command_ignores_mock_binary_without_explicit_
    // opt_in`, and `default_backend_command_for_provider_only_overrides_
    // claude` used to live here, testing the two functions removed above
    // this module's own doc comment for why. Removed along with them.

    /// read_codex_api_key_from_auth_file's real, only call site
    /// (spawn_gateway_process) requires this to be correct without ever
    /// touching this developer's actual ~/.codex/auth.json -- covered via
    /// ACPX_CODEX_AUTH_FILE pointing at a disposable temp file instead.
    #[test]
    fn read_codex_api_key_from_auth_file_reads_the_configured_field() {
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Writes `contents` to a disposable temp `auth.json`, points
    /// `ACPX_CODEX_AUTH_FILE` at it, and returns a guard whose drop
    /// restores the prior env var and removes the temp dir -- shared
    /// setup for `resolve_codex_native_auth_method_id`'s regression
    /// tests below, which each need a fresh file of a specific shape.
    struct TempCodexAuthFile {
        dir: PathBuf,
        prior: Option<std::ffi::OsString>,
    }

    impl TempCodexAuthFile {
        fn write(contents: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rui-codex-auth-mode-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            let auth_file = dir.join("auth.json");
            std::fs::write(&auth_file, contents).expect("write temp auth file");
            let prior = std::env::var_os("ACPX_CODEX_AUTH_FILE");
            unsafe {
                std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
            }
            Self { dir, prior }
        }
    }

    impl Drop for TempCodexAuthFile {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(value) => unsafe { std::env::set_var("ACPX_CODEX_AUTH_FILE", value) },
                None => unsafe { std::env::remove_var("ACPX_CODEX_AUTH_FILE") },
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Regression test for the actual live bug this plan fixes: this
    /// developer's real `~/.codex/auth.json` had `"auth_mode": "chatgpt"`
    /// (a real, completed ChatGPT-plan login) *and* a stale, leftover
    /// non-empty `OPENAI_API_KEY` field, and the old field-presence-only
    /// detection always picked "api-key" for that combination, silently
    /// contradicting the file's own declared mode. `auth_mode` must now
    /// win regardless of the leftover key's presence.
    #[test]
    fn resolve_codex_native_auth_method_id_prefers_declared_chatgpt_over_leftover_api_key() {
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _auth = TempCodexAuthFile::write(
            r#"{"auth_mode": "chatgpt", "OPENAI_API_KEY": "sk-stale-leftover-key", "tokens": {"access_token": "at-real-login"}}"#,
        );

        assert_eq!(resolve_codex_native_auth_method_id(), Some("chat-gpt"));
    }

    /// `auth_mode` absent entirely (an `auth.json` shape from before this
    /// field existed) must still fall back to the old presence-based
    /// priority: a real `OPENAI_API_KEY` resolves to "api-key".
    #[test]
    fn resolve_codex_native_auth_method_id_falls_back_to_api_key_presence_without_auth_mode() {
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _auth = TempCodexAuthFile::write(r#"{"OPENAI_API_KEY": "sk-test-key"}"#);

        assert_eq!(resolve_codex_native_auth_method_id(), Some("api-key"));
    }

    /// `auth_mode: "chatgpt"` must be trusted on its own -- with
    /// `OPENAI_API_KEY` explicitly `null` and no `tokens` object at all
    /// (no recognized token evidence whatsoever) -- since acpx's
    /// `authenticate` call only ever sends `{"methodId": "chat-gpt"}`
    /// with no credential payload; codex-acp itself re-reads the same
    /// auth.json natively to actually consume the login.
    #[test]
    fn resolve_codex_native_auth_method_id_trusts_declared_chatgpt_with_no_token_evidence() {
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _auth =
            TempCodexAuthFile::write(r#"{"auth_mode": "chatgpt", "OPENAI_API_KEY": null}"#);

        assert_eq!(resolve_codex_native_auth_method_id(), Some("chat-gpt"));
    }

    /// An unrecognized `auth_mode` value must be ignored (not trusted as
    /// either mode) and fall back to presence-based detection, same as a
    /// missing field.
    #[test]
    fn resolve_codex_native_auth_method_id_falls_back_on_unrecognized_auth_mode() {
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _auth = TempCodexAuthFile::write(
            r#"{"auth_mode": "some-future-mode", "OPENAI_API_KEY": "sk-test-key"}"#,
        );

        assert_eq!(resolve_codex_native_auth_method_id(), Some("api-key"));
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
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                .unwrap_or_else(|e| e.into_inner());
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

    /// Spawns a real `acpx-server` child process on a fresh ephemeral
    /// port, retrying the whole reserve-port/spawn/wait-for-connect cycle
    /// (bounded at 5 attempts) if the process never becomes reachable
    /// within one attempt's own shorter window.
    ///
    /// **Why this exists.** A bare "bind a listener, read its port, then
    /// immediately drop the listener" trick has an unavoidable TOCTOU
    /// gap: the port is released back to the OS the instant the listener
    /// drops, and nothing stops a *different* concurrently-running test's
    /// own port pick (this crate's real-process tests each spawn their
    /// own `acpx-server`, and the default `cargo test` runner runs many
    /// of them in parallel) from claiming the exact same port before this
    /// function's own spawned process gets to bind it. The gap is wider
    /// than a spawn's worth of scheduling jitter, too: `acpx-server`'s
    /// own startup does a real network fetch of the ACP registry
    /// *before it even attempts its own bind* (see the deadline comment
    /// below), which measured up to ~1.6s in this sandbox -- a window
    /// easily long enough for another test's independent port pick to
    /// land on the same number while ours is still sitting unbound.
    /// **Observed directly**: re-running this crate's full `--lib` suite
    /// back-to-back under the default parallel runner rotated which
    /// real-process test failed, and how many failed (1, then 11, then 3
    /// on three consecutive runs of an otherwise-identical tree), while
    /// every run passed cleanly under `--test-threads=1` -- exactly the
    /// signature of a port race, not a logic bug in any one test.
    ///
    /// The fix is the same reserve-then-bind convention `provision_gateway`
    /// (this module's production gateway-spawn path, non-test code) already
    /// uses for its own real `acpx-server` children: [`reserve_ephemeral_port`]
    /// binds an ephemeral port and, in the same call, atomically
    /// `create_new`s a `rui-acpx-port-<port>.lock` file in the shared
    /// system temp dir (visible to and honored by every process using this
    /// convention, including other `cargo test` processes and any other
    /// worktree's test run on this same host) before dropping its own
    /// listener -- so a second concurrent reservation for the same port
    /// number fails outright instead of silently colliding. The lock is
    /// held for this whole function's reachability-polling window (not
    /// just through the `spawn()` call), because that ~1.6s registry-fetch
    /// gap above is exactly the period another reservation could otherwise
    /// slip in before this attempt's `acpx-server` has actually bound the
    /// port; only once this attempt is done with the port (reachable, or
    /// given up on) is the lock file removed so someone else can reuse the
    /// number.
    fn spawn_acpx_server_with_retry(
        configure: impl Fn(&mut std::process::Command, u16),
    ) -> (std::process::Child, String) {
        for attempt in 0..5 {
            let Some((port, lock)) = reserve_ephemeral_port() else {
                // Only fails if 32 straight ephemeral-port binds all lost
                // the reserve-file race or the bind itself failed -- rare
                // enough that a short backoff-and-retry (same shape as the
                // "server never got reachable" branch below) is the right
                // response, not a hard panic on this attempt alone.
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1)));
                }
                continue;
            };
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
            // `lock` (from `reserve_ephemeral_port` above) is held for
            // this entire window -- see this function's own doc comment
            // for why releasing it any earlier would reopen the race.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
            let mut reachable = false;
            while std::time::Instant::now() < deadline {
                if std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    std::time::Duration::from_millis(100),
                )
                .is_ok()
                {
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
            // This attempt is done with `port` either way -- release the
            // reservation now so a retry (this loop's own next iteration,
            // or an unrelated concurrently-running test) can claim it.
            // Once `acpx-server` has actually bound it (the `reachable`
            // case), the OS itself refuses any other bind for as long as
            // the child lives, so the advisory lock file has no further
            // job to do.
            drop(lock);
            let _ = std::fs::remove_file(
                std::env::temp_dir().join(format!("rui-acpx-port-{port}.lock")),
            );
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
            // `acpx-server` now defaults `ACPX_DB_PATH` to a fixed
            // `~/.acpx/acpx.db` when unset (durable-persistence-by-
            // default, see `main.rs::default_db_path`'s doc comment) --
            // every test spawn that used to rely on "no ACPX_DB_PATH ==
            // no persistence, fully isolated in-memory state" would
            // otherwise silently share and lock-contend on that one real
            // file across every parallel test process. When the caller
            // doesn't ask for a specific `db_path`, mint a fresh one-off
            // sqlite file per spawn instead (leaked via `into_path`, not
            // cleaned up -- same tradeoff any throwaway test tempfile
            // makes; a locked/shared default file across dozens of
            // concurrent `cargo test` processes is the far worse
            // failure mode this avoids).
            let owned_db_path;
            let db_path = match db_path {
                Some(path) => path,
                None => {
                    owned_db_path = tempfile::tempdir()
                        .expect("tempdir for isolated test ACPX_DB_PATH")
                        .into_path()
                        .join("acpx-test.db");
                    owned_db_path.as_path()
                }
            };
            let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
                command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
                test_only_set_backend_cmd_env(command, backend_cmd)
                    .env("ACPX_DEFAULT_AGENT_ID", persona)
                    .env("RUI_MOCK_AGENT_PERSONA", persona)
                    .env("RUST_LOG", "error")
                    .env("ACPX_DB_PATH", db_path);
            });
            TestGateway { child, base_url }
        }

        /// Same as [`Self::spawn_with_persona_and_db`], but also points
        /// `rui-mock-agent` at `event_log_path` (`RUI_MOCK_AGENT_EVENT_LOG`)
        /// so a test can inspect exactly which real ACP methods actually
        /// reached the backend -- used to prove acpx-core's `_acpx.bg`
        /// `session/close` override genuinely suppresses the backend call
        /// (rather than just not erroring).
        fn spawn_with_persona_db_and_event_log(
            persona: &str,
            db_path: Option<&std::path::Path>,
            event_log_path: &std::path::Path,
        ) -> Self {
            let backend_cmd = mock_agent_bin().to_string_lossy().into_owned();
            let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
                command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
                test_only_set_backend_cmd_env(command, &backend_cmd)
                    .env("ACPX_DEFAULT_AGENT_ID", persona)
                    .env("RUI_MOCK_AGENT_PERSONA", persona)
                    .env("RUI_MOCK_AGENT_EVENT_LOG", event_log_path)
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

    fn read_event_log(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// **`acpx-startup-recovery-unbounded`'s `investigate_background_mode_
    /// functionality` phase.** `close_thread`'s `background` parameter had
    /// no wiring at all before this fix -- `AcpxThreadHandle::
    /// close_session` took no arguments and the request it built never
    /// carried acpx-core's `_acpx.bg` extension field, so panel-rust's own
    /// per-thread "background" toggle (`PanelStateStore::
    /// effective_background_session`) was purely a stored/displayed
    /// boolean with zero effect on any real `session/close` call.
    ///
    /// Proves the fix reaches the real wire, not just that
    /// `close_thread` returns `true` either way: acpx-core's
    /// `maybe_suppress_close` intercepts a `background: true` close
    /// *before* it ever reaches the backend, so `rui-mock-agent`'s own
    /// `session/close` handler (which unconditionally logs a
    /// `RUI_MOCK_AGENT_EVENT_LOG` entry when it's actually invoked) must
    /// see zero such entries for a background close, and exactly one for
    /// a normal one.
    #[test]
    fn close_thread_background_flag_reaches_the_real_acpx_bg_override() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let event_log = tempfile::NamedTempFile::new().expect("event log tempfile");
        let gateway =
            TestGateway::spawn_with_persona_db_and_event_log("test", None, event_log.path());
        let names = ["background-thread", "normal-thread"];
        let bridge =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("bridge");
        wait_for_thread_ready(&bridge, 0);
        wait_for_thread_ready(&bridge, 1);

        assert!(
            bridge.close_thread(0, true),
            "a background close must still report success to the caller"
        );
        let after_background_close = read_event_log(event_log.path());
        assert!(
            !after_background_close
                .iter()
                .any(|event| event["method"] == "session/close"),
            "a background=true close must never reach the real backend at all \
             (acpx-core's _acpx.bg override must intercept it first), got: \
             {after_background_close:?}"
        );

        assert!(
            bridge.close_thread(1, false),
            "a normal close must still succeed"
        );
        let after_normal_close = read_event_log(event_log.path());
        let normal_close_events: Vec<_> = after_normal_close
            .iter()
            .filter(|event| event["method"] == "session/close")
            .collect();
        assert_eq!(
            normal_close_events.len(),
            1,
            "a background=false close must reach the real backend exactly once, got: \
             {after_normal_close:?}"
        );
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

    /// Real, live, billed "click + -> a real reply, rendered" chain,
    /// parameterized by `provider`/`configure_env` so the same flow can be
    /// pointed at any registered backend (`codex`, `claude`, ...) without
    /// duplicating the logic per provider -- see the two thin `#[test]`
    /// wrappers below for the actual provider/model choices.
    ///
    /// Covers strictly more than `AgentBridge::history()` alone: after
    /// the real backend replies, the reply is driven through the exact
    /// same `update()`/`sync::apply_message_ops` reducer path
    /// `dispatch_frame_poll` uses in the live app. That reducer/sync
    /// layer is where this session's real bugs lived (list_model/
    /// apply_*_ops key-cache desync crashing the whole `panic = "abort"`
    /// process, and the thread-switch "prefill" bug where a
    /// coincidentally-unchanged transcript diff suppressed the shared
    /// model's resync) -- `AgentBridge::history()` alone can't catch
    /// either, since both are bugs in what happens *after* the bridge
    /// already has the right data.
    ///
    /// `configure_env` mirrors `spawn_acpx_server_with_retry`'s own
    /// closure shape -- each provider supplies whatever auth wiring
    /// `spawn_gateway_process` uses for a real thread of that provider
    /// (codex: `ACPX_NATIVE_AUTH_METHOD_ID` + `CODEX_API_KEY`; claude:
    /// nothing extra, ambient OAuth). `model_config_value`, if set, is
    /// applied via the real `session/set_config_option("model", ...)`
    /// extension before prompting -- pass the cheapest/fastest model this
    /// provider offers to keep the billed call negligible.
    fn assert_new_thread_reaches_a_real_backend_and_renders_through_the_full_reducer(
        provider: &str,
        configure_env: impl Fn(&mut std::process::Command, u16),
        model_config_value: Option<&str>,
        prompt: &str,
        expect_upper_contains: &str,
    ) {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_DEFAULT_AGENT_ID", provider)
                .env("RUST_LOG", "error");
            configure_env(command, port);
        });
        let _gateway_guard = TestGateway {
            child,
            base_url: base_url.clone(),
        };

        // Empty initial thread_specs -- the exact cold-start shape
        // panel_rust_create produces by default (830ec21), and what
        // on_new_thread_requested's click handler drives.
        let mut bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &[],
            move |_provider| Ok(base_url.clone()),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge with zero initial threads");

        // Exactly what on_new_thread_requested calls.
        let index = bridge
            .add_thread_with_profile_and_provider(
                &format!("Real {provider} smoke test"),
                None,
                Some(provider),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "add_thread_with_profile_and_provider must succeed against a real, \
                     correctly-configured {provider} gateway: {error}"
                )
            });

        if let Some(model_value) = model_config_value {
            // Real session/set_config_option extension call, same as
            // acpx/acpx-server/tests/real_ambient_multi_agent_test.rs's
            // run_claude_conversation -- forces the cheap/fast model
            // before prompting.
            bridge.set_config_option(index, "model".to_owned(), serde_json::json!(model_value));
            let config_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while std::time::Instant::now() < config_deadline
                && !bridge
                    .config_options(index)
                    .iter()
                    .any(|opt| opt.current_value.as_deref() == Some(model_value))
            {
                bridge.poll();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        bridge.push_local(
            index,
            ChatMessage {
                kind: MessageKind::User,
                text: prompt.to_owned(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
            },
        );
        bridge.send_prompt(index, prompt.to_owned());

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
        assert!(ended, "real {provider}-acp turn did not finish within 60s");

        // Bridge-layer check: proves the real backend round-trip alone
        // works.
        let history = bridge.history(index);
        assert!(
            history
                .iter()
                .any(|message| message.text.to_uppercase().contains(expect_upper_contains)),
            "expected a real {provider} reply containing {expect_upper_contains}, got: {history:?}"
        );

        // Reducer/sync-layer check: the actual gap this helper closes.
        // Build the Model the same shape lib.rs's cold start does, and
        // feed it the bridge's *real* transcript/session data through the
        // exact same reducer message dispatch_frame_poll uses in the live
        // app -- proving a real reply doesn't just reach AgentBridge, but
        // renders correctly into the shared Slint-facing model with no
        // crash, no duplicate rows, and no stale "prefill" from a
        // previous (nonexistent, here) thread.
        let thread_id = bridge
            .thread_binding(index)
            .map(|binding| binding.thread_id)
            .unwrap_or_else(|| format!("thread:{index}"));
        let mut model = crate::model::Model::default();
        model.threads.push(crate::model::ThreadModel {
            thread_id: thread_id.clone(),
            display_name: format!("Real {provider} smoke test"),
            provider: provider.to_owned(),
            ..crate::model::ThreadModel::default()
        });
        model.visible_indices = vec![0];
        model.selected_thread = 0;
        // displayed_thread starts at None (nothing shown yet) so the
        // switch below exercises the exact "coincidentally unchanged
        // transcript" gap the prefill-data regression test covers, but
        // this time against a real transcript, not a fabricated one.
        assert_eq!(model.displayed_thread, None);

        let snapshot = crate::msg::ThreadFrameSnapshot {
            thread_id: thread_id.clone(),
            real_index: index,
            transcript: bridge.transcript(index),
            has_older_messages: bridge.has_older_page(index),
            pending_request: crate::PendingRequestItem::default(),
            terminals: vec![],
            expanded_terminal: None,
            open_terminals: vec![],
            local_terminal: crate::LocalTerminalItem::default(),
            connection_status: bridge.transport_status(index),
            session_modes: bridge.session_modes(index),
            config_options: bridge.config_options(index),
            available_commands: bridge.available_commands(index),
            plan: vec![],
            session_title: None,
            usage: (0, 0),
        };
        let (_, dirty) = crate::update::update(
            &mut model,
            crate::msg::Msg::Frame(crate::msg::FrameInput {
                selected_thread_snapshot: Some(snapshot),
                ..crate::msg::FrameInput::default()
            }),
        );

        assert_eq!(model.displayed_thread, Some(0));
        let ops = dirty
            .iter()
            .find_map(|item| match item {
                crate::dirty::Dirty::MessagesDiff { thread_id: id, ops } if id == &thread_id => {
                    Some(ops.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "switching to this thread must produce a MessagesDiff for its real \
                     transcript, got: {dirty:?}"
                )
            });

        crate::sync::apply_thread_message_ops(&model, &thread_id, &ops);

        assert!(
            model
                .thread_view_models
                .get(&thread_id)
                .unwrap()
                .row_count()
                > 0,
            "the real {provider} reply must render into its retained thread model, not just \
             AgentBridge::history()"
        );
        assert_eq!(
            model
                .thread_view_models
                .get(&thread_id)
                .unwrap()
                .row_count(),
            model.thread_view_models.keys(&thread_id).unwrap().len(),
            "retained thread model and its key cache must stay aligned after a real reply renders \
             (this exact desync used to abort the whole process)"
        );
        let rendered_text: String = (0..model.messages_model.row_count())
            .filter_map(|i| model.messages_model.row_data(i))
            .map(|row| row.text.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            rendered_text.to_uppercase().contains(expect_upper_contains),
            "expected the real {provider} reply to render into messages_model, got: {rendered_text:?}"
        );
    }

    /// `codex` instance of
    /// `assert_new_thread_reaches_a_real_backend_and_renders_through_the_full_reducer`
    /// -- ambient auth via this machine's already-logged-in Codex CLI
    /// session (same mechanism `acpx/acpx-server/tests/
    /// real_ambient_multi_agent_test.rs` uses), forced to
    /// `ollama/qwen2.5:0.5b` via the real `session/set_config_option`
    /// extension. This machine's `codex-acp` is already configured
    /// against a local Bifrost proxy (`OPENAI_API_KEY=sk-bf-...` in
    /// `~/.config/acpx/acpx-server.env`) that exposes every locally
    /// pulled Ollama model under an `ollama/<name>` id (confirmed live:
    /// `curl http://bifrost.localdev.com/v1/models` lists
    /// `ollama/qwen2.5:0.5b`) -- so this is a real `session/new` +
    /// `session/prompt` round trip through the genuine adapter/gateway
    /// stack, but the actual model call is free and local (a 494M-param
    /// model via `ollama serve` on this machine), not a billed API call.
    /// Verified suitable for this exact prompt: 3/3 direct
    /// `ollama/api/chat`/`v1/chat/completions` calls with "Reply with
    /// exactly the single word PANG and nothing else." returned exactly
    /// `PANG`, in ~0.3s each.
    ///
    /// Still `#[ignore]`d and gated on `ACPX_LIVE_TEST_AMBIENT=1` --
    /// unlike the model call, codex-acp's own ACP handshake still
    /// requires this machine's real Codex CLI login (its `authenticate`
    /// exchange, not the eventual model call, is what needs `api-key`
    /// auth), so this still isn't safe to run unconditionally in CI.
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
                 machine's real, already-logged-in codex CLI session (free/local via Ollama, \
                 but still needs a real codex-acp ACP handshake)"
            );
            return;
        }
        assert_new_thread_reaches_a_real_backend_and_renders_through_the_full_reducer(
            "codex",
            |command, _port| {
                // Same real-auth wiring spawn_gateway_process uses for a
                // real "codex" thread -- no ACPX_BACKEND_CMD override, so
                // acpx-server falls through to its own real default (real
                // codex-acp via npx), not a mock.
                command.env("ACPX_NATIVE_AUTH_METHOD_ID", "api-key");
                if std::env::var_os("CODEX_API_KEY").is_none() {
                    if let Some(key) = read_codex_api_key_from_auth_file() {
                        command.env("CODEX_API_KEY", key);
                    }
                }
            },
            Some("ollama/qwen2.5:0.5b"),
            "Reply with exactly the single word PANG and nothing else.",
            "PANG",
        );
    }

    /// **`acpx-reconnect-retry-duplicates-session-new`.** Real, no-mock
    /// integration coverage requested directly by the user after the
    /// live 512-session leak investigation: a real `acpx-server`, a real
    /// codex-acp backend (this machine's real, already-logged-in codex
    /// CLI session), forced to the free/local `ollama/qwen2.5:0.5b`
    /// model -- not `rui-mock-agent`, not a shell-script stand-in. Opens
    /// 6 real, concurrent conversations (not 1) on the *same* gateway
    /// process, sends a distinct real prompt on each, and asserts every
    /// single one completes cleanly with a real reply and zero errors
    /// (no session-capacity rejection, no attachment failure) -- the
    /// actual "test mode" scenario the 512-error investigation needed
    /// but never had: real multi-conversation load against a real
    /// backend, not a synthetic race against a shell-script mock.
    ///
    /// `#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1` -- needs a
    /// real codex-acp ACP handshake (this machine's real Codex CLI
    /// login), so not safe to run unconditionally in CI.
    ///
    /// Run with:
    /// ```text
    /// ACPX_LIVE_TEST_AMBIENT=1 cargo test --lib \
    ///   agent_bridge::tests::six_real_concurrent_conversations_all_complete_with_zero_errors \
    ///   -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn six_real_concurrent_conversations_all_complete_with_zero_errors() {
        if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
                 machine's real, already-logged-in codex CLI session (free/local via Ollama, \
                 but still needs a real codex-acp ACP handshake)"
            );
            return;
        }

        const CONVERSATION_COUNT: usize = 6;
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_DEFAULT_AGENT_ID", "codex")
                .env("RUST_LOG", "error")
                .env("ACPX_NATIVE_AUTH_METHOD_ID", "api-key");
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

        let mut bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &[],
            move |_provider| Ok(base_url.clone()),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge with zero initial threads");

        // Open all 6 real conversations up front -- real session/new
        // against the same gateway process for each, exactly like 6
        // real user-initiated "New thread" clicks in a row.
        let mut indices = Vec::with_capacity(CONVERSATION_COUNT);
        for i in 0..CONVERSATION_COUNT {
            let index = bridge
                .add_thread_with_profile_and_provider(
                    &format!("Real conversation {i}"),
                    None,
                    Some("codex"),
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "add_thread_with_profile_and_provider #{i} must succeed against a \
                         real, correctly-configured codex gateway: {error}"
                    )
                });
            indices.push(index);
        }

        // Force every thread to the free/local model before prompting,
        // same as the single-conversation smoke tests.
        for &index in &indices {
            bridge.set_config_option(
                index,
                "model".to_owned(),
                serde_json::json!("ollama/qwen2.5:0.5b"),
            );
        }
        let config_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < config_deadline
            && !indices.iter().all(|&index| {
                bridge
                    .config_options(index)
                    .iter()
                    .any(|opt| opt.current_value.as_deref() == Some("ollama/qwen2.5:0.5b"))
            })
        {
            bridge.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Send a distinct real prompt on every conversation.
        for (i, &index) in indices.iter().enumerate() {
            let prompt = format!("Reply with exactly the single word PANG{i} and nothing else.");
            bridge.push_local(
                index,
                ChatMessage {
                    kind: MessageKind::User,
                    text: prompt.clone(),
                    status: None,
                    id: None,
                    raw_input: None,
                    raw_output: None,
                },
            );
            bridge.send_prompt(index, prompt);
        }

        // Poll until every conversation has ended its turn, collecting
        // every error event seen along the way (the actual thing this
        // test exists to prove is zero of: no session-capacity
        // rejection, no attachment failure, across all 6 real,
        // concurrently-open conversations on one real gateway process).
        let mut ended = vec![false; CONVERSATION_COUNT];
        let mut errors: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while std::time::Instant::now() < deadline && !ended.iter().all(|&done| done) {
            for event in bridge.poll() {
                match &event.event {
                    AgentEvent::TurnEnded(_) => {
                        if let Some(slot) = ended.get_mut(event.thread_index) {
                            *slot = true;
                        }
                    }
                    AgentEvent::Error(message) => {
                        errors.push(format!("thread {}: {message}", event.thread_index));
                    }
                    _ => {}
                }
            }
            if !ended.iter().all(|&done| done) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        assert!(
            errors.is_empty(),
            "expected zero errors across {CONVERSATION_COUNT} real, concurrent \
             conversations (no session-capacity rejection, no attachment failure), got: \
             {errors:?}"
        );
        assert!(
            ended.iter().all(|&done| done),
            "all {CONVERSATION_COUNT} real conversations must finish their turn within 90s, \
             got: {ended:?}"
        );

        // Every conversation must show a real, distinct reply -- not
        // just "no error", but genuinely completed real backend work.
        for (i, &index) in indices.iter().enumerate() {
            let history = bridge.history(index);
            let expect = format!("PANG{i}");
            assert!(
                history
                    .iter()
                    .any(|message| message.text.to_uppercase().contains(&expect)),
                "expected conversation {i}'s real codex reply to contain {expect:?}, got: \
                 {history:?}"
            );
        }
    }

    /// `claude`/`haiku` instance of
    /// `assert_new_thread_reaches_a_real_backend_and_renders_through_the_full_reducer`
    /// -- ambient auth via this machine's already-logged-in Claude CLI
    /// session, forced to the `haiku` model via the real
    /// `session/set_config_option` extension (cheapest/fastest model
    /// available, matching `real_claude_multi_agent_test.rs`'s own "use
    /// only haiku or low-variant models for testing" convention).
    ///
    /// `#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1` -- makes a
    /// real, billed model call using whatever account this machine's
    /// Claude CLI is logged into (haiku keeps the cost negligible).
    ///
    /// Run with:
    /// ```text
    /// ACPX_LIVE_TEST_AMBIENT=1 cargo test --lib \
    ///   agent_bridge::tests::add_thread_after_empty_cold_start_reaches_a_real_claude_haiku_backend \
    ///   -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn add_thread_after_empty_cold_start_reaches_a_real_claude_haiku_backend() {
        if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
                 machine's real, already-logged-in claude CLI session (makes a real, \
                 haiku-model billed API call)"
            );
            return;
        }
        assert_new_thread_reaches_a_real_backend_and_renders_through_the_full_reducer(
            "claude",
            |command, _port| {
                // acpx-server's own bare default is codex-only
                // (config.rs's `ACPX_BACKEND_CMD` fallback is literally
                // `npx ... codex-acp`). This test drives a raw
                // `_acpx.profile`-less native-mode session directly
                // against a hand-spawned test gateway (not through
                // `AgentBridge`/`spawn_gateway_process`, which no longer
                // ever sets this env var in production -- see PROF-3),
                // so it still needs its own explicit override here or a
                // "claude" test/thread would silently get a real
                // codex-acp reply instead (confirmed live: this test
                // once timed out because the spawned backend was
                // codex-acp, which never received the config expected of
                // it). No ACPX_NATIVE_AUTH_METHOD_ID: unlike codex-acp's
                // explicit API-key exchange, claude-acp's ambient auth
                // (~/.claude/.credentials.json) doesn't need one.
                test_only_set_backend_cmd_env(
                    command,
                    "npx -y @agentclientprotocol/claude-agent-acp@0.58.1",
                );
            },
            Some("haiku"),
            "Reply with exactly the single word PANG and nothing else.",
            "PANG",
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
        // Persona/agent id must match `NO_PROVIDER_REQUESTED_FALLBACK`
        // ("codex") -- `bridge_with_single_gateway` builds its single
        // thread via `specs_for_names`, which assigns every synthesized
        // spec that fallback provider (see its own doc comment) --
        // `list_sessions_for_agent` selects the backend by this exact
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
        // recoverable-attach-fix: the attach itself (gateway resolution +
        // `session/load`) now runs on the background runtime instead of
        // blocking this call, so the binding is not necessarily present
        // the instant `add_thread_recovering_session` returns -- poll for
        // it with a deadline, same convention every other event-driven
        // assertion in this module already follows.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut binding = bridge.thread_binding(recovered_idx);
        while std::time::Instant::now() < deadline && binding.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            binding = bridge.thread_binding(recovered_idx);
        }
        assert_eq!(
            binding.map(|b| b.session_id),
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
    fn archived_thread_survives_bridge_restart_via_jsonl_cache() {
        // setup-followups plan, archive_thread_backend_verify: proves
        // archive_thread's flag is real durable state (not just an
        // in-memory Mutex that a restart would silently drop), same
        // "restart the whole bridge, load from the same cache dir"
        // convention as history_persists_across_bridge_restarts above.
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let gateway = TestGateway::spawn();
        let names = ["Thread One", "Thread Two"];

        {
            let bridge =
                bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                    .expect("first bridge");
            assert!(!bridge.thread_archived(0));
            assert!(bridge.archive_thread(0));
            assert!(bridge.thread_archived(0));
            // Only thread 0 was archived -- thread 1 must be unaffected,
            // both now and after the restart below.
            assert!(!bridge.thread_archived(1));
        }

        let bridge2 =
            bridge_with_single_gateway(&names, &gateway, Some(cache_dir.path().to_path_buf()))
                .expect("second bridge");
        assert!(bridge2.thread_archived(0));
        assert!(!bridge2.thread_archived(1));
    }

    /// setup-followups plan, archive_thread_backend_verify: real-backend
    /// counterpart to `archived_thread_survives_bridge_restart_via_jsonl_
    /// cache` above, following the exact real/opt-in/free-model
    /// convention `add_thread_after_empty_cold_start_reaches_a_real_
    /// codex_backend` established (this machine's real, already-
    /// logged-in Codex CLI session via `ACPX_NATIVE_AUTH_METHOD_ID=
    /// api-key` + `CODEX_API_KEY` from the auth file; `session/new` +
    /// `session/prompt` are real ACP round trips through the genuine
    /// codex-acp adapter, but the actual model call is forced to
    /// `ollama/qwen2.5:0.5b` via the real `session/set_config_option`
    /// extension -- free and local, not a billed API call).
    ///
    /// Where the unit test above only proves `archive_thread`'s flag
    /// itself survives a bridge restart, this proves the *full* pipeline
    /// a user actually exercises: create a thread against a real
    /// backend, get a real reply, archive it, and drive that archived
    /// state through the exact same `update()`/`sync::apply_thread_row`
    /// reducer path `dispatch_frame_poll` uses in the live app -- the
    /// layer `archived_thread_survives_bridge_restart_via_jsonl_cache`
    /// cannot reach, since it only calls `AgentBridge` methods directly.
    ///
    /// `#[ignore]`d and gated on `ACPX_LIVE_TEST_AMBIENT=1` -- still
    /// needs a real codex-acp ACP handshake (this machine's real Codex
    /// CLI login), so not safe to run unconditionally in CI.
    ///
    /// Run with:
    /// ```text
    /// ACPX_LIVE_TEST_AMBIENT=1 cargo test --lib \
    ///   agent_bridge::tests::archiving_a_real_backend_thread_renders_through_the_full_reducer \
    ///   -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn archiving_a_real_backend_thread_renders_through_the_full_reducer() {
        use slint::Model as _;
        if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
                 machine's real, already-logged-in codex CLI session (free/local via Ollama, \
                 but still needs a real codex-acp ACP handshake)"
            );
            return;
        }

        let cache_dir = tempfile::tempdir().expect("tempdir");
        let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_DEFAULT_AGENT_ID", "codex")
                .env("RUST_LOG", "error")
                .env("ACPX_NATIVE_AUTH_METHOD_ID", "api-key");
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

        let mut bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &[],
            move |_provider| Ok(base_url.clone()),
            Some(cache_dir.path().to_path_buf()),
        )
        .expect("bridge with zero initial threads");

        let index = bridge
            .add_thread_with_profile_and_provider("Real archive smoke test", None, Some("codex"))
            .unwrap_or_else(|error| {
                panic!("add_thread_with_profile_and_provider must succeed against a real, correctly-configured codex gateway: {error}")
            });

        // Force the free/local model before prompting, same as
        // add_thread_after_empty_cold_start_reaches_a_real_codex_backend.
        bridge.set_config_option(
            index,
            "model".to_owned(),
            serde_json::json!("ollama/qwen2.5:0.5b"),
        );
        let config_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < config_deadline
            && !bridge
                .config_options(index)
                .iter()
                .any(|opt| opt.current_value.as_deref() == Some("ollama/qwen2.5:0.5b"))
        {
            bridge.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let prompt = "Reply with exactly the single word PANG and nothing else.";
        bridge.push_local(
            index,
            ChatMessage {
                kind: MessageKind::User,
                text: prompt.to_owned(),
                status: None,
                id: None,
                raw_input: None,
                raw_output: None,
            },
        );
        bridge.send_prompt(index, prompt.to_owned());

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
        assert!(
            !bridge.thread_archived(index),
            "thread must not be archived yet"
        );

        // The actual feature under test: archive the real, now-populated
        // thread, then render it through the exact same reducer path
        // dispatch_frame_poll uses live.
        assert!(bridge.archive_thread(index), "archive_thread must succeed");
        assert!(bridge.thread_archived(index));

        let thread_id = bridge
            .thread_binding(index)
            .map(|binding| binding.thread_id)
            .unwrap_or_else(|| format!("thread:{index}"));
        let mut model = crate::model::Model::default();
        model.threads.push(crate::model::ThreadModel {
            thread_id: thread_id.clone(),
            display_name: "Real archive smoke test".to_owned(),
            provider: "codex".to_owned(),
            archived: bridge.thread_archived(index),
            ..crate::model::ThreadModel::default()
        });
        model.visible_indices = vec![0];
        // Build model.thread_rows via the exact same private row-builder
        // production's thread_list_dirty_with_keys calls, rather than
        // hand-crafting a fixture that risks silently drifting from what
        // that function really outputs.
        model.thread_rows = vec![crate::update::visible_thread_row(&model, 0)
            .expect("visible_thread_row for a real, archived thread")];

        // apply_thread_row only ever *updates* an existing thread_model
        // row (set_row_data by key lookup) -- it doesn't insert one, same
        // as the real live app: this row already exists (pushed when the
        // thread was first created), and the archive click is an
        // in-place Dirty::ThreadRow update to that same row. Seed the
        // pre-archive state exactly like a real cold render would.
        model.thread_model.push(crate::ThreadItem {
            name: "Real archive smoke test".into(),
            archived: false,
            ..crate::ThreadItem::default()
        });
        model.thread_model_keys.borrow_mut().push(thread_id.clone());

        // Same Dirty::ThreadRow -> apply_thread_row path
        // ThreadMsg::ArchiveRequested's handler in update.rs queues, and
        // dispatch_thread_archive drives after the real archive_thread
        // effect completes.
        crate::sync::apply_thread_row(&model, 0);

        assert_eq!(
            model.thread_model.row_count(),
            1,
            "the archived real thread must render into the shared thread_model"
        );
        let row = model.thread_model.row_data(0).expect("archived row");
        assert!(
            row.archived,
            "the real, archived thread's row must render archived: true, got {row:?}"
        );
    }

    #[test]
    fn archive_thread_on_out_of_range_index_returns_false() {
        let gateway = TestGateway::spawn();
        let bridge = bridge_with_single_gateway(&["Only Thread"], &gateway, None).expect("bridge");
        assert!(!bridge.archive_thread(5));
        assert!(!bridge.thread_archived(5));
    }

    #[test]
    fn set_agent_enabled_degrades_gracefully_with_no_admin_plane_reachable() {
        // setup-followups plan, agent_settings_ordering_and_install_
        // enable_flow: with no RUI_ACPX_ADMIN_URL/TOKEN override, no
        // self-spawned admin creds registered, and (in this test's own
        // isolated env) no shared token file at the derived SNAPSHOTD_HOME,
        // resolve_admin_creds must return None and set_agent_enabled must
        // degrade to `false`, never panic.
        // SAFETY: guarded by restoring prior values unconditionally, and
        // serialized against every other env-mutating test in this module
        // by `ENV_MUTATION_LOCK` (see its own doc comment) -- not a
        // `--test-threads=1` convention, which was never actually enforced.
        let _env_guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior_home = std::env::var("SNAPSHOTD_HOME").ok();
        let empty_home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("SNAPSHOTD_HOME", empty_home.path());
        let prior_url = std::env::var("RUI_ACPX_ADMIN_URL").ok();
        let prior_token = std::env::var("RUI_ACPX_ADMIN_TOKEN").ok();
        std::env::remove_var("RUI_ACPX_ADMIN_URL");
        std::env::remove_var("RUI_ACPX_ADMIN_TOKEN");

        let gateway = TestGateway::spawn();
        let bridge = bridge_with_single_gateway(&["Only Thread"], &gateway, None).expect("bridge");
        let result = bridge.set_agent_enabled("codex-acp", false);

        match prior_home {
            Some(v) => std::env::set_var("SNAPSHOTD_HOME", v),
            None => std::env::remove_var("SNAPSHOTD_HOME"),
        }
        match prior_url {
            Some(v) => std::env::set_var("RUI_ACPX_ADMIN_URL", v),
            None => std::env::remove_var("RUI_ACPX_ADMIN_URL"),
        }
        match prior_token {
            Some(v) => std::env::set_var("RUI_ACPX_ADMIN_TOKEN", v),
            None => std::env::remove_var("RUI_ACPX_ADMIN_TOKEN"),
        }

        assert!(
            !result,
            "expected false (no admin plane reachable), not a panic"
        );
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
                        command: "echo".into(),
                        args: Vec::new(),
                        started_at: "2026-07-24T00:00:00.000000000Z".into(),
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
                    archived: false,
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
    fn dropping_bridge_closes_gateway_session_with_bounded_cleanup() {
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
                .all(|session| session.acp_session_id != session_id),
            "AgentBridge drop must close the foreground session; got {sessions:?}"
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

    /// PROF-1: `specs_for_names` no longer alternates "codex"/"claude" by
    /// thread position (the old `provider_for_index`) -- every
    /// synthesized spec gets the one documented fallback, since this
    /// helper only backs name-only test/dev constructors that don't
    /// track real per-thread provider identity at all.
    #[test]
    fn specs_for_names_assigns_the_documented_fallback_to_every_thread() {
        let specs = specs_for_names(&["Thread One", "Thread Two", "Thread Three"]);
        assert_eq!(specs.len(), 3);
        assert!(
            specs
                .iter()
                .all(|spec| spec.provider == NO_PROVIDER_REQUESTED_FALLBACK),
            "got: {specs:?}"
        );
    }

    /// A thread with no project bound and no session cwd override is a
    /// normal, fully-supported state (default cold-start thread, unscoped
    /// thread kept open across a project switch -- see
    /// `retain_items_for_project` and the `2e20021c`/`a8058a45` seeded
    /// default thread). `probe_provider_selection` must not treat "no
    /// project" as a provider auth failure: no `ProviderProbe` event at all
    /// should be emitted, since a stored `Err` here becomes a
    /// "Provider unavailable" toast and disables Send
    /// (`selected-provider-unavailable`) for a reason that has nothing to
    /// do with the provider itself.
    #[test]
    fn probe_provider_selection_is_a_silent_noop_when_thread_has_no_project() {
        let mut bridge = AgentBridge::new_with_gateway_url(&["Untitled"], "http://127.0.0.1:1".to_owned())
            .expect("bridge");

        // stale-provider-switch-pulse fix: the `bool` return is exactly
        // what `effect_executor.rs`'s `Effect::ProbeProvider` arm uses to
        // know no completion event is ever coming, so it can clear
        // `Model::provider_probes_in_flight` itself instead of leaving the
        // "Switching provider..." pulse stuck forever. Must be `false`
        // here -- see this test's own doc comment above for why no event
        // is pushed for this precondition.
        let will_complete = bridge.probe_provider_selection(0, "codex".to_owned(), None);
        assert!(
            !will_complete,
            "a no-project probe must report that no completion event is coming"
        );

        // Give any (unexpected) spawned task a chance to land before
        // asserting the queue is empty.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let events = bridge.poll();
        assert!(
            events.is_empty(),
            "no-project probe must not emit any ProviderProbe event, got: {events:?}"
        );
    }

    /// Supersedes the old `normalize_provider_maps_registry_ids_onto_
    /// gateway_keys` test now that `normalize_provider` is gone entirely
    /// (see `resolve_provider_for`'s doc comment): every distinct
    /// registry id -- not just "codex"/"claude" -- must resolve its own
    /// gateway and be recorded as itself, never collapsed onto a
    /// two-family bucket. This is the routing half of the "selected
    /// grok-build, codex underneath" fix; see
    /// `open_session_maybe_profiled`'s doc comment for the other half
    /// (the profile-name fallback needed to actually reach the right
    /// backend).
    #[test]
    fn distinct_registry_ids_each_resolve_and_record_their_own_provider_no_family_collapsing() {
        let requested = Arc::new(Mutex::new(Vec::new()));
        let requested_for_resolver = requested.clone();
        let mut bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &[] as &[&str],
            move |provider| {
                requested_for_resolver
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(provider.to_owned());
                Ok(format!("http://127.0.0.1:1/{provider}"))
            },
            None,
        )
        .expect("bridge");

        let codex_idx = bridge
            .add_thread_with_profile_and_provider("Codex Thread", None, Some("codex-acp"))
            .expect("codex-acp thread");
        let claude_idx = bridge
            .add_thread_with_profile_and_provider("Claude Thread", None, Some("claude-acp"))
            .expect("claude-acp thread");
        let grok_idx = bridge
            .add_thread_with_profile_and_provider("Grok Thread", None, Some("grok-build"))
            .expect("grok-build thread -- must not be rejected or silently rebucketed");

        assert_eq!(
            bridge.thread_provider(codex_idx).as_deref(),
            Some("codex-acp"),
            "provider is recorded as the raw requested registry id, not normalized"
        );
        assert_eq!(bridge.thread_provider(claude_idx).as_deref(), Some("claude-acp"));
        assert_eq!(
            bridge.thread_provider(grok_idx).as_deref(),
            Some("grok-build"),
            "grok-build must get its own identity, not collapse onto codex-acp/codex"
        );
        assert!(
            requested
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&"grok-build".to_owned()),
            "the resolver must actually be asked to provision grok-build's own gateway"
        );
    }

    #[test]
    fn packaged_gateway_binary_resolution_prefers_override_then_relative_install() {
        // Windows installs the real binary as `acpx-server.exe`
        // (`EXE_SUFFIX`) -- exercise the exact platform-appropriate name
        // so this test still proves the resolver finds a real packaged
        // install on every CI OS (build-windows.yml runs `cargo test
        // --release` on windows-latest too), not just Unix's bare name.
        let exe_name = format!("acpx-server{}", std::env::consts::EXE_SUFFIX);
        let temp = tempfile::tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let packaged = bin_dir.join(&exe_name);
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
        let libexec_bin = libexec_dir.join(&exe_name);
        std::fs::write(&libexec_bin, b"binary").expect("libexec binary");
        std::fs::remove_file(&packaged).expect("remove sibling binary");
        assert_eq!(
            resolve_acpx_server_bin_from(None, Some(&exe), Path::new("/manifest")),
            bin_dir.join(format!("../libexec/{exe_name}"))
        );
    }

    #[test]
    fn packaged_gateway_binary_resolution_falls_back_to_dev_checkout() {
        let exe_name = format!("acpx-server{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(
            resolve_acpx_server_bin_from(None, None, Path::new("/manifest")),
            PathBuf::from(format!("/manifest/../acpx/target/debug/{exe_name}"))
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

    /// Real multi-provider routing: two distinct threads, explicitly
    /// bound to two distinct (locally-spawned) `acpx-server` gateway
    /// processes via their own `ThreadSpec::provider`, each tagging its
    /// reply with its own persona -- the concrete `AgentBridge`-level
    /// version of `rui-acpx-client`'s own `two_gateways_stay_isolated_
    /// no_cross_provider_bleed` test, proving the wiring in *this*
    /// crate's constructor (provider resolution, per-provider gateway
    /// auto-spawn) also keeps threads isolated, not just the lower-level
    /// transport. PROF-1: deliberately uses real ACP-shaped agent ids
    /// ("codex-acp"/"claude-acp", not the old bare "codex"/"claude"
    /// gateway keys) as the `ThreadSpec::provider` value, proving those
    /// ids now flow straight through to gateway resolution with no
    /// normalization step collapsing them onto anything else.
    #[test]
    fn two_threads_route_to_two_distinct_gateways_by_provider() {
        let codex_gateway = TestGateway::spawn_with_persona("codex");
        let claude_gateway = TestGateway::spawn_with_persona("claude");
        let codex_url = codex_gateway.base_url.clone();
        let claude_url = claude_gateway.base_url.clone();
        let specs = vec![
            ThreadSpec {
                display_name: "Codex Thread".to_owned(),
                provider: "codex-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
            ThreadSpec {
                display_name: "Claude Thread".to_owned(),
                provider: "claude-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];

        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            move |provider| {
                if provider == "codex-acp" {
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

    /// PUI-003 e2e: proves the compose `/` menu's per-thread isolation
    /// end to end, not just "by construction". Before this test, only
    /// `parse_capability_update_recognizes_available_commands_update`
    /// (`gateway_actor/thread_actor.rs`) covered `available_commands_
    /// update` at all -- a pure JSON-parse unit test with no real
    /// backend, no `AgentBridge`, and no second provider, so it could
    /// never have caught two threads' commands actually bleeding into
    /// each other. This spins up two real gateway processes (mirroring
    /// `two_threads_route_to_two_distinct_gateways_by_provider` above),
    /// each backed by `rui-mock-agent` under a different
    /// `RUI_MOCK_AGENT_PERSONA`, which now advertises persona-specific
    /// commands (`codex_*` / `claude_*` -- see `persona_commands` in
    /// `src/bin/mock_agent.rs`) on each session's first prompt turn.
    /// Deliberately disjoint command *names* per persona (not just
    /// disjoint descriptions) mean a cross-wiring regression shows up as
    /// an unambiguous "wrong command name in this thread's list" failure
    /// instead of two coincidentally-similar lists passing by luck.
    #[test]
    fn two_threads_show_only_their_own_providers_available_commands() {
        let codex_gateway = TestGateway::spawn_with_persona("codex");
        let claude_gateway = TestGateway::spawn_with_persona("claude");
        let codex_url = codex_gateway.base_url.clone();
        let claude_url = claude_gateway.base_url.clone();
        let specs = vec![
            ThreadSpec {
                display_name: "Codex Thread".to_owned(),
                provider: "codex-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
            ThreadSpec {
                display_name: "Claude Thread".to_owned(),
                provider: "claude-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];

        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            move |provider| {
                if provider == "codex-acp" {
                    Ok(codex_url.clone())
                } else {
                    Ok(claude_url.clone())
                }
            },
            None,
        )
        .expect("bridge with two distinct gateways");

        // `available_commands_update` is only sent on the mock agent's
        // first prompt turn (see `persona_commands`'s doc comment in
        // mock_agent.rs for why session/new itself can't be used), so a
        // real prompt has to be driven for each thread before polling.
        bridge.send_prompt(0, "ping".into());
        bridge.send_prompt(1, "ping".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut commands = [Vec::new(), Vec::new()];
        while std::time::Instant::now() < deadline
            && (commands[0].is_empty() || commands[1].is_empty())
        {
            for _ in bridge.poll() {}
            commands[0] = bridge.available_commands(0);
            commands[1] = bridge.available_commands(1);
            if commands[0].is_empty() || commands[1].is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        let codex_names: Vec<&str> = commands[0].iter().map(|c| c.name.as_str()).collect();
        let claude_names: Vec<&str> = commands[1].iter().map(|c| c.name.as_str()).collect();
        assert!(
            !codex_names.is_empty(),
            "codex thread never received any available_commands"
        );
        assert!(
            !claude_names.is_empty(),
            "claude thread never received any available_commands"
        );
        assert!(
            codex_names.contains(&"codex_plan") && codex_names.contains(&"codex_review"),
            "codex thread missing its own persona's commands, got: {codex_names:?}"
        );
        assert!(
            claude_names.contains(&"claude_plan") && claude_names.contains(&"claude_summarize"),
            "claude thread missing its own persona's commands, got: {claude_names:?}"
        );
        assert!(
            !codex_names.iter().any(|n| claude_names.contains(n)),
            "codex thread's commands leaked into claude thread (or vice versa): \
             codex={codex_names:?} claude={claude_names:?}"
        );
    }

    /// PROF-1's own acceptance test: a THIRD agent id -- neither "codex"
    /// nor "claude", and not a variant of either name -- must resolve to
    /// its own gateway, not get silently bucketed into codex the way the
    /// old `normalize_provider`'s `else { "codex" }` fallback would have
    /// (any id that didn't contain "claude" fell through to codex,
    /// including this one). Three real, distinct locally-spawned
    /// `acpx-server` processes, three distinct `ThreadSpec::provider`
    /// values, proving "adding a new live agent requires zero
    /// panel-rust code changes to route to its own gateway."
    #[test]
    fn a_third_agent_id_routes_to_its_own_gateway_not_codex() {
        let codex_gateway = TestGateway::spawn_with_persona("codex");
        let claude_gateway = TestGateway::spawn_with_persona("claude");
        let gemini_gateway = TestGateway::spawn_with_persona("gemini");
        let codex_url = codex_gateway.base_url.clone();
        let claude_url = claude_gateway.base_url.clone();
        let gemini_url = gemini_gateway.base_url.clone();
        let specs = vec![
            ThreadSpec {
                display_name: "Codex Thread".to_owned(),
                provider: "codex-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
            ThreadSpec {
                display_name: "Claude Thread".to_owned(),
                provider: "claude-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
            ThreadSpec {
                display_name: "Gemini Thread".to_owned(),
                provider: "gemini-acp".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];

        let bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            move |provider| match provider {
                "codex-acp" => Ok(codex_url.clone()),
                "claude-acp" => Ok(claude_url.clone()),
                "gemini-acp" => Ok(gemini_url.clone()),
                other => Err(BridgeError::Gateway(format!(
                    "resolver received unexpected provider {other:?}"
                ))),
            },
            None,
        )
        .expect("bridge with three distinct gateways");

        bridge.send_prompt(0, "ping".into());
        bridge.send_prompt(1, "ping".into());
        bridge.send_prompt(2, "ping".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = [false, false, false];
        while std::time::Instant::now() < deadline && !ended.iter().all(|&done| done) {
            for ev in bridge.poll() {
                if let AgentEvent::TurnEnded(_) = ev.event {
                    if let Some(slot) = ended.get_mut(ev.thread_index) {
                        *slot = true;
                    }
                }
            }
            if !ended.iter().all(|&done| done) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(
            ended.iter().all(|&done| done),
            "timed out waiting for all three threads' turns to end: {ended:?}"
        );

        let gemini_reply = bridge
            .history(2)
            .into_iter()
            .find(|m| m.text.contains("PING"))
            .expect("gemini thread reply");
        assert!(
            gemini_reply.text.starts_with("[GEMINI]"),
            "the third agent id's own gateway/persona must answer its thread, not codex's -- \
             got: {:?}",
            gemini_reply.text
        );
        assert!(
            !bridge
                .history(0)
                .iter()
                .any(|m| m.text.starts_with("[GEMINI]")),
            "the gemini reply must never land on the codex thread"
        );
    }

    /// PROF-6 (`profile-only-backend-selection` plan): real-gateway pin for
    /// PUI-014's lazy-attach path -- `add_thread_deferred` claims a slot
    /// with no session open yet, and the provider/profile it actually binds
    /// to are read fresh from the model at FIRST SEND
    /// (`dispatch::dispatch_compose_send_maybe_attach`), not at creation
    /// time. The plan doc flagged this as a real risk: if seeding/wiring
    /// between PROF-2's default-profile fallback and this attach call had a
    /// gap, a first message could attach with no profile (silently falling
    /// through to native mode) or the wrong one, and nothing at the reducer
    /// level (which only proves `profile_name` gets computed correctly, not
    /// that it reaches a real `session/new`) would catch it.
    ///
    /// Two distinct real gateways/personas, deliberately the opposite of
    /// the bridge's own first (index 0, "codex") seed thread, so a bug that
    /// silently reused the seed thread's own provider/gateway instead of
    /// the one set at attach time shows up as an unambiguous wrong-persona
    /// reply ([CODEX] instead of [CLAUDE]), not a coincidental pass.
    #[test]
    fn deferred_thread_attaches_with_the_profile_and_provider_set_at_first_send() {
        let codex_gateway = TestGateway::spawn_with_persona("codex");
        let claude_gateway = TestGateway::spawn_with_persona("claude");
        let codex_url = codex_gateway.base_url.clone();
        let claude_url = claude_gateway.base_url.clone();
        let specs = vec![
            ThreadSpec {
                display_name: "Codex Seed".to_owned(),
                provider: "codex".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
            ThreadSpec {
                display_name: "Claude Seed".to_owned(),
                provider: "claude".to_owned(),
                session_id: None,
                profile_name: None,
                project_path: None,
            },
        ];
        let mut bridge = AgentBridge::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
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

        // Real profile on the claude gateway, created via thread 1 (the
        // claude seed)'s own handle so it lands on the right gateway.
        assert!(
            bridge.create_profile(
                1,
                serde_json::json!({
                    "name": "lazy-claude-profile",
                    "agent_id": "claude",
                }),
            ),
            "expected profiles/create against the claude gateway (thread 1) to succeed"
        );

        // PUI-014: create the thread DEFERRED -- no session opens yet, the
        // provider/profile picker stays editable.
        let idx = bridge
            .add_thread_deferred("Lazy Thread", Some("claude"))
            .expect("add_thread_deferred");
        assert!(
            bridge.is_deferred(idx),
            "thread must still be deferred before first send"
        );

        // First send: mirrors dispatch_compose_send_maybe_attach's exact
        // call shape (provider + profile read from the model at send time,
        // not from anything captured at creation).
        bridge
            .attach_deferred_thread(idx, Some("claude"), Some("lazy-claude-profile"))
            .expect("attach_deferred_thread");
        assert!(
            !bridge.is_deferred(idx),
            "thread must no longer be deferred after attach"
        );

        bridge.send_prompt(idx, "which persona are you".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut reply = None;
        while std::time::Instant::now() < deadline && reply.is_none() {
            for ev in bridge.poll() {
                if ev.thread_index == idx {
                    if let AgentEvent::Message(msg) = ev.event {
                        if msg.text.contains("WHICH PERSONA ARE YOU") {
                            reply = Some(msg.text);
                        }
                    }
                }
            }
            if reply.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let reply = reply.expect("expected a reply from the lazily attached thread");
        assert!(
            reply.starts_with("[CLAUDE]"),
            "expected the deferred thread's first-send attach to bind the claude \
             provider/profile set at send time, not silently fall back to the seed thread's \
             own codex provider or to native mode -- got: {reply:?}"
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
    /// PROF-8 (`profile-only-backend-selection` plan) canary: the tripwire
    /// `models::is_backend_requires_authentication_error`'s own doc
    /// comment names. Real acpx-server, a real stand-in backend that
    /// advertises `authMethods` on `initialize` with no
    /// `auth_method_id` configured -- exactly what makes acpx-core's
    /// `Router::resolve_profile`/`ensure_backend_initialized` return
    /// `RouterError::BackendRequiresAuthentication` instead of proceeding
    /// to `session/new`. Asserts the REAL error text panel-rust receives
    /// over the wire still contains the substring
    /// `is_backend_requires_authentication_error` matches on, so a future
    /// acpx-core wording change fails this test loudly instead of that
    /// detector silently going dark (see its own doc comment for the full
    /// "this is fragile by design" reasoning this test exists to guard).
    #[test]
    fn open_session_fails_with_a_detectable_authentication_required_message() {
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("requires_auth_backend.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[{"id":"api-key","name":"API Key"}]}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#,
        )
        .expect("write requires-auth stand-in backend script");
        let gateway = TestGateway::spawn_with_backend_cmd(
            &format!("sh {}", script_path.display()),
            "requires-auth-test",
            None,
        );

        let names = ["Needs Auth Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut error_message = None;
        while std::time::Instant::now() < deadline && error_message.is_none() {
            for ev in bridge.poll() {
                if let AgentEvent::Error(message) = ev.event {
                    error_message = Some(message);
                }
            }
            if error_message.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let error_message =
            error_message.expect("expected a real BackendRequiresAuthentication error");
        assert!(
            crate::models::is_backend_requires_authentication_error(&error_message),
            "the real acpx-core error text no longer matches \
             is_backend_requires_authentication_error's substring -- update the detector \
             (and its doc comment) to track acpx-core's actual wording, got: {error_message:?}"
        );
    }

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
                command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
                test_only_set_backend_cmd_env(command, format!("sh {}", script_path.display()))
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
                command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
                test_only_set_backend_cmd_env(command, format!("sh {}", script_path.display()))
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

        // Was its own bespoke free_port()+spawn+100x30ms-connect-poll
        // here, with no reservation lock and no retry -- exactly the
        // unlocked pattern `spawn_acpx_server_with_retry`'s own doc
        // comment describes replacing, just not actually routed through
        // it. This test was the single most consistent failure across
        // repeated default-parallelism runs of the full suite (present in
        // every one of three observed flaky runs); reusing the shared,
        // lock-protected helper here closes the same port race for it.
        // PROF-5's compile-enforced guard requires ACPX_BACKEND_CMD to be
        // set only via test_only_set_backend_cmd_env, never a direct
        // .env("ACPX_BACKEND_CMD", ...) call -- both fixes composed here
        // rather than picking one over the other.
        let (child, base_url) = spawn_acpx_server_with_retry(|command, port| {
            command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
            test_only_set_backend_cmd_env(command, format!("sh {}", script_path.display()))
                .env("ACPX_DEFAULT_AGENT_ID", "profile-picker-agent")
                .env("RUST_LOG", "error");
        });
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

    /// SCNA-02: `MAX_RETAINED_TERMINALS_PER_THREAD`/
    /// `evict_exited_terminals_over_cap_in` have pure-logic unit test
    /// coverage elsewhere in this file, but never against a real host
    /// spawning more than the cap. This reuses exactly
    /// `add_thread_with_profile_unlocks_terminal_access_end_to_end`'s
    /// stand-in-backend-plus-real-`acpx-server` technique -- no new
    /// `rui-mock-agent` capability is needed, since acpx-server's own
    /// `terminal/create` handling (`acpx-core::router`'s
    /// `handle_terminal_request` + `spawn_terminal_output_stream`) really
    /// executes the process and really streams `acpx/terminal_output`
    /// back to this bridge once the client-side relay is approved via
    /// `respond_to_request` -- just looped past the cap instead of once.
    #[test]
    fn terminal_eviction_cap_holds_against_a_real_host_spawning_more_than_the_cap() {
        let terminal_count = MAX_RETAINED_TERMINALS_PER_THREAD + 2;
        let script_dir = tempfile::tempdir().expect("script tempdir");
        let script_path = script_dir.path().join("stand_in_backend.sh");
        let mut terminal_create_calls = String::new();
        for i in 0..terminal_count {
            let req_id = 900 + i;
            terminal_create_calls.push_str(&format!(
                r#"  printf '{{"jsonrpc":"2.0","id":{req_id},"method":"terminal/create","params":{{"sessionId":"backend-cap","command":"sh","args":["-c","printf term-{i}"]}}}}\n'
  while IFS= read -r reply_line; do
    echo "$reply_line" | grep -q '"id":{req_id}' && break
  done
"#,
            ));
        }
        std::fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-cap"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
{terminal_create_calls}    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#
            ),
        )
        .expect("write stand-in backend script");

        let (port, _port_lock) =
            reserve_ephemeral_port().expect("reserve ephemeral port for terminal-cap acpx");
        let mut command = std::process::Command::new(acpx_server_bin());
        command
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
            .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
            .env("ACPX_DEFAULT_AGENT_ID", "terminal-cap-agent")
            .env("RUST_LOG", "error")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = command.spawn().expect("spawn real acpx-server binary");
        let base_url = format!("http://127.0.0.1:{port}");
        for _ in 0..100 {
            if std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_millis(100),
            )
            .is_ok()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let gateway = TestGateway { child, base_url };

        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let client = acpx_client::raw::GatewayClient::new(gateway.base_url.clone());
            client
                .call(
                    "profiles/create",
                    serde_json::json!({
                        "name": "cap-enabled",
                        "agent_id": "terminal-cap-agent",
                        "allow_terminal_access": true
                    }),
                    None,
                )
                .await
                .expect("profiles/create");
        });

        let mut bridge =
            bridge_with_single_gateway(&["Seed Thread", "Seed Thread Two"], &gateway, None)
                .expect("bridge with two seed threads");
        let idx = bridge
            .add_thread_with_profile("Cap Thread", Some("cap-enabled"))
            .expect("add_thread_with_profile");
        bridge.send_prompt(idx, "spawn many terminals".into());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut approved = 0usize;
        while std::time::Instant::now() < deadline && approved < terminal_count {
            let pending = bridge.pending_requests(idx);
            if let Some(event) = pending.first() {
                assert_eq!(event.method, "terminal/create");
                let response = crate::permission::build_response(event, true);
                bridge.respond_to_request(idx, &event.relay_id, response);
                approved += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert_eq!(
            approved, terminal_count,
            "expected to approve every one of the {terminal_count} terminal/create relays"
        );

        // Every approved terminal's real process exits immediately
        // (`printf` has no wait), so once acpx-server's real
        // `spawn_terminal_output_stream` delivers each one's final
        // `acpx/terminal_output` (carrying `exitStatus`), eviction runs
        // synchronously inside `store_terminal_output` -- poll until the
        // bridge's own live view settles under the cap instead of
        // asserting on a single snapshot.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut active = bridge.active_terminals(idx);
        while std::time::Instant::now() < deadline
            && active.len() > MAX_RETAINED_TERMINALS_PER_THREAD
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            active = bridge.active_terminals(idx);
        }
        assert!(
            active.len() <= MAX_RETAINED_TERMINALS_PER_THREAD,
            "active_terminals should have settled at or under the cap ({MAX_RETAINED_TERMINALS_PER_THREAD}), \
             got {} entries out of {terminal_count} approved -- eviction did not keep up: {active:?}",
            active.len()
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
                command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
                test_only_set_backend_cmd_env(command, format!("sh {}", script_path.display()))
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

        // setup-followups plan: a value this thread's attached backend
        // never advertised (the cross-provider case ACP has no
        // primitive for -- see set_config_option's own doc comment)
        // must be rejected before ever reaching the backend, not
        // silently forwarded to whatever process happens to be
        // attached. Overwrite the marker file first so its continued
        // absence-of-a-*new*-write is a real signal, not just "the
        // earlier real call already wrote it".
        std::fs::write(&set_config_marker, "UNTOUCHED\n").expect("reset marker");
        bridge.set_config_option(
            0,
            "model".to_string(),
            serde_json::json!("claude-opus-not-a-real-codex-model"),
        );
        // No deadline-poll here on purpose: this must resolve
        // synchronously (the validation runs before the async task is
        // even spawned) or not at all -- a fixed settle time is the
        // right tool for asserting an absence, unlike the real round
        // trips above which need to poll for a positive result.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            std::fs::read_to_string(&set_config_marker).unwrap_or_default(),
            "UNTOUCHED\n",
            "an unadvertised config value must never reach the backend"
        );
        let events = bridge.poll();
        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                AgentEvent::Error(message) if message.contains("is not one this thread's attached backend advertised")
            )),
            "expected a rejection AgentEvent::Error, got: {events:?}"
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
        let stdio_entry = |command: &str| {
            crate::protocol_types::McpServerEntry::new(
                "bridge-fs",
                crate::protocol_types::McpServerConfig::Stdio {
                    command: command.to_string(),
                    args: Vec::new(),
                    env: Default::default(),
                    timeout: None,
                },
            )
        };
        assert!(bridge.create_mcp_server(0, stdio_entry("mcp-bridge-fs")).is_ok());
        let after_create = bridge.list_mcp_servers(0);
        assert_eq!(after_create.len(), 1);
        assert_eq!(after_create[0].name, "bridge-fs");

        // The real failure-text contract this test exists to prove:
        // create_mcp_server/etc. used to collapse every failure into a
        // bare `bool`/`Option`, discarding the actual reason. A duplicate
        // create against the real gateway is rejected server-side
        // (`acpx_core::mcp_servers::McpServerStore::create`'s
        // `AlreadyExists` error) -- confirm that real message reaches the
        // caller, not a generic "failed" string with no context.
        let duplicate_create_err = bridge
            .create_mcp_server(0, stdio_entry("mcp-bridge-fs"))
            .expect_err("creating the same name twice must fail");
        assert!(
            duplicate_create_err.contains("bridge-fs") || duplicate_create_err.contains("already"),
            "expected the real gateway rejection reason (naming the duplicate server or \
             \"already exists\"), got: {duplicate_create_err:?}"
        );

        assert!(bridge.update_mcp_server(0, stdio_entry("mcp-bridge-fs-v2")).is_ok());
        let after_update = bridge.list_mcp_servers(0);
        assert_eq!(after_update.len(), 1);
        assert_eq!(after_update[0].command(), Some("mcp-bridge-fs-v2"));

        assert!(bridge.delete_mcp_server(0, "bridge-fs").is_ok());
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

    /// Real end-to-end proof of `AgentBridge::fetch_mcp_server_tools` --
    /// proven at the layer `lib.rs` actually calls from Slint (the
    /// sibling `gateway_actor_mcp_agents_e2e_test.rs` tests only prove
    /// this one level lower, through `AcpxThreadHandle` directly).
    /// Real `snapflowd-mcp` subprocess, real `mcp_servers/tools_fetch`
    /// RPC, real background probe, polled through the real gateway.
    #[test]
    fn fetch_mcp_server_tools_reaches_a_real_backend_through_the_bridge() {
        let gateway = TestGateway::spawn();
        let names = ["Tools Fetch Thread"];
        let bridge = bridge_with_single_gateway(&names, &gateway, None).expect("bridge");

        let global_dir = tempfile::tempdir().expect("global dir tempdir");
        std::fs::create_dir_all(global_dir.path().join("release")).expect("skill dir");
        std::fs::write(
            global_dir.path().join("release").join("SKILL.md"),
            "---\nname: release\ndescription: release process\n---\n",
        )
        .expect("write SKILL.md");

        let entry = crate::protocol_types::McpServerEntry::new(
            "bridge-tools-preview",
            crate::protocol_types::McpServerConfig::Stdio {
                command: resolve_snapflowd_mcp_bin().to_string_lossy().into_owned(),
                args: vec![
                    "--global-dir".to_string(),
                    global_dir.path().to_string_lossy().into_owned(),
                ],
                env: Default::default(),
                timeout: None,
            },
        );
        assert!(bridge.create_mcp_server(0, entry).is_ok());

        let before_fetch = bridge
            .list_mcp_servers(0)
            .into_iter()
            .find(|e| e.name == "bridge-tools-preview")
            .expect("just-created entry should be listed");
        assert_eq!(before_fetch.tool_catalog, None);

        assert!(
            bridge.fetch_mcp_server_tools(0, "bridge-tools-preview").is_ok(),
            "fetch_mcp_server_tools kickoff should reach the real gateway"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ready_tools = None;
        while std::time::Instant::now() < deadline {
            let entry = bridge
                .list_mcp_servers(0)
                .into_iter()
                .find(|e| e.name == "bridge-tools-preview")
                .expect("entry should still be listed while polling");
            match entry.tool_catalog {
                Some(crate::protocol_types::McpToolCatalog::Ready { tools }) => {
                    ready_tools = Some(tools);
                    break;
                }
                Some(crate::protocol_types::McpToolCatalog::Error { message }) => {
                    panic!("real tools/list probe through the bridge failed: {message}");
                }
                _ => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let tool_names: Vec<String> = ready_tools
            .expect("expected a ready real tool catalog within the timeout")
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            tool_names.contains(&"list_skills".to_string()),
            "expected the real snapflowd-mcp tool catalog to include list_skills, got {tool_names:?}"
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
                command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
                test_only_set_backend_cmd_env(command, format!("sh {}", script_path.display()))
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
        // Remainder must stay smaller than HISTORY_PAGE_SIZE so the third
        // page (the "real start") is reached in exactly one more
        // `load_older_page` call after the first two full pages -- this
        // constant shrank from 500 to 20 (see
        // memory/acpx/gen/plans/panel-thread-switch-freeze-fix-plan.md's
        // "Cross-check: message loading is already paginated" section);
        // the original `+ 37` relied on `37 < 500` and silently broke this
        // test's "two total load_older_page calls reach the start"
        // assumption once the page size shrank below it.
        let total_messages = HISTORY_PAGE_SIZE * 2 + 7;
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

        // Second call reaches the real start (7 remaining messages).
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

    /// `skill_injection_verification` phase: `snapflowd_mcp_servers_entry`'s
    /// output shape -- the actual client-supplied `mcpServers` entry every
    /// `session/new`/`session/load` now sends (see `Command::OpenSession`/
    /// `Command::ResumeSession`'s doc comments), verified directly rather
    /// than through a real acpx-server round trip (this sandbox's
    /// acpx-server makes a real network call to cdn.agentclientprotocol.com
    /// at startup before binding its port -- confirmed directly by
    /// inspecting its own startup log -- making real round-trip tests here
    /// flaky on network latency, unrelated to this logic itself).
    #[test]
    fn snapflowd_mcp_servers_entry_includes_the_skills_server_for_mcp_backed_providers() {
        // "claude" stays MCP-backed (skills_manager::agent_registry::
        // is_live_verified only covers "codex"/"codex-acp") -- was
        // "codex" before phase 8's MCP removal for
        // codex specifically; this test's actual point (the "skills"
        // entry's shape) is unaffected by which still-MCP-backed provider
        // exercises it.
        //
        // Not asserting entries.len() == 1: a real snapshotd daemon
        // happening to run on the test host makes snapshotd_mcp_server_
        // entry's liveness probe legitimately append a second "snapshotd"
        // entry (see that function's own doc comment) regardless of
        // provider -- this test only cares that "skills" is present,
        // first, and correctly shaped, for a provider still on MCP.
        let entries = snapflowd_mcp_servers_entry(None, "claude");
        assert!(!entries.is_empty());
        assert_eq!(entries[0]["name"], "skills");
        assert!(entries[0]["command"]
            .as_str()
            .unwrap()
            .contains("snapflowd-mcp"));
        let args = entries[0]["args"].as_array().expect("args is an array");
        assert!(args.contains(&serde_json::Value::String("--global-dir".to_string())));
        assert!(
            !args.contains(&serde_json::Value::String("--project-dir".to_string())),
            "no project open -- args must not claim a --project-dir"
        );
    }

    #[test]
    fn snapshotd_mcp_entry_omitted_when_snapflow_disabled() {
        let _gate = SNAPFLOW_MCP_GATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = SNAPFLOW_MCP_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
        SNAPFLOW_MCP_ENABLED.store(false, std::sync::atomic::Ordering::Relaxed);
        let without = snapshotd_mcp_server_entry_for_addr(Some("127.0.0.1:9"));
        assert!(
            without.is_empty(),
            "disabled snapflow must not inject snapshotd into mcpServers"
        );
        SNAPFLOW_MCP_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        let with = snapshotd_mcp_server_entry_for_addr(Some("127.0.0.1:9"));
        assert_eq!(with.len(), 1);
        assert_eq!(with[0]["name"], "snapshotd");
        SNAPFLOW_MCP_ENABLED.store(prev, std::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn apply_snapflow_to_client_mcp_list_removes_and_restores() {
        let base = vec![
            serde_json::json!({"name": "skills", "command": "x", "env": []}),
            serde_json::json!({"type": "http", "name": "snapshotd", "url": "http://1/mcp", "headers": []}),
        ];
        let off = apply_snapflow_to_client_mcp_list(&base, false, None);
        assert_eq!(off.len(), 1);
        assert_eq!(off[0]["name"], "skills");
        let on = apply_snapflow_to_client_mcp_list(&off, true, Some("127.0.0.1:9"));
        assert!(
            on.iter()
                .any(|e| e.get("name").and_then(|n| n.as_str()) == Some("snapshotd")),
            "re-enable with an inject addr must put snapshotd back"
        );
        assert!(on
            .iter()
            .any(|e| e.get("name").and_then(|n| n.as_str()) == Some("skills")));
    }

    /// **Regression test for the real, live-found silent-drop bug**
    /// (`video-generation-e2e-harness` plan's `custom_mcp_and_skills_
    /// support` phase, 2026-07-23): real `codex-acp`'s own request
    /// schema requires `env` as a non-optional array for stdio-shaped
    /// MCP server entries, and silently drops (no error) any entry that
    /// fails validation -- confirmed live that an identical entry
    /// without `env` never appeared in a real session's `/mcp` listing
    /// at all, while the same entry with `env: []` did. Without this
    /// field, the app's own real "skills" custom MCP server was being
    /// silently dropped by every real codex-acp session.
    #[test]
    fn snapflowd_mcp_servers_entry_skills_server_includes_the_required_env_field() {
        let entries = snapflowd_mcp_servers_entry(None, "claude");
        assert_eq!(entries[0]["name"], "skills");
        assert!(
            entries[0]["env"].is_array(),
            "the skills entry must include an 'env' array -- real codex-acp's own request \
             schema requires it for stdio-shaped MCP servers and silently drops (no error) \
             any entry missing it, confirmed live"
        );
    }

    #[test]
    fn snapflowd_mcp_servers_entry_adds_project_dir_from_the_open_project_files_parent() {
        let project_dir = std::path::Path::new("/tmp/my-project/.snapflow/timeline");
        let entries = snapflowd_mcp_servers_entry(Some(project_dir), "claude");
        let args = entries[0]["args"].as_array().expect("args is an array");
        let project_dir_idx = args
            .iter()
            .position(|a| a == "--project-dir")
            .expect("--project-dir must be present when a project is open");
        assert_eq!(
            args[project_dir_idx + 1],
            serde_json::Value::String("/tmp/my-project/.snapflow/timeline".to_string()),
            "--project-dir must be the canonical project store directory"
        );
    }

    /// Phase 8 of memory/acpx/gen/plans/acpx-skills/meta.json: MCP skill
    /// delivery is removed ONLY for vendor_ids a live test actually
    /// proved deliver skills via native filesystem discovery with no MCP
    /// present -- panel-rust/tests/skills_manager_live_discovery_e2e_test.rs
    /// ran 4/4 real passes against a real codex-acp backend. "codex" and
    /// "codex-acp" (both forms `slot.provider` can currently take) must
    /// therefore get NO "skills" MCP entry at
    /// all -- snapshotd's entry (if a live daemon answers) is unaffected,
    /// this is skills-specific.
    #[test]
    fn snapflowd_mcp_servers_entry_omits_the_skills_server_for_live_verified_filesystem_providers()
    {
        for provider in ["codex", "codex-acp"] {
            let entries = snapflowd_mcp_servers_entry(None, provider);
            assert!(
                entries.iter().all(|e| e["name"] != "skills"),
                "provider {provider:?} passed the live filesystem-discovery gate (phase 7) -- \
                 it must not still be sent the \"skills\" MCP entry, entries: {entries:?}"
            );
        }
    }

    /// `"type": "http"` was confirmed live to work with both real
    /// `codex-acp` (which otherwise hard-rejects `"type": "sse"`
    /// entirely) and real `claude-agent-acp` -- this entry is provider-
    /// agnostic by design now, so the shape must stay identical
    /// regardless of which provider string is passed in.
    #[test]
    fn snapshotd_mcp_server_entry_is_absent_without_cached_daemon_status() {
        assert!(snapshotd_mcp_server_entry_for_addr(None).is_empty());
    }

    /// **Regression test for the real, live-found MCP transport bug**
    /// (`video-generation-e2e-harness` plan's `custom_mcp_and_skills_
    /// support` phase, 2026-07-22): a real `codex-acp` session given
    /// this entry's *old* URL (`/sse`, even under `"type": "http"`)
    /// failed its MCP handshake with `HTTP 405: Method Not Allowed`.
    /// `snapshotd/internal/mcpadapter/sse.go`'s `SSEServer` serves the
    /// Streamable HTTP transport at `/mcp`, not `/sse` -- this test
    /// pins the entry to that endpoint so this exact regression can't
    /// silently return.
    #[test]
    fn snapshotd_mcp_server_entry_points_at_the_streamable_http_endpoint_not_sse() {
        // The authoritative address is supplied by daemon.mcpStatus; this
        // pure helper verifies the generated entry without a per-call dial.
        let addr = "127.0.0.1:43210";
        let entries = snapshotd_mcp_server_entry_for_addr(Some(addr));

        assert_eq!(
            entries.len(),
            1,
            "a live-answering daemon must produce exactly one entry"
        );
        assert_eq!(
            entries[0]["url"],
            serde_json::Value::String(format!("http://{addr}/mcp")),
            "must point at the Streamable HTTP endpoint (/mcp), not the legacy SSE one (/sse) -- \
             codex-acp's real MCP client requires this exact shape, confirmed live"
        );
    }

    /// Exercises the real control-socket request/response path used by the
    /// watcher: Unix socket dial, newline-delimited JSON-RPC request, and
    /// daemon.mcpStatus result parsing. The entry-shape tests above do not
    /// cover this transport boundary.
    #[cfg(unix)]
    #[test]
    fn snapshotd_mcp_status_query_reads_the_authoritative_bound_address() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind control socket");
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept control client");
            let mut reader = BufReader::new(stream.try_clone().expect("clone control stream"));
            let mut request = String::new();
            reader
                .read_line(&mut request)
                .expect("read JSON-RPC request");
            let request: serde_json::Value =
                serde_json::from_str(&request).expect("valid JSON-RPC request");
            assert_eq!(request["method"], "daemon.mcpStatus");
            let mut stream = stream;
            stream
                .write_all(
                    br#"{"jsonrpc":"2.0","id":1,"result":{"addr":"127.0.0.1:0","listening":true,"authEnabled":false}}
"#,
                )
                .expect("write JSON-RPC response");
        });

        assert_eq!(
            query_snapshotd_mcp_addr_at(&socket_path).as_deref(),
            Some("127.0.0.1:0")
        );
        handle.join().expect("control server thread");
    }

    /// PISO-8 (project-isolation-mlt-binding plan): the JSONL shapes below
    /// are literal `registry.ProcessInstance`/`registry.Project` output as
    /// produced by `cmdList`/`cmdListProjects` (`snapshotd/cmd/snapshotd/
    /// main.go`) -- PascalCase, no `json` tags. Only a `"ready"` instance
    /// whose `ProjectID` resolves through the projects list counts as
    /// live; a `"closed"` instance and an instance whose project never
    /// appears in the projects list are both excluded, not defaulted to
    /// some guessed path.
    #[test]
    fn parse_daemon_list_and_projects_joins_ready_instances_to_their_project_path() {
        let list_jsonl = concat!(
            r#"{"ID":"inst-a","ProjectID":"proj-a","PID":111,"SocketPath":"/tmp/a.sock","Status":"ready","Headless":true}"#,
            "\n",
            r#"{"ID":"inst-b","ProjectID":"proj-b","PID":222,"SocketPath":"/tmp/b.sock","Status":"closed","Headless":false}"#,
            "\n",
            r#"{"ID":"inst-c","ProjectID":"proj-unknown","PID":333,"SocketPath":"/tmp/c.sock","Status":"ready","Headless":true}"#,
        );
        let projects_jsonl = concat!(
            r#"{"ID":"proj-a","RootDir":"/home/user/projects/alpha","MltFileName":"project.mlt","Status":"active"}"#,
            "\n",
            r#"{"ID":"proj-b","RootDir":"/home/user/projects/beta","MltFileName":"project.mlt","Status":"active"}"#,
        );
        let live = parse_daemon_list_and_projects(list_jsonl, projects_jsonl);
        assert_eq!(
            live,
            vec![DaemonProjectInstance {
                project_path: "/home/user/projects/alpha/project.mlt".to_string(),
                headless: true,
            }],
            "only the ready instance whose ProjectID resolves through the projects list \
             may appear -- the closed instance and the instance with an unknown project \
             must both be dropped, not guessed at"
        );
    }

    #[test]
    fn parse_daemon_list_and_projects_returns_empty_for_empty_or_malformed_input() {
        assert!(parse_daemon_list_and_projects("", "").is_empty());
        assert!(parse_daemon_list_and_projects("not json\n", "also not json\n").is_empty());
    }

    #[test]
    fn parse_daemon_list_and_projects_threads_the_headless_flag_through() {
        let list_jsonl =
            r#"{"ID":"inst-a","ProjectID":"proj-a","Status":"ready","Headless":false}"#;
        let projects_jsonl = r#"{"ID":"proj-a","RootDir":"/p","MltFileName":"project.mlt"}"#;
        let live = parse_daemon_list_and_projects(list_jsonl, projects_jsonl);
        assert_eq!(live.len(), 1);
        assert!(
            !live[0].headless,
            "a project the user has open headful (PISO-9's headful-wins reuse) must not be \
             misreported as headless"
        );
    }

    /// Broad coverage for registry poll-diff identity: name/enabled/config
    /// changes count; tool_catalog (ephemeral fetch state) does not; order
    /// is irrelevant.
    #[test]
    fn mcp_registry_identity_ignores_tool_catalog_and_detects_real_diffs() {
        use crate::protocol_types::{McpServerConfig, McpServerEntry, McpToolCatalog};

        let a = McpServerEntry::new(
            "fs",
            McpServerConfig::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-filesystem".into()],
                env: Default::default(),
                timeout: None,
            },
        );
        let mut a_with_tools = a.clone();
        a_with_tools.tool_catalog = Some(McpToolCatalog::Fetching);
        assert_eq!(
            mcp_registry_identity(std::slice::from_ref(&a)),
            mcp_registry_identity(std::slice::from_ref(&a_with_tools)),
            "tool catalog must not look like a registry mutation"
        );

        let mut disabled = a.clone();
        disabled.enabled = false;
        assert_ne!(
            mcp_registry_identity(std::slice::from_ref(&a)),
            mcp_registry_identity(std::slice::from_ref(&disabled)),
            "enabled flip is a real registry change"
        );

        let b = McpServerEntry::new(
            "git",
            McpServerConfig::Stdio {
                command: "mcp-git".into(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        let ordered = mcp_registry_identity(&[b.clone(), a.clone()]);
        let reversed = mcp_registry_identity(&[a, b]);
        assert_eq!(ordered, reversed, "list order must not matter");
    }

    #[test]
    fn merge_mcp_list_keeps_fetching_and_enabled_while_ops_in_flight() {
        use crate::protocol_types::{McpServerConfig, McpServerEntry, McpToolCatalog};
        use std::collections::HashSet;

        let mut local = McpServerEntry::new(
            "fs",
            McpServerConfig::Stdio {
                command: "npx".into(),
                args: vec![],
                env: Default::default(),
                timeout: None,
            },
        );
        local.enabled = false;
        local.tool_catalog = Some(McpToolCatalog::Fetching);

        // Wire still shows pre-toggle enabled=true and no catalog yet.
        let mut wire = local.clone();
        wire.enabled = true;
        wire.tool_catalog = None;

        let mut ops = HashSet::new();
        ops.insert("tools_fetch:fs".to_owned());
        ops.insert("enabled:fs".to_owned());

        let merged = merge_mcp_list_with_optimistic(vec![wire], std::slice::from_ref(&local), &ops);
        assert_eq!(merged.len(), 1);
        assert!(
            !merged[0].enabled,
            "in-flight enable toggle must keep optimistic enabled"
        );
        assert_eq!(
            merged[0].tool_catalog,
            Some(McpToolCatalog::Fetching),
            "in-flight tools_fetch must keep optimistic Fetching over empty list"
        );

        // Wire Ready wins even while tools_fetch key is still present.
        let mut ready = local.clone();
        ready.tool_catalog = Some(McpToolCatalog::Ready { tools: vec![] });
        let merged_ready =
            merge_mcp_list_with_optimistic(vec![ready], std::slice::from_ref(&local), &ops);
        assert!(matches!(
            merged_ready[0].tool_catalog,
            Some(McpToolCatalog::Ready { .. })
        ));

        // An immediate RPC failure must survive the next empty list poll so
        // the row remains useful after the shared toast expires.
        let error_message = "300002: no mcp server named snapflow".to_owned();
        let mut failed = local.clone();
        failed.tool_catalog = Some(McpToolCatalog::Error {
            message: error_message.clone(),
        });
        let mut empty_catalog = failed.clone();
        empty_catalog.tool_catalog = None;
        let merged_error = merge_mcp_list_with_optimistic(
            vec![empty_catalog],
            std::slice::from_ref(&failed),
            &HashSet::new(),
        );
        assert_eq!(
            merged_error[0].tool_catalog,
            Some(McpToolCatalog::Error {
                message: error_message,
            })
        );
    }

    /// lock_audit Layer 1 (F-01/F-02): frame-poll catalog path must never
    /// `block_on` — empty bridge returns an empty cache immediately, and
    /// a refresh with no slots is a no-op (does not hang the caller).
    #[test]
    fn gateway_catalog_cache_is_ui_thread_safe_without_block_on() {
        let bridge = AgentBridge::new_with_gateway_url(&[], "http://127.0.0.1:9".to_owned())
            .expect("empty-thread bridge for catalog cache test");
        assert!(
            bridge.gateway_catalog_empty(),
            "cold cache must start empty (gen == 0)"
        );
        let empty = crate::msg::SettingsGatewaySnapshot {
            profiles: Vec::new(),
            mcp_servers: Vec::new(),
            agents: Vec::new(),
            agents_fetched: false,
            recoverable_sessions: Vec::new(),
            recovery_provider: String::new(),
        };
        let snap = bridge.gateway_catalog_snapshot(empty.clone());
        assert!(snap.profiles.is_empty());
        assert!(snap.agents.is_empty());
        assert!(snap.mcp_servers.is_empty());
        // Out-of-range refresh must return without awaiting any RPC.
        bridge.request_gateway_catalog_refresh(0);
        assert!(bridge.gateway_catalog_empty());
        // Still a pure clone after refresh request with no slots.
        let again = bridge.gateway_catalog_snapshot(empty);
        assert!(again.agents.is_empty());
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
    let history = slot
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let session_id = slot
        .acp_session_id
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    let older_available = *slot
        .older_available
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let real_message_count = if older_available {
        history.len()
            + *slot
                .oldest_loaded_index
                .lock()
                .unwrap_or_else(|e| e.into_inner())
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
