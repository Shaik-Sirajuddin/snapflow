//! Method classification (gateway-native vs. proxied vs. hybrid) per
//! `02-architecture.md`'s classification table. Phase 1 only needs
//! classification for the single-agent passthrough set; profile
//! resolution, MCP-server merge, and gateway-native handlers land in
//! Phase 2/3.

use crate::keystore::Keystore;
use crate::lifecycle::LifecycleConfig;
use crate::mcp_servers::McpServerStore;
use crate::notify::NotificationHub;
use crate::agent_relay::AgentRequestHub;
use crate::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    Direction, PersistenceStore,
};
use crate::profile::{PermissionPolicy, Profile, ProfileStore};
use crate::provider::ProviderStore;
use crate::session_registry::{BackendSessionId, SessionRegistry, TenantId};
use crate::{AgentEnablement, CustomAgentStore, InteractionHub, DEFAULT_INTERACTION_TIMEOUT};
use acpx_proto::agent::{AgentSource, AgentStatus};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Which bucket a given JSON-RPC method falls into. See the classification
/// table in `02-architecture.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodClass {
    /// Handled entirely in-process; no backend agent involved.
    GatewayNative,
    /// Session-resolve + forward, payload untouched.
    Proxied,
    /// One-time gateway logic (profile/agent resolution + spawn), then
    /// delegates to the backend.
    Hybrid,
    /// **ACP compatibility gap closed post-review.** `session/fork` is a
    /// real v1 ACP method (`ForkSessionRequest`/`ForkSessionResponse`,
    /// `x-side: agent`) that was entirely unclassified before this fix --
    /// found by cross-checking every method the real
    /// `agent-client-protocol` 1.2.0 crate's own `impl_jsonrpc_request!`
    /// macro invocations define
    /// (`src/schema/client_to_agent/requests.rs`) against `classify`'s
    /// match arms; every other one was covered, this one fell straight
    /// through to `MethodClass::Unknown`.
    ///
    /// **Not yet stabilized upstream** -- gated behind
    /// `agent-client-protocol-schema`'s own `unstable_session_fork`
    /// Cargo feature (see this workspace's root `Cargo.toml`, which now
    /// enables it), i.e. the ACP project itself still considers this an
    /// opt-in draft extension, not a baseline-spec method every agent
    /// must implement. acpx supports it anyway because a real backend
    /// can and does advertise fork support (`claude-agent-acp` 0.58.1's
    /// own `session/new` response includes `sessionCapabilities.fork:
    /// {}}`, verified against the real npx-installed adapter), so any
    /// client that checked that capability before calling `session/fork`
    /// would get a spurious "unknown method" from acpx even though the
    /// backend it's actually talking to genuinely supports it.
    ///
    /// Neither `Proxied` (its response mints a *new* session id, unlike
    /// every other proxied method) nor `Hybrid` (it forwards against an
    /// *existing* session's already-running backend process, not a
    /// freshly resolved profile/spawn like `session/new`) fits, so this
    /// is its own bucket: resolve the *source* session's agent/backend
    /// (like `Proxied`), forward with the gateway `sessionId` rewritten
    /// to the backend-native one (like `Proxied`), then register the
    /// backend's newly-minted forked session id under a *new* gateway
    /// session id and rewrite the response the same way `session/new`
    /// does. See `Router::dispatch_session_fork`/
    /// `dispatch_session_fork_shared`.
    SessionFork,
    /// Not a recognized ACP or acpx method.
    Unknown,
}

/// Classify a JSON-RPC method name. Pure function, no state -- routing
/// state (session registry, profile store, conductor) lives in `Router`.
pub fn classify(method: &str) -> MethodClass {
    match method {
        // **Phase 6 addition -- closes a real, previously-undiscovered
        // gap:** a spec-compliant ACP client always sends `initialize`
        // as its very first call over the wire, before anything else
        // (per agentclientprotocol.com's own documented handshake
        // flow). Every dispatch path and every test in this workspace
        // before this phase only ever implemented/exercised the
        // *backend*-facing side of `initialize`/`authenticate` (acpx
        // calling out to whatever process a profile spawns -- see
        // `ensure_backend_initialized`); acpx's own client-facing
        // endpoint never classified either method at all, so it fell
        // through to `MethodClass::Unknown` and any real ACP editor/IDE
        // connecting to acpx would get an immediate `UnknownMethod`
        // error on its first ever request, before `session/new` was
        // ever reached. See `dispatch_native`'s `"initialize"`/
        // `"authenticate"` arms for what acpx answers now.
        // **Phase 9 addition:** `logout` is a real, stable v1 ACP method
        // (`agentclientprotocol.com`'s schema: `x-side: agent`, no
        // `sessionId` -- it's connection-scoped, not session-scoped) that
        // was entirely unclassified before this phase, same category as
        // phase 6's pre-fix `initialize`/`authenticate` gap: it fell
        // through to `MethodClass::Unknown` and any client that first
        // checked `agentCapabilities.auth.logout` (correctly, since
        // that's how the spec says to gate calling it at all) would
        // never even try -- but a client that called it anyway got a
        // generic `UnknownMethod` rather than a clear "not supported"
        // answer. Routed `GatewayNative` (not `Proxied`) because it has
        // no `sessionId` to resolve a specific backend from -- in
        // acpx's multi-backend gateway there is no single unambiguous
        // backend a bare `logout` could target, unlike a real
        // single-agent ACP agent where there's exactly one connection.
        // See `dispatch_native`'s `"logout"` arm for what acpx answers.
        "initialize" | "authenticate" | "logout" => MethodClass::GatewayNative,
        "session/new" => MethodClass::Hybrid,
        "session/prompt" | "session/resume" | "session/load" | "session/close"
        | "session/set_mode" | "session/cancel"
        // `session/set_config_option`: not one of the plan's originally
        // enumerated ACP methods, but a real, published extension method
        // used by `@agentclientprotocol/claude-agent-acp` (and, per the
        // ACP ecosystem's `configOptions` pattern surfaced on every
        // `session/new` response, likely other adapters too) for
        // in-session model selection -- verified against the real
        // published adapter, see `acpx/COVERAGE.md`'s "real multi-agent
        // concurrency" section for how this was discovered. Session-scoped
        // (carries `sessionId`, forwarded byte-for-byte like every other
        // proxied method) so it fits this bucket exactly; omitting it
        // meant a real client had no way to ever pick a non-default model
        // for a claude-agent-acp-backed profile through the gateway.
        | "session/set_config_option"
        // **Phase 9 addition:** `session/delete` -- real, stable v1 ACP
        // method (`DeleteSessionRequest`/`DeleteSessionResponse`, `x-side:
        // agent`, carries `sessionId`) found entirely unclassified during
        // this phase's schema recheck (fetched the real `schema/v1/
        // schema.json` off `agentclientprotocol/agent-client-protocol`
        // directly rather than trusting secondary summaries, after phase
        // 8's recheck flagged conflicting claims about its stability).
        // `claude-agent-acp`'s own compiled `dist/acp-agent.js` implements
        // `deleteSession` for real (confirmed by reading it in this
        // phase), so this was a genuine, exercisable gap, not a
        // theoretical one. Session-scoped like `session/close`, so it
        // fits `Proxied` exactly, and shares `rehydrate_session`'s
        // restart-survival fallback with `session/load`/`session/resume`
        // below -- deleting a session a client knows about from a
        // *previous* acpx process lifetime is exactly as legitimate a use
        // case as loading/resuming one.
        | "session/delete" => MethodClass::Proxied,
        // See `MethodClass::SessionFork`'s doc comment for why this is
        // neither `Proxied` nor `Hybrid`.
        "session/fork" => MethodClass::SessionFork,
        "agents/list" | "agents/install" | "agents/status" | "session/list" => {
            MethodClass::GatewayNative
        }
        "profiles/create" | "profiles/list" | "profiles/update" | "profiles/delete" => {
            MethodClass::GatewayNative
        }
        "mcp_servers/create" | "mcp_servers/list" | "mcp_servers/update" | "mcp_servers/delete" => {
            MethodClass::GatewayNative
        }
        // `retention_administration` (`acpx-session-lifecycle`). Not a
        // real ACP method -- an acpx-only extension namespace, same
        // category as `profiles/*`/`mcp_servers/*` above. Tenant-scoped
        // (see `dispatch_native`'s arms): each resolves/mutates only a
        // session already owned by the authenticated tenant issuing the
        // request, via `SessionRegistry`'s existing tenant-nested map --
        // exactly the same ownership check `Router::set_session_pinned`
        // already enforced as an in-process-only seam before this;
        // these give it a real, authenticated, gateway-native JSON-RPC
        // surface.
        "session/retention/get"
        | "session/retention/list"
        | "session/retention/pin"
        | "session/retention/unpin"
        | "session/retention/set_ttl" => MethodClass::GatewayNative,
        _ => MethodClass::Unknown,
    }
}

/// **Phase 13 addition.** Which specific backend a dual-mode
/// `session/list` call should be proxied to, per the `_acpx` extension
/// convention already established by `session/new`'s `_acpx.profile`.
/// `Profile` resolves through `Router::resolve_profile` exactly like
/// `session/new`'s managed mode; `AgentId` names an already-registered
/// supervisor key directly (most usefully `default_agent_id`, for
/// native/unmanaged mode, which has no profile at all to name).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionListSelector {
    Profile(String),
    AgentId(String),
}

/// Extracts a [`SessionListSelector`] from a `session/list` call's
/// `params`, if any -- `None` means "no backend selector given," which
/// is what routes the call to acpx's own gateway-scoped aggregate view
/// instead of a real per-backend proxy (see `dispatch_native`'s
/// `"session/list"` arm and `dispatch_shared`'s matching guard, which
/// both call this to decide).
fn session_list_selector(params: &serde_json::Value) -> Option<SessionListSelector> {
    let ext = params.get("_acpx")?;
    if let Some(name) = ext.get("profile").and_then(|p| p.as_str()) {
        return Some(SessionListSelector::Profile(name.to_string()));
    }
    if let Some(id) = ext.get("agentId").and_then(|p| p.as_str()) {
        return Some(SessionListSelector::AgentId(id.to_string()));
    }
    None
}

/// Phase 1 stub: the real `Router` composes `SessionRegistry` +
/// `acpx-conductor::Supervisor` + (from Phase 3) `ProfileStore` to actually
/// dispatch. Left unimplemented here; `acpx-server`'s Phase 1 spike talks to
/// `acpx-conductor` directly for its single hardcoded backend instead of
/// going through this type, per `04-phased-plan.md` step 4's "validates the
/// framing/spawn/proxy plumbing in isolation before adding gateway
/// complexity" note.
///
/// Phase 2 step 9: this is now the real multi-agent router. It owns an
/// `acpx_conductor::Supervisor` (spawn/reuse backend processes) and a
/// `SessionRegistry` (gateway <-> backend session id mapping), and
/// dispatches each JSON-RPC request per the classification table above:
/// gateway-native methods are answered in-process, proxied methods are
/// session-resolved and forwarded byte-for-byte (only the `sessionId`
/// field is rewritten in place), and `session/new` is hybrid (resolve
/// agent -> ensure process running -> forward -> register the returned
/// backend session id under a fresh gateway session id).
///
/// **Formerly a "Known Phase 2 gap", now closed** (was tracked in
/// `05-open-risks.md`'s "Reverse-direction (agent-initiated) messages"
/// item): agent-initiated messages that arrive on a backend's stdout
/// without a matching request id (overwhelmingly `session/update`
/// notifications -- confirmed against a real published adapter, not just
/// a hypothetical) are no longer dropped. `read_matching_response`
/// collects them and every proxied/hybrid dispatch path folds them into
/// the JSON-RPC response envelope's `_acpx.updates` array -- see
/// `read_matching_response`'s doc comment for the full rationale and
/// `acpx/COVERAGE.md`'s "real ACP content delivery" section for why this
/// mattered: without it, a client talking to a real streaming-style
/// adapter through acpx got a `session/prompt` result with no actual
/// reply text in it, ever.
pub struct Router {
    supervisor: acpx_conductor::Supervisor,
    sessions: SessionRegistry,
    /// Native gateway-wide limits, checked before `session/new` consumes
    /// a connector/backend session.
    lifecycle: LifecycleConfig,
    /// Live and in-flight session admissions. This is independent of the
    /// router lock so permits can safely span backend I/O without leaking
    /// capacity when their owning task is cancelled.
    admission: Arc<Mutex<AdmissionState>>,
    /// Fallback agent id used when `session/new` carries no `_acpx.profile`
    /// (native/unmanaged mode, see `02-architecture.md`) -- Phase 3 adds
    /// real profile -> agent resolution; until then every session goes to
    /// this one configured agent.
    default_agent_id: String,
    /// Explicit backend authentication method for native/unmanaged sessions.
    /// Unset preserves the safe default: ACPX never guesses a backend auth
    /// method when a caller did not select a managed profile.
    native_auth_method_id: Option<String>,
    /// HTTP client for `acpx-registry`'s live registry fetch. Reused across
    /// calls rather than constructed per-request.
    http: reqwest::Client,
    /// Lazily-populated cache of the last successful registry fetch (live
    /// or fallback) -- `agents/list`/`agents/status`/`agents/install` all
    /// refresh it via `ensure_registry_loaded` rather than re-fetching on
    /// every call. No TTL/invalidation yet; a later phase can add one if
    /// the registry needs to be re-polled for changes mid-run.
    registry_cache: Option<acpx_registry::Registry>,
    /// Cached results from disposable backend capability probes. Unlike the
    /// registry cache this stores adapter-runtime data (`configOptions`,
    /// permission modes, and auth methods), not launch metadata.
    capability_cache: acpx_registry::CapabilityCache,
    /// Optional sqlite-backed persistence (Phase 2 step 10, see
    /// `crate::persistence`). `None` by default -- a `Router` used purely
    /// in-memory (e.g. most of this crate's own tests) never touches
    /// sqlite at all. When set via [`Router::with_persistence`], session
    /// metadata and transcripts are written fire-and-forget via
    /// `tokio::spawn` off the dispatch hot path, per that module's
    /// "written asynchronously" design goal -- a slow/failed persistence
    /// write never delays or fails the client's actual request.
    persistence: Option<PersistenceStore>,
    /// Durable admin-plane read stores. They remain absent for
    /// in-memory-only routers, preserving the legacy default behavior.
    agent_enablement: Option<AgentEnablement>,
    custom_agents: Option<CustomAgentStore>,
    /// Custom definitions that have been materialized into the supervisor.
    /// Keeping this local lets a long-lived router reject a deleted custom
    /// id instead of falling back to its stale supervisor specification.
    materialized_custom_agents: HashSet<String>,
    /// Phase 3 stores: provider config, secret material, and
    /// {agent, provider, key-ref, launch overrides, mcp servers} profiles.
    /// All in-memory only (see `crate::provider`/`crate::profile`'s doc
    /// comments for why -- not persisted to the sqlite `persistence` path
    /// used for sessions/transcripts). `session/new`'s `_acpx.profile`
    /// resolves against `profiles`, which in turn references `providers`
    /// and `keystore`.
    providers: ProviderStore,
    keystore: Keystore,
    profiles: ProfileStore,
    /// **`durable_secret_and_configuration_store`.** Set only via
    /// [`Router::enable_durable_config`] (never by [`Router::new`] or
    /// [`Router::with_persistence`] alone) -- encrypts every secret
    /// `Keystore::store`s from that point on before it reaches
    /// `persistence`. `None` keeps `keystore` exactly as in-memory-only
    /// as it always was, so a `Router` that never opts in behaves
    /// byte-for-byte like before this field existed.
    secret_keyring: Option<Arc<Mutex<crate::keystore::MasterKeyring>>>,
    /// Where [`Self::rotate_master_key`] writes the keyring back to disk
    /// after minting a new version. Set alongside `secret_keyring`.
    keyring_path: Option<std::path::PathBuf>,
    /// Centrally-registered MCP servers (Phase 3 step 17a), merged by
    /// name into a resolved profile's `mcpServers` at `session/new` --
    /// client entries always win on collision, see
    /// `crate::mcp_servers::merge_mcp_servers`.
    mcp_servers: McpServerStore,
    /// **Phase 14 addition.** Live `session/update` fan-out to whichever
    /// persistent transport connection (stdio/WS) currently owns a given
    /// gateway session -- see `crate::notify`'s module doc comment for
    /// the full rationale. Cheaply cloneable (an `Arc` internally), so
    /// [`Self::notification_hub`] hands a clone straight to a transport
    /// without that transport ever needing to come back through this
    /// `Router`'s own lock to subscribe/publish.
    notification_hub: NotificationHub,
    /// **Interactive relay addition.** Live agent-initiated request
    /// relay (`session/request_permission` today; see
    /// `crate::agent_relay`'s module doc comment) to whichever transport
    /// connection currently owns a given gateway session. Same
    /// cheaply-cloneable-handle convention as `notification_hub` right
    /// above; kept as a fully separate hub rather than folded into
    /// `NotificationHub` because it's bidirectional (request-out,
    /// reply-in) where `NotificationHub` is publish-only.
    agent_request_hub: AgentRequestHub,
    /// Correlates backend-initiated requests with responses from the
    /// persistent ACP client that currently owns the session.
    interaction_hub: InteractionHub,
    /// **`process_reader_demux` HTTP-fallback buffer.** Undelivered
    /// `session/update` notifications the demux consumer
    /// ([`spawn_demux_consumer`]) observed for a gateway session with no
    /// live WS/stdio subscriber right now -- queued here instead of
    /// discarded, keyed by `(tenant_id, gateway_session_id)`, so the
    /// *next* `POST /rpc` call against that same session can drain and
    /// attach them to its own response's `_acpx.updates`, exactly the
    /// data a live-subscribed transport gets pushed immediately and an
    /// HTTP-only caller has no other way to ever see (`transport::http`
    /// has no live-push channel at all). Closes the real "demux silently
    /// zeroes out `_acpx.updates` for every `POST /rpc` caller" data-loss
    /// gap `process_reader_demux`'s field doc comment used to flag as
    /// the reason the flag's default stayed off -- see
    /// [`PendingUpdates`]'s own doc comment for the drain-on-read/
    /// bounded-size contract.
    pending_updates: PendingUpdates,
    /// **Phase 15 addition.** Identity (`Arc::as_ptr` cast to `usize`) of
    /// every physical backend process instance that already has an idle
    /// scavenger task (see [`spawn_idle_scavenger`]/[`backend_idle_
    /// scavenger`]) running for it. Keyed by pointer identity rather than
    /// `agent_id` on purpose: a crash+respawn hands back a brand new
    /// `SharedBackendProcess` (a fresh `Arc`, per `Supervisor::
    /// ensure_running`'s doc comment), which naturally yields a fresh,
    /// not-yet-present key here, so a respawned process always gets its
    /// own fresh scavenger rather than either leaking the crashed
    /// process's now-pointless task forever or requiring any explicit
    /// crash-detection bookkeeping of its own -- the stale task simply
    /// notices its process has exited (`BackendProcess::has_exited`) and
    /// returns on its own next tick, see that function's doc comment.
    scavenged_backends: HashSet<usize>,
    /// Opt-in isolation for managed profile processes. Disabled by default
    /// so existing deployments continue sharing one process per profile.
    tenant_process_isolation: bool,
    /// Opt-in *session*-level isolation for managed profile processes,
    /// orthogonal to (and composable with) `tenant_process_isolation`
    /// (`backend_process_model` hardening item, `acp-gateway-daemon`
    /// plan). Disabled by default -- every managed profile still shares
    /// one process per (profile[, tenant]) key across every session
    /// using it, unchanged. When enabled, every *new* managed session
    /// gets its own dedicated backend process (keyed by folding that
    /// session's own gateway id into the supervisor key -- see
    /// `dispatch_session_new`'s use of this flag), trading process count
    /// for full request/response isolation instead of two concurrent
    /// sessions on one profile serializing behind that profile's single
    /// process mutex. Native/unmanaged sessions (no `_acpx.profile`) are
    /// unaffected regardless of this setting -- see `dispatch_session_new`.
    session_process_isolation: bool,
    /// **`ACPX_STARTUP_SESSION_RECOVERY_ENABLED` gate, threaded through
    /// to on-demand rehydration too.** Defaults `true` (matches every
    /// pre-existing deployment/test that never sets the env var, and
    /// matches `rehydrate_session`'s original always-on behavior before
    /// this flag existed). When an operator explicitly disables startup
    /// session recovery, that must mean "recovery is off," full stop --
    /// not just "don't proactively batch-recover at boot, but still
    /// silently resurrect any persisted session the instant any client
    /// happens to touch its gateway id." `rehydrate_session` checks this
    /// before even querying the persistence store.
    on_demand_recovery_enabled: bool,
    /// **`process_reader_demux`, phase 1 of
    /// `memory/acpx/gen/acpx-concurrency-config-execution.meta.json`.**
    /// **On by default** (`ACPX_PROCESS_READER_DEMUX=0` opts out). When
    /// enabled, all four backend
    /// round-trip dispatch paths that can share a process --
    /// [`dispatch_proxied_shared`], [`dispatch_session_new_shared`],
    /// [`dispatch_session_fork_shared`], and
    /// [`dispatch_session_list_real_shared`] -- register-then-await a
    /// response via
    /// `BackendProcess::pending` instead of holding the per-process lock
    /// across the entire write + blocking-read-loop of a turn -- see
    /// `memory/acpx/tasks/zed_integration.yaml` task 7. This is what lets
    /// two sessions sharing one backend process actually overlap in wall
    /// time instead of fully serializing behind each other's whole turn.
    /// The legacy, non-`_shared` `Router::dispatch*` methods (used only
    /// by direct, non-multi-tenant callers/tests, not any production
    /// transport -- see `dispatch_shared`'s own doc comment) are still
    /// unaffected either way; every production dispatch path is covered.
    /// **All four must agree on this flag together for one process**:
    /// once any one of them calls `BackendProcess::start_demux` (taking
    /// the raw reader), every *other* call sharing that same process
    /// must also route through the pending table instead of
    /// `reader_mut()` -- calling `reader_mut()` after demux has started
    /// panics outright (`BackendProcess::reader_mut`'s own doc comment).
    /// This field is read once per call and threaded through, so a
    /// config change only takes effect for backend processes spawned
    /// after the change, never retroactively for an already-demuxed one.
    ///
    /// **Why it is now safe to default on** (previously deferred for
    /// three separate, real regressions -- all three are now closed,
    /// each with its own regression test):
    ///
    /// 1. **`session/fork`/`session/list` crash** once any other call on
    ///    the same shared process had already activated demux (`proc.
    ///    reader` taken, `reader_mut()` panics). Fixed: both now
    ///    register-then-await like every other dispatch path.
    /// 2. **`InteractionHub`/`AgentRequestHub` relay and live terminal
    ///    streaming silently stopped working** for every session on a
    ///    demuxed process. Root cause: [`spawn_demux_consumer`]'s single
    ///    per-*process* `LiveNotifyCtx` has no single session's
    ///    `tenant_id`/`gateway_session_id` to carry (it serves every
    ///    session sharing that process), so it always built one with
    ///    both `None` -- and [`try_forward_interaction`] (the path
    ///    Zed's `/acp` bridge relies on for `session/request_permission`
    ///    etc.) and [`try_relay_agent_request`] both treated `None` as
    ///    "give up," not "resolve it per-frame" the way
    ///    [`try_deliver_live`] already did. Fixed: all three (plus the
    ///    `terminal/create` live-stream spawn) now resolve the gateway
    ///    session from each frame's own `params.sessionId` via
    ///    [`resolve_gateway_session`], same as `try_deliver_live` always
    ///    did. Before this fix, a live Zed session behind a shared,
    ///    demuxed backend process would never be asked to approve a
    ///    permission request -- it silently fell straight through to
    ///    the profile's static auto-answer instead.
    /// 3. **`POST /rpc` data loss**: `transport::http` has no live-push
    ///    channel at all (see that module's doc comment), so its only
    ///    way to see a call's interleaved notifications is the legacy
    ///    path's inline `_acpx.updates` buffering
    ///    (`UnmatchedOutcome::Notification` -> `attach_updates`) --
    ///    which the demux consumer bypasses entirely (see tradeoff
    ///    below). Fixed: [`Router::pending_updates`] buffers whatever
    ///    the demux consumer couldn't deliver live, per gateway session,
    ///    and [`dispatch_proxied_shared`]'s demux branch drains it into
    ///    its own response's `_acpx.updates` before returning, so a
    ///    `POST /rpc` caller still sees the same data it always did --
    ///    just via a bounded (`MAX_BUFFERED_UPDATES_PER_SESSION`)
    ///    best-effort buffer instead of blocking inline delivery.
    ///
    /// **Known tradeoff while enabled**: unmatched frames (bare
    /// notifications and agent-initiated requests) are handled entirely
    /// by an independent per-process consumer task
    /// ([`spawn_demux_consumer`]) rather than by whichever call happened
    /// to be in the read loop. A response's own `_acpx.updates`/
    /// `_acpx.agentRequests` are populated from `PendingUpdates`
    /// (`session/update` only, notification-delivery-order preserved,
    /// FIFO) rather than gathered inline during the read loop --
    /// functionally equivalent for a well-behaved caller, but the
    /// ordering relative to *this exact call's own response frame* is
    /// no longer strictly guaranteed the way a single shared read loop
    /// naturally guaranteed it (a notification that raced the response
    /// frame itself could end up buffered for the *next* call instead of
    /// this one). `_acpx.agentRequests` is not buffered at all --
    /// agent-initiated requests always get *some* answer immediately
    /// (relay if live, policy auto-answer otherwise), so there is
    /// nothing left to buffer for a later call to attach.
    process_reader_demux: bool,
    /// **`connector_reference_lifecycle`.** Supervisor keys currently
    /// observed to have zero referencing live sessions, mapped to when
    /// that was first observed. Only populated/consulted when
    /// `LifecycleConfig::connector_idle_shutdown_ttl` is `Some` --
    /// `mark_unreferenced_if_idle`/`reap_unreferenced_backends` are the
    /// only readers/writers. A key is removed the moment any session
    /// references it again (see `dispatch_session_new`'s bookkeeping),
    /// so re-use before the TTL elapses cancels a pending shutdown
    /// cleanly rather than racing it.
    unreferenced_backends: HashMap<String, std::time::Instant>,
}

/// Outcome of proactively restoring durable sessions during startup.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StartupRecoveryReport {
    /// Rows restored into the live [`SessionRegistry`].
    pub restored: usize,
    /// Rows whose backend recovery RPC failed.
    pub failed: usize,
    /// Rows already registered in this router, so no recovery was needed.
    pub skipped: usize,
}

/// Startup scheduling policy for durable session recovery. The scheduler
/// allows different backend processes to recover concurrently; each
/// individual backend remains serialized by its own stdio process mutex.
#[derive(Debug, Clone)]
pub struct StartupRecoveryPolicy {
    pub timeout: Duration,
    pub concurrency: usize,
    pub fail_fast: bool,
}

impl Default for StartupRecoveryPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            concurrency: 2,
            fail_fast: false,
        }
    }
}

impl StartupRecoveryPolicy {
    pub fn validate(&self) -> Result<(), RouterError> {
        if self.timeout.is_zero() {
            return Err(RouterError::InvalidRecoveryPolicy(
                "timeout must be greater than zero".to_string(),
            ));
        }
        if self.concurrency == 0 {
            return Err(RouterError::InvalidRecoveryPolicy(
                "concurrency must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

struct PreparedRecoveryJob {
    tenant_id: TenantId,
    entry: crate::session_registry::SessionEntry,
    admission: SessionAdmissionPermit,
    backend: acpx_conductor::supervisor::SharedBackendProcess,
    call_policy: BackendCallPolicy,
    request: serde_json::Value,
    request_id: serde_json::Value,
}

/// Result of one lifecycle reaper pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleReapReport {
    pub closed: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Debug, Default)]
struct AdmissionState {
    live_total: usize,
    live_by_tenant: HashMap<TenantId, usize>,
    reserved_total: usize,
    reserved_by_tenant: HashMap<TenantId, usize>,
}

impl AdmissionState {
    fn reserve(
        &mut self,
        tenant_id: &TenantId,
        lifecycle: &LifecycleConfig,
    ) -> Result<(), RouterError> {
        let total = self.live_total + self.reserved_total;
        if total >= lifecycle.max_sessions_total {
            return Err(RouterError::GlobalSessionCapacity {
                current: total,
                limit: lifecycle.max_sessions_total,
            });
        }
        let tenant_count = self
            .live_by_tenant
            .get(tenant_id)
            .copied()
            .unwrap_or_default()
            + self
                .reserved_by_tenant
                .get(tenant_id)
                .copied()
                .unwrap_or_default();
        if tenant_count >= lifecycle.max_sessions_per_tenant {
            return Err(RouterError::TenantSessionCapacity {
                tenant: tenant_id.0.clone(),
                current: tenant_count,
                limit: lifecycle.max_sessions_per_tenant,
            });
        }
        self.reserved_total += 1;
        *self
            .reserved_by_tenant
            .entry(tenant_id.clone())
            .or_default() += 1;
        Ok(())
    }

    fn release_reservation(&mut self, tenant_id: &TenantId) {
        debug_assert!(self.reserved_total > 0);
        self.reserved_total -= 1;
        let remove_tenant = {
            let reserved = self
                .reserved_by_tenant
                .get_mut(tenant_id)
                .expect("session admission tenant must exist");
            debug_assert!(*reserved > 0);
            *reserved -= 1;
            *reserved == 0
        };
        if remove_tenant {
            self.reserved_by_tenant.remove(tenant_id);
        }
    }

    fn commit(&mut self, tenant_id: &TenantId) {
        self.release_reservation(tenant_id);
        self.live_total += 1;
        *self.live_by_tenant.entry(tenant_id.clone()).or_default() += 1;
    }

    fn release_live(&mut self, tenant_id: &TenantId) {
        debug_assert!(self.live_total > 0);
        self.live_total -= 1;
        let remove_tenant = {
            let live = self
                .live_by_tenant
                .get_mut(tenant_id)
                .expect("registered session tenant must exist");
            debug_assert!(*live > 0);
            *live -= 1;
            *live == 0
        };
        if remove_tenant {
            self.live_by_tenant.remove(tenant_id);
        }
    }
}

/// Reserves one future registry insertion. Dropping an uncommitted permit
/// returns the reservation, including when Tokio cancels the request task.
struct SessionAdmissionPermit {
    state: Arc<Mutex<AdmissionState>>,
    tenant_id: TenantId,
    committed: bool,
}

impl SessionAdmissionPermit {
    fn commit(mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.commit(&self.tenant_id);
        self.committed = true;
    }
}

impl Drop for SessionAdmissionPermit {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.release_reservation(&self.tenant_id);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("request has no \"method\" field")]
    MissingMethod,
    #[error("request has no \"params\" field")]
    MissingParams,
    #[error("request has no \"id\" field")]
    MissingId,
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("params.sessionId missing or not a string")]
    MissingSessionId,
    #[error("no session registered for gateway session id {0}")]
    UnknownSession(String),
    #[error(
        "session capacity reached for tenant {tenant}: {current}/{limit} live gateway sessions"
    )]
    TenantSessionCapacity {
        tenant: String,
        current: usize,
        limit: usize,
    },
    #[error("global session capacity reached: {current}/{limit} live gateway sessions")]
    GlobalSessionCapacity { current: usize, limit: usize },
    #[error("backend response to session/new has no result.sessionId")]
    MissingBackendSessionId,
    #[error("backend rejected session/new: {0}")]
    BackendSessionNewError(serde_json::Value),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error(transparent)]
    Supervisor(#[from] acpx_conductor::supervisor::SupervisorError),
    #[error(transparent)]
    Framing(#[from] acpx_conductor::framing::FramingError),
    #[error("agents/status: unknown agent id {0}")]
    UnknownAgentId(String),
    #[error("session/new: agent {0} is disabled")]
    AgentDisabled(String),
    #[error("custom agent id {0} conflicts with an existing registered backend")]
    CustomAgentIdConflict(String),
    #[error(transparent)]
    Install(#[from] acpx_registry::InstallError),
    #[error("agents/install: missing or non-string params.id")]
    MissingAgentId,
    #[error("profile store: {0}")]
    Profile(#[from] crate::profile::ProfileStoreError),
    #[error("provider store: {0}")]
    Provider(#[from] crate::provider::ProviderStoreError),
    #[error("mcp server store: {0}")]
    McpServer(#[from] crate::mcp_servers::McpServerStoreError),
    #[error("custom agent store: {0}")]
    CustomAgent(#[from] crate::CustomAgentStoreError),
    #[error("keystore: {0}")]
    Keystore(#[from] crate::keystore::KeystoreError),
    #[error("session/new: no profile named {0}")]
    UnknownProfile(String),
    #[error("session/new cannot select both _acpx.profile and _acpx.agentId")]
    ConflictingSessionSelection,
    #[error("profile {profile} references unknown provider {provider}")]
    UnknownProviderRef { profile: String, provider: String },
    #[error("profile {profile}'s agent id {agent_id} has no npx/uvx distribution in the registry")]
    NoLaunchableDistribution { profile: String, agent_id: String },
    #[error("backend requires authentication before session/new (advertised authMethods: {0}); configure Profile::auth_method_id to pick one")]
    BackendRequiresAuthentication(serde_json::Value),
    #[error("backend rejected authenticate: {0}")]
    BackendAuthenticationError(serde_json::Value),
    #[error("authenticate: acpx's own initialize response advertises no authMethods (requested methodId: {0:?}); no transport-level bearer-token/session auth is bypassed by this -- see acpx-server's own HTTP/WS auth")]
    NoAuthMethodsAdvertised(Option<String>),
    #[error(
        "session/load: gateway session {0} not found in this process's live registry and \
         no persistence store is configured to recover it from -- pass ACPX_DB_PATH so \
         session/load can survive an acpx restart"
    )]
    SessionNotPersisted(String),
    #[error(
        "session/load: gateway session {0} could not be recovered from the persistence store: {1}"
    )]
    SessionRehydrationFailed(String, crate::persistence::PersistenceError),
    #[error(
        "session/load: gateway session {0} is still restoring; retry this request after recovery completes"
    )]
    SessionRestoring(String),
    #[error(
        "logout: acpx's own initialize response advertises no agentCapabilities.auth.logout \
         (gateway-level auth is transport-level, not ACP-level -- see acpx-server's own \
         HTTP/WS auth); acpx also has no single unambiguous backend a bare, session-less \
         logout could target across its multiple managed profiles"
    )]
    LogoutNotSupported,
    #[error("backend rejected session/list: {0}")]
    BackendSessionListError(serde_json::Value),
    #[error("startup recovery for gateway session {0} has non-object recovery params")]
    InvalidRecoveryParams(String),
    #[error("startup recovery timed out for gateway session {0}")]
    RecoveryTimeout(String),
    #[error("startup recovery stopped after a failed session: {0}")]
    RecoveryFailFast(String),
    #[error("invalid startup recovery policy: {0}")]
    InvalidRecoveryPolicy(String),
    #[error("persistence: {0}")]
    Persistence(#[from] crate::persistence::PersistenceError),
    #[error("durable config requires Router::with_persistence to be configured first")]
    DurableConfigRequiresPersistence,
    #[error("secret rotation requires Router::enable_durable_config to be configured first")]
    RotationRequiresDurableConfig,
    #[error("master keyring I/O error: {0}")]
    KeyringIo(String),
    #[error(
        "session/retention/pin: tenant {tenant_id} already has {current} of at most {limit} \
         pinned sessions"
    )]
    PinQuotaExceeded {
        tenant_id: String,
        current: usize,
        limit: usize,
    },
    #[error(
        "lifecycle reaper: backend session/close for gateway session {0} did not respond \
         within {1:?}; leaving this session in place for a later reap pass instead of \
         holding the whole gateway's lock indefinitely"
    )]
    ReapBackendCallTimeout(String, Duration),
    #[error(
        "backend produced no output for {0:?}; the wedged process was killed and this call \
         failed rather than holding its per-process lock forever"
    )]
    BackendIdleReadTimeout(Duration),
    #[error(
        "backend produced no response to its {0} handshake within {1:?}; the wedged process \
         was killed and this call failed rather than holding its per-process lock forever"
    )]
    BackendHandshakeTimeout(&'static str, Duration),
    #[error(
        "backend process's reader task ended (process exited or a read error occurred) before \
         this call's response arrived"
    )]
    BackendDemuxReaderClosed,
    #[error(
        "backend stdin write timed out after {0:?}; the wedged process was killed and this \
         call failed rather than holding its per-process/writer lock forever"
    )]
    BackendWriteTimeout(Duration),
}

/// Hard ceiling on the per-candidate backend `session/close` round trip
/// inside [`Router::reap_expired_sessions`].
///
/// **Real incident this guards against, not a hypothetical.** The lifecycle
/// reaper (`acpx-server/src/main.rs`'s periodic tick, default every 60s)
/// calls this function while the caller already holds the single global
/// [`SharedRouter`] mutex for the *entire* pass across every idle-expired
/// session -- exactly the same lock `acp_bridge::BridgeRuntime::
/// refresh_models` guards with `MODEL_PROBE_TIMEOUT` for the same reason.
/// This function's own `ensure_backend_initialized`/`read_matching_response`
/// round trip (sending a `session/close` to the session's backend) had no
/// timeout of its own -- confirmed live: after acpx ran for a while with a
/// real Zed client attached, one idle session's backend (`codex-acp`)
/// stopped answering, the very next lifecycle-reaper tick's `session/close`
/// blocked forever inside this loop, and every other tenant/session on the
/// server hung behind the same held mutex from that point on -- matching a
/// real "works right after restart, wedges again later" report exactly.
/// Timing out here drops the inner future (and the per-process
/// `BackendProcess` lock acquired inside it), so a single stuck backend
/// only skips *this* session's reap this pass (it stays live, tried again
/// on the next tick) instead of freezing the whole gateway.
const REAP_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard backstop for a single `read_value` call inside
/// [`read_matching_response`]'s read loop -- distinct from, and much
/// larger than, `LifecycleConfig::active_turn_deadline`. That deadline is
/// a *cooperative* nudge: once a turn has been continuously in-flight too
/// long, the reaper best-effort-sends the backend a `session/cancel`
/// notification and trusts it to eventually reply. This constant is the
/// backstop for when that trust is misplaced -- a backend that ignores
/// its own cancel notification (or has simply wedged/deadlocked
/// internally) leaves this loop's `read_value().await` blocked forever,
/// which holds the per-process `BackendProcess` lock forever, which
/// queues up every other call against that same backend (every session
/// on a shared agent) behind it indefinitely -- confirmed live: a Zed
/// client left connected long enough to hit this, and every subsequent
/// request to the same agent hung at "loading" with no way to recover
/// short of restarting acpx-server entirely.
///
/// Deliberately much longer than `PERMISSION_RELAY_TIMEOUT` (15 minutes):
/// that wait happens *after* `read_value` already returned a real
/// backend-initiated request, so it is a separate, already-bounded await
/// and never itself counts against this budget -- only genuine silence on
/// the wire (no bytes at all) does. A well-behaved backend should produce
/// *some* output (even just the final error/cancelled reply to its own
/// cancel notification) well within this window for any real turn; one
/// that doesn't is not recoverable by waiting longer.
///
/// On expiry the backend process is killed outright, not merely given up
/// on: leaving it running and simply returning early would let its
/// eventual, stale reply for the abandoned `id` arrive during some later,
/// unrelated call's own read loop, where it has no `method` and doesn't
/// match that call's `id` -- indistinguishable from a real notification,
/// so it would be silently misclassified and attached to a completely
/// unrelated response's `_acpx.updates`. Killing avoids that entirely and
/// forces `Supervisor::ensure_running`'s next call to see
/// `has_exited() == true` and spawn a fresh process instead.
const BACKEND_IDLE_READ_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// **`acpx-connect-loading-feedback`.** Bound on how long a connect/resume
/// class call (`session/new`, `session/load`, `session/resume`) may go
/// silent on the wire before it fails, distinct from and much shorter
/// than [`BACKEND_IDLE_READ_TIMEOUT`]'s 20-minute backstop.
///
/// These three methods are exactly the ones a real ACP client's
/// connection/thread-load UI gates its "loading" indicator on (in Zed,
/// `ConversationView::is_loading()` -- see `acp_thread`/`agent_ui`'s
/// `ServerState::Loading`, which wraps this whole call and only clears on
/// success or a hard error) -- unlike `session/prompt`, where a long wait
/// is often entirely legitimate (a real, in-progress generation) and
/// already has its own two-tier budget (`LifecycleConfig::
/// active_turn_deadline`'s cooperative cancel nudge, then this same 20
/// minute hard backstop), nothing legitimately keeps a *connect* call
/// silent for anywhere close to 20 minutes: `session/new`/`session/load`/
/// `session/resume` only ever reconstruct/initialize session state, never
/// run a real model turn. Leaving them on the full 20-minute budget meant
/// a genuinely wedged backend left a real client's connect/resume UI
/// spinning with zero feedback for up to 20 minutes before this module's
/// own kill-and-fail recovery ever kicked in -- technically "shows as
/// loading" the whole time, but not a bound any user would tolerate
/// waiting out, and indistinguishable from a true hang to everyone
/// downstream. A short, deterministic failure here instead lets a real
/// client's own error/retry path (in Zed: `ConversationView::
/// handle_load_error` -> `ServerState::LoadError`) fire promptly.
const SESSION_ESTABLISH_IDLE_READ_TIMEOUT: Duration = Duration::from_secs(45);

/// **Real incident fix, not a hypothetical.** Bound on how long
/// [`ensure_backend_initialized`]'s `initialize`/`authenticate` handshake
/// reads may go silent before this call fails and the wedged process is
/// killed.
///
/// Before this constant existed, both handshake reads were bare
/// `proc.reader.read_value().await` calls with no timeout of their own --
/// unlike every read that follows them, which is always bounded by
/// [`BACKEND_IDLE_READ_TIMEOUT`]/[`SESSION_ESTABLISH_IDLE_READ_TIMEOUT`]
/// via [`read_matching_response_with_idle_timeout`]. Confirmed live: a
/// backend that never answers `initialize` left every dispatch path that
/// calls `ensure_backend_initialized` first -- `session/new`, and
/// critically `session/load`/`session/resume` against a durably
/// recovered session whose `BackendProcess` had not yet completed its
/// handshake -- hanging forever, holding the per-process
/// `BackendProcess` lock for the lifetime of the daemon. At the bridge
/// layer (`acp_bridge::bind`) this manifested as a permanent
/// `BridgeSessionState::Binding` livelock: every retry of the exact
/// "bridge session binding is in progress; retry the request" error
/// kept observing the same stuck state, since neither `finish_binding`
/// nor `fail_binding` could ever run. Set shorter than
/// `SESSION_ESTABLISH_IDLE_READ_TIMEOUT`'s already-short 45s connect
/// budget -- a handshake that hasn't completed in this window is
/// exactly as legitimately silent as a connect call that hasn't, so it
/// gets an equally aggressive bound, not the 20-minute prompt-turn
/// backstop.
const BACKEND_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound for the raw stdin write [`Router::cancel_stuck_turns`] makes to a
/// backend's `cancel_writer` pipe. **Live incident this closes**: this
/// call runs every lifecycle-reaper tick while the caller
/// ([`acpx_server::main`]'s reaper task) is holding the *entire* router
/// mutex around the whole `reap_expired_sessions` /
/// `reap_unreferenced_backends` / `cancel_stuck_turns` sequence -- unlike
/// [`dispatch_session_cancel_shared`], which deliberately drops the
/// router lock *before* writing to the exact same `cancel_writer` pipe
/// (see that function's doc comment), this one has no such luxury: it's
/// mutating `&mut self` in place mid-reap. A backend process wedged badly
/// enough not to be draining its own stdin (observed live: `codex-acp`
/// past its `active_turn_deadline`, already failing capability probes)
/// leaves `ChildStdin::write_all` blocked once the pipe's kernel buffer
/// fills, which -- with no timeout here -- blocks this call forever,
/// which blocks the reaper's held router mutex forever, which then wedges
/// every other concurrent request that touches the router lock at all
/// (plain `initialize`/`agents/list` -- neither of which ever talks to a
/// backend -- hang indefinitely, and client sockets pile up in
/// `CLOSE-WAIT` because their handler tasks can never return to their own
/// read loop to notice the peer has gone away). Short timeout is
/// deliberate: writing one small JSON line to a healthy process's stdin
/// pipe should complete in microseconds, and this is already a
/// best-effort notification (the reap loop's own doc comment: "this only
/// ever asks the backend to *stop*") -- skipping one candidate on timeout
/// and retrying next tick is strictly better than freezing the whole
/// gateway on its behalf.
const CANCEL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

impl Router {
    pub fn default_agent_id(&self) -> &str {
        &self.default_agent_id
    }

    pub fn new(default_agent_id: impl Into<String>) -> Self {
        Self {
            supervisor: acpx_conductor::Supervisor::new(),
            sessions: SessionRegistry::new(),
            lifecycle: LifecycleConfig::default(),
            admission: Arc::new(Mutex::new(AdmissionState::default())),
            default_agent_id: default_agent_id.into(),
            native_auth_method_id: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("valid HTTP client configuration"),
            registry_cache: None,
            capability_cache: acpx_registry::CapabilityCache::new(Duration::from_secs(300)),
            persistence: None,
            agent_enablement: None,
            custom_agents: None,
            materialized_custom_agents: HashSet::new(),
            providers: ProviderStore::new(),
            keystore: Keystore::new(),
            profiles: ProfileStore::new(),
            mcp_servers: McpServerStore::new(),
            secret_keyring: None,
            keyring_path: None,
            notification_hub: NotificationHub::new(),
            agent_request_hub: AgentRequestHub::new(),
            interaction_hub: InteractionHub::new(),
            pending_updates: PendingUpdates::new(),
            scavenged_backends: HashSet::new(),
            tenant_process_isolation: false,
            session_process_isolation: false,
            on_demand_recovery_enabled: true,
            process_reader_demux: false,
            unreferenced_backends: HashMap::new(),
        }
    }

    /// A clone of this router's live `session/update` notification hub
    /// (Phase 14) -- `acpx-server`'s stdio/WS transports call this once
    /// per connection to subscribe to whichever gateway sessions that
    /// connection touches. See `crate::notify`'s module doc comment.
    pub fn notification_hub(&self) -> NotificationHub {
        self.notification_hub.clone()
    }

    /// A clone of this router's live agent-request relay hub -- see
    /// [`Self::notification_hub`]'s doc comment for the sharing
    /// convention and `crate::agent_relay`'s module doc comment for what
    /// this hub is for.
    pub fn agent_request_hub(&self) -> AgentRequestHub {
        self.agent_request_hub.clone()
    }

    /// A clone of the persistent client interaction bridge. Transports bind
    /// sessions they own and resolve client responses through this hub.
    pub fn interaction_hub(&self) -> InteractionHub {
        self.interaction_hub.clone()
    }

    /// A clone of this router's `process_reader_demux` HTTP-fallback
    /// buffer -- see the `pending_updates` field's doc comment. Not
    /// exposed outside this crate (no `pub`): every caller today is a
    /// free function in this same module (`spawn_demux_consumer`,
    /// `dispatch_proxied_shared`).
    fn pending_updates(&self) -> PendingUpdates {
        self.pending_updates.clone()
    }

    /// **Phase 15.** Ensure exactly one idle scavenger task
    /// ([`backend_idle_scavenger`]) is running for this exact physical
    /// `backend` instance, spawning one the first time this backend is
    /// ever seen and doing nothing on every later call against the same
    /// still-running process. Called from every `_shared` dispatch path
    /// right after `Supervisor::ensure_running` hands back a
    /// `SharedBackendProcess`, while that call already holds `self`'s own
    /// lock briefly for bookkeeping -- spawning a task is not backend
    /// I/O, so doing it here doesn't violate this file's "release the
    /// lock before any backend round trip" convention.
    fn spawn_idle_scavenger_if_new(
        &mut self,
        router_handle: &SharedRouterHandle,
        agent_id: &str,
        backend: &acpx_conductor::supervisor::SharedBackendProcess,
    ) {
        let key = std::sync::Arc::as_ptr(backend) as usize;
        if !self.scavenged_backends.insert(key) {
            return;
        }
        let ctx = LiveNotifyCtx {
            router: std::sync::Arc::clone(router_handle),
            agent_id: agent_id.to_string(),
            tenant_id: None,
            agent_relay: self.agent_request_hub.clone(),
            gateway_session_id: None,
            notification_hub: self.notification_hub.clone(),
            backend: std::sync::Arc::clone(backend),
        };
        let backend = std::sync::Arc::clone(backend);
        tokio::spawn(backend_idle_scavenger(backend, ctx));
    }

    /// Attach a [`PersistenceStore`] -- session metadata and transcripts
    /// are recorded from that point on. Builder-style so callers can write
    /// `Router::new(id).with_persistence(store)`.
    pub fn with_persistence(mut self, store: PersistenceStore) -> Self {
        self.agent_enablement = Some(AgentEnablement::new(store.clone()));
        self.custom_agents = Some(CustomAgentStore::new(store.clone()));
        self.persistence = Some(store);
        self
    }

    /// **`durable_secret_and_configuration_store`.** Load an on-disk (or
    /// freshly-created) [`crate::keystore::MasterKeyring`] at
    /// `keyring_path`, then repopulate `providers`/`profiles`/
    /// `mcp_servers`/`keystore` from whatever was persisted by a prior
    /// process's runtime CRUD -- and, from this call forward, every
    /// `profiles/create|update|delete`, `mcp_servers/create|update|
    /// delete`, and secret store also writes through to `persistence`.
    /// Requires [`Self::with_persistence`] to have been called first
    /// (there is nowhere durable to load from or write through to
    /// otherwise). Intended call site: `acpx-server`'s `main.rs`, once,
    /// right after `with_persistence`, before either transport starts
    /// accepting requests -- exactly like `warm_default_profiles`.
    pub async fn enable_durable_config(
        &mut self,
        keyring_path: std::path::PathBuf,
    ) -> Result<(), RouterError> {
        let Some(persistence) = self.persistence.clone() else {
            return Err(RouterError::DurableConfigRequiresPersistence);
        };
        let keyring = crate::keystore::MasterKeyring::load_or_create(&keyring_path)
            .map_err(|error| RouterError::KeyringIo(error.to_string()))?;

        for (key_ref, ciphertext, nonce, key_version) in persistence.load_all_secrets().await? {
            let plaintext = keyring
                .decrypt(key_version, &nonce, &ciphertext)
                .map_err(RouterError::Keystore)?;
            let secret = String::from_utf8(plaintext).map_err(|error| {
                RouterError::KeyringIo(format!("decrypted secret was not valid UTF-8: {error}"))
            })?;
            self.keystore
                .insert_known(crate::keystore::KeyRef(key_ref), secret);
        }

        for raw in persistence.load_providers().await? {
            if let Ok(provider) = serde_json::from_value::<crate::provider::ProviderConfig>(raw) {
                self.register_provider(provider);
            }
        }

        for raw in persistence.load_mcp_servers().await? {
            self.mcp_servers.create(raw)?;
        }

        for raw in persistence.load_profiles().await? {
            if let Ok(profile) = serde_json::from_value::<Profile>(raw) {
                self.profiles.create(profile)?;
            }
        }

        self.secret_keyring = Some(Arc::new(Mutex::new(keyring)));
        self.keyring_path = Some(keyring_path);
        Ok(())
    }

    /// Mint a new keyring version, re-encrypt every currently-known
    /// secret under it, persist both the updated ciphertext rows and the
    /// new keyring file, and return the new version number. Operator-
    /// triggered (see `acpx-server`'s `ACPX_MASTER_KEYRING_ROTATE`) --
    /// there is no automatic rotation schedule.
    pub async fn rotate_master_key(&mut self) -> Result<u32, RouterError> {
        let Some(keyring_lock) = self.secret_keyring.clone() else {
            return Err(RouterError::RotationRequiresDurableConfig);
        };
        let Some(persistence) = self.persistence.clone() else {
            return Err(RouterError::RotationRequiresDurableConfig);
        };
        let keyring_path = self
            .keyring_path
            .clone()
            .ok_or(RouterError::RotationRequiresDurableConfig)?;

        // Snapshot every (key_ref, secret) pair before touching the
        // keyring lock -- `Keystore::iter` borrows `self.keystore`
        // immutably, and re-encrypting doesn't need to hold that borrow
        // across the persistence `.await`s below.
        let secrets: Vec<(crate::keystore::KeyRef, String)> = self
            .keystore
            .iter()
            .map(|(key_ref, secret)| (key_ref.clone(), secret.to_string()))
            .collect();

        let new_version = {
            let mut keyring = keyring_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let version = keyring.rotate();
            keyring
                .save(&keyring_path)
                .map_err(|error| RouterError::KeyringIo(error.to_string()))?;
            version
        };

        let now = now_rfc3339();
        for (key_ref, secret) in secrets {
            let (version, nonce, ciphertext) = {
                let keyring = keyring_lock
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                keyring.encrypt(secret.as_bytes())
            };
            persistence
                .record_secret(
                    key_ref.0,
                    ciphertext,
                    nonce,
                    version,
                    now.clone(),
                    Some(now.clone()),
                )
                .await?;
        }

        Ok(new_version)
    }

    /// Encrypt-and-persist one secret, no-op if durable config was never
    /// enabled. Called right after `Keystore::store` at every profile
    /// secret entry point. Errors propagate (fail-closed): once durable
    /// config is enabled, a caller that got a successful `profiles/
    /// create` response should never discover after a restart that the
    /// secret silently never made it to disk.
    async fn persist_secret_if_durable(
        &self,
        key_ref: &crate::keystore::KeyRef,
        secret: &str,
    ) -> Result<(), RouterError> {
        let (Some(keyring_lock), Some(persistence)) =
            (self.secret_keyring.as_ref(), self.persistence.as_ref())
        else {
            return Ok(());
        };
        let (version, nonce, ciphertext) = {
            let keyring = keyring_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            keyring.encrypt(secret.as_bytes())
        };
        persistence
            .record_secret(
                key_ref.0.clone(),
                ciphertext,
                nonce,
                version,
                now_rfc3339(),
                None,
            )
            .await?;
        Ok(())
    }

    async fn persist_profile_if_durable(&self, profile: &Profile) -> Result<(), RouterError> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let json = serde_json::to_value(profile).expect("Profile always serializes");
        persistence
            .upsert_profile(profile.name.clone(), json)
            .await?;
        Ok(())
    }

    async fn delete_persisted_profile_if_durable(&self, name: &str) -> Result<(), RouterError> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        persistence.delete_profile(name).await?;
        Ok(())
    }

    async fn persist_mcp_server_if_durable(
        &self,
        name: &str,
        entry: &serde_json::Value,
    ) -> Result<(), RouterError> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        persistence.upsert_mcp_server(name, entry.clone()).await?;
        Ok(())
    }

    async fn delete_persisted_mcp_server_if_durable(&self, name: &str) -> Result<(), RouterError> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        persistence.delete_mcp_server(name).await?;
        Ok(())
    }

    async fn ensure_agent_enabled(&self, agent_id: &str) -> Result<(), RouterError> {
        if let Some(enablement) = &self.agent_enablement {
            if !enablement.is_enabled(agent_id).await? {
                return Err(RouterError::AgentDisabled(agent_id.to_owned()));
            }
        }
        Ok(())
    }

    async fn custom_spawn_spec(
        &self,
        agent_id: &str,
    ) -> Result<Option<acpx_conductor::SpawnSpec>, RouterError> {
        let Some(custom_agents) = &self.custom_agents else {
            return Ok(None);
        };
        let Some(agent) = custom_agents.get(agent_id).await? else {
            return Ok(None);
        };
        let mut spec = acpx_conductor::SpawnSpec::new(agent.command, agent.args);
        spec.env.extend(agent.env);
        if let Some(cwd) = agent.cwd {
            spec = spec.with_cwd(cwd);
        }
        Ok(Some(spec))
    }

    async fn ensure_custom_agent_registered(
        &mut self,
        agent_id: &str,
    ) -> Result<bool, RouterError> {
        let Some(spec) = self.custom_spawn_spec(agent_id).await? else {
            if self.materialized_custom_agents.contains(agent_id) {
                return Err(RouterError::UnknownAgentId(agent_id.to_owned()));
            }
            return Ok(false);
        };
        if self.supervisor.spec(agent_id).is_some()
            && !self.materialized_custom_agents.contains(agent_id)
        {
            return Err(RouterError::CustomAgentIdConflict(agent_id.to_owned()));
        }
        self.supervisor.register(agent_id.to_owned(), spec);
        self.materialized_custom_agents.insert(agent_id.to_owned());
        Ok(true)
    }

    /// Clone the optional durable store for server-owned features that need
    /// to persist metadata adjacent to a native gateway session.
    pub fn persistence_store(&self) -> Option<PersistenceStore> {
        self.persistence.clone()
    }

    /// Override native session limits. Server configuration should validate
    /// the values before constructing a router; this builder preserves the
    /// low-friction in-process test API.
    pub fn with_lifecycle_config(mut self, config: LifecycleConfig) -> Self {
        self.lifecycle = config;
        self
    }

    /// **`virtual_and_pinned_resource_limits`.** Read-only accessor so a
    /// caller outside this module (`acpx-server`'s strict `/acp` bridge
    /// reaper task, which owns its own separate `BridgeSessionStore` not
    /// tracked by this `Router`) can reuse the same deployment-configured
    /// `unbound_bridge_session_ttl` rather than needing a second,
    /// independently-configured copy of it.
    pub fn lifecycle_config(&self) -> &LifecycleConfig {
        &self.lifecycle
    }

    /// Replace the live notification hub before transports are attached.
    /// The server uses this to apply deployment-level subscriber limits.
    pub fn with_notification_hub(mut self, notification_hub: NotificationHub) -> Self {
        self.notification_hub = notification_hub;
        self
    }

    /// Configure whether a managed profile's backend process is shared
    /// across tenants (default) or isolated per tenant.
    pub fn with_tenant_process_isolation(mut self, enabled: bool) -> Self {
        self.tenant_process_isolation = enabled;
        self
    }

    /// Configure whether every new managed session gets its own dedicated
    /// backend process (see `session_process_isolation`'s field doc
    /// comment). Composable with [`Self::with_tenant_process_isolation`]:
    /// both may be enabled together, in which case the per-session key is
    /// layered on top of the per-tenant key.
    pub fn with_session_process_isolation(mut self, enabled: bool) -> Self {
        self.session_process_isolation = enabled;
        self
    }

    /// Wire `ACPX_STARTUP_SESSION_RECOVERY_ENABLED` into on-demand
    /// rehydration too -- see `on_demand_recovery_enabled`'s field doc
    /// comment. `main.rs` passes the same `config.
    /// startup_session_recovery_enabled` value used to gate the eager
    /// batch job at boot; this is one operator-facing toggle, not two.
    pub fn with_on_demand_recovery_enabled(mut self, enabled: bool) -> Self {
        self.on_demand_recovery_enabled = enabled;
        self
    }

    /// Opt-in per-process reader-task demultiplexing -- see
    /// `process_reader_demux`'s field doc comment, including why the
    /// default is still off. Disabled by default.
    pub fn with_process_reader_demux(mut self, enabled: bool) -> Self {
        self.process_reader_demux = enabled;
        self
    }

    /// Configure the backend auth method ACPX uses for a native/unmanaged
    /// session. Managed profiles retain their own `auth_method_id`, and an
    /// unset native value still refuses to guess.
    pub fn with_native_auth_method_id(mut self, method_id: Option<String>) -> Self {
        self.native_auth_method_id = method_id.filter(|value| !value.is_empty());
        self
    }

    /// **Real bug this fixes** (found chasing a startup-recovery failure
    /// that never reproduced live, tracked down to a race): every
    /// registry-listed agent (`codex-acp`, `claude-acp`, ...) gets an
    /// auto-seeded [`Profile`] at startup (`ensure_default_profiles_seeded`)
    /// specifically so native/unmanaged sessions still pick up its
    /// `allow_fs_access`/`allow_terminal_access`/`permission_policy` --
    /// but that seeded profile's `auth_method_id` is always `None` (never
    /// set by auto-seeding, only by an operator's explicit
    /// `profiles/create`). The previous `if profile.is_none()` guard here
    /// meant *any* call site resolving through that seeded profile --
    /// `call_policy_for`'s no-profile-name fallback (every startup
    /// recovery of a native/bridge session) and `dispatch_session_new`'s
    /// own `call_policy_profile` fallback alike -- silently discarded
    /// `ACPX_NATIVE_AUTH_METHOD_ID`/`Router::with_native_auth_method_id`
    /// entirely, even though a profile was technically found. Confirmed
    /// live: a persisted `codex-acp` bridge session failed recovery on
    /// every single restart with `backend requires authentication...`,
    /// while a live client's *first* prompt against the very same fresh
    /// backend process minutes later succeeded -- only because an
    /// unrelated periodic model-refresh capability probe
    /// (`probe_adapter_capabilities`, which already worked around this
    /// exact defect locally by passing `None` instead of a resolved
    /// profile) happened to authenticate the shared process first. Fixing
    /// it here, not by special-casing more call sites the way the probe
    /// did, closes it for every current and future caller uniformly:
    /// falling back to `native_auth_method_id` whenever the *resolved
    /// auth method specifically* is absent, regardless of whether some
    /// other (non-auth) profile field was found.
    fn call_policy(&self, profile: Option<&Profile>) -> BackendCallPolicy {
        let mut policy = BackendCallPolicy::from_profile(profile);
        if policy.auth_method_id.is_none() {
            policy.auth_method_id = self.native_auth_method_id.clone();
        }
        policy
    }

    /// [`call_policy`], but resolves `profile_name` (a session's own
    /// persisted/explicit profile, `None` for native/unmanaged mode) the
    /// same way `dispatch_session_new`/`dispatch_session_new_shared` now
    /// do: an explicit profile name wins as before, but a `None` (every
    /// native-mode session, which is what the `/acp` bridge always creates)
    /// still picks up whatever profile [`Router::ensure_default_profiles_seeded`]
    /// auto-seeded under this exact `agent_id`, instead of always falling
    /// through to `BackendCallPolicy::default()`'s `false`/`false`. Without
    /// this, a session's *first* backend round trip (`session/new`) could
    /// declare `fs`/`terminal` capability `true` at `initialize` time (per
    /// that same fallback) while every later `session/prompt`/`cancel`/...
    /// against the very same session recomputed a call policy that denies
    /// them anyway -- a declared-vs-enforced contradiction, not just an
    /// inconsistency, since a well-behaved backend that trusted the
    /// `initialize` declaration and asked would get a false "disabled for
    /// this profile" error back.
    async fn call_policy_for(
        &mut self,
        profile_name: Option<&str>,
        agent_id: &str,
    ) -> BackendCallPolicy {
        if let Some(name) = profile_name {
            return self.call_policy(self.profiles.get(name));
        }
        self.call_policy(self.profiles.get(agent_id))
    }

    /// Warm [`ensure_default_profiles_seeded`] once, outside any per-request
    /// critical section -- call this right after constructing/registering
    /// agents on a `Router`, before it ever serves a real request. Real
    /// registry-agent auto-seeding (see that function's doc comment) can
    /// do genuine network I/O the first time (`ensure_registry_loaded`'s
    /// registry fetch) plus a `crate::detect::detect` subprocess check per
    /// candidate agent; every `_shared` dispatch function in this file
    /// exists specifically to never hold `router`'s lock across I/O like
    /// that, so none of them call this themselves (confirmed as a real,
    /// concurrency-test-caught regression, not a hypothetical one, when
    /// this was tried inline in `call_policy_for` -- see git history).
    /// `call_policy_for`'s own fallback lookup is a plain, cheap
    /// `HashMap::get` either way; it just returns nothing to elevate on
    /// until this has actually run once.
    pub async fn warm_default_profiles(&mut self) {
        self.ensure_default_profiles_seeded().await;
    }

    /// Lifecycle-management seam used by the server's future authenticated
    /// retention controls. Pinning never bypasses explicit `session/close`.
    pub async fn set_session_pinned(
        &mut self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
        pinned: bool,
    ) -> Result<(), RouterError> {
        let gateway_id = acpx_proto::session::GatewaySessionId(gateway_session_id.to_string());
        if self.sessions.resolve(tenant_id, &gateway_id).is_none() {
            return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
        }
        if let Some(store) = self.persistence.clone() {
            store
                .update_session_pinned(gateway_session_id.to_string(), pinned, now_unix_nanos())
                .await?;
        }
        self.sessions.set_pinned(tenant_id, &gateway_id, pinned);
        Ok(())
    }

    /// **`retention_administration`.** `session/retention/pin`'s actual
    /// handler: [`Self::set_session_pinned`] plus the per-tenant pin
    /// quota (`LifecycleConfig::max_pinned_sessions_per_tenant`). A
    /// no-op re-pin of an already-pinned session never itself trips the
    /// quota it is already counted toward -- only a session transitioning
    /// from unpinned to pinned can exceed it.
    async fn set_session_pinned_administered(
        &mut self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
        pinned: bool,
    ) -> Result<(), RouterError> {
        if pinned {
            if let Some(limit) = self.lifecycle.max_pinned_sessions_per_tenant {
                let gateway_id =
                    acpx_proto::session::GatewaySessionId(gateway_session_id.to_string());
                let already_pinned = self
                    .sessions
                    .resolve(tenant_id, &gateway_id)
                    .is_some_and(|entry| entry.pinned);
                let current = self.sessions.pinned_count(tenant_id);
                if !already_pinned && current >= limit {
                    return Err(RouterError::PinQuotaExceeded {
                        tenant_id: tenant_id.0.clone(),
                        current,
                        limit,
                    });
                }
            }
        }
        self.set_session_pinned(tenant_id, gateway_session_id, pinned)
            .await
    }

    /// **`retention_administration`.** `session/retention/set_ttl`'s
    /// handler. Mirrors [`Self::set_session_pinned`]'s ownership check
    /// and persistence write-through shape.
    async fn set_session_custom_ttl(
        &mut self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), RouterError> {
        let gateway_id = acpx_proto::session::GatewaySessionId(gateway_session_id.to_string());
        if self.sessions.resolve(tenant_id, &gateway_id).is_none() {
            return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
        }
        if let Some(store) = self.persistence.clone() {
            store
                .update_session_custom_ttl(
                    gateway_session_id.to_string(),
                    ttl.map(|duration| duration.as_secs() as i64),
                )
                .await?;
        }
        self.sessions.set_custom_ttl(tenant_id, &gateway_id, ttl);
        Ok(())
    }

    /// **`retention_administration`.** `session/retention/get`'s handler
    /// (and every other retention arm's response shape) -- resolves and
    /// formats one tenant-owned session's retention state. Errors
    /// `UnknownSession` for a session that either never existed or
    /// belongs to a different tenant (never distinguishable, matching
    /// every other tenant-ownership check in this file).
    fn retention_info_json(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
    ) -> Result<serde_json::Value, RouterError> {
        let gateway_id = acpx_proto::session::GatewaySessionId(gateway_session_id.to_string());
        let entry = self
            .sessions
            .resolve(tenant_id, &gateway_id)
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.to_string()))?;
        Ok(retention_entry_json(gateway_session_id, entry))
    }

    fn admit_session(&self, tenant_id: &TenantId) -> Result<SessionAdmissionPermit, RouterError> {
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reserve(tenant_id, &self.lifecycle)?;
        Ok(SessionAdmissionPermit {
            state: Arc::clone(&self.admission),
            tenant_id: tenant_id.clone(),
            committed: false,
        })
    }

    fn release_live_session(&self, tenant_id: &TenantId) {
        let mut state = self
            .admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.release_live(tenant_id);
    }

    /// Register how to spawn a given agent id. Mirrors
    /// `Supervisor::register` -- `Router` doesn't reinterpret the spec, it
    /// just owns the `Supervisor` instance.
    pub fn register_agent(&mut self, agent_id: impl Into<String>, spec: acpx_conductor::SpawnSpec) {
        self.supervisor.register(agent_id, spec);
    }

    /// Diagnostic/test-only seam: `ProfileStore` has no CRUD wiring exposed
    /// over any transport yet (no admin endpoint, no config-file loader --
    /// `_acpx.profile` can currently only ever name a profile registered
    /// this way, in-process, before the first request that references it).
    /// Exists so integration tests can register one directly against
    /// `Router` without needing that wiring to exist first.
    pub fn register_profile(
        &mut self,
        profile: crate::profile::Profile,
    ) -> Result<(), crate::profile::ProfileStoreError> {
        self.profiles.create(profile)
    }

    /// Revoke a custom definition from the live process manager after the
    /// admin plane deletes it. Existing sessions using that definition are
    /// intentionally terminated rather than left attached to a command an
    /// operator has explicitly removed.
    pub async fn revoke_custom_agent(&mut self, agent_id: &str) -> Result<(), RouterError> {
        self.supervisor.stop(agent_id).await?;
        for profile in self
            .profiles
            .list()
            .filter(|profile| profile.agent_id == agent_id)
        {
            let key = format!("profile:{}", profile.name);
            self.supervisor.stop(&key).await?;
            self.supervisor
                .stop_prefix(&format!("{key}:tenant:"))
                .await?;
        }
        // Keep the id marked as materialized: without the durable custom
        // record, a stale direct supervisor spec must never become
        // launchable again in this daemon lifetime.
        self.materialized_custom_agents.insert(agent_id.to_owned());
        Ok(())
    }

    /// Ensure an official registry adapter has a launch specification
    /// registered without starting its process. The strict ACP bridge uses
    /// this before its first lazy-bound turn; native callers remain free to
    /// provision explicit specs through [`Self::register_agent`].
    pub async fn ensure_registry_agent_registered(
        &mut self,
        agent_id: &str,
    ) -> Result<(), RouterError> {
        if self.supervisor.spec(agent_id).is_some() {
            return Ok(());
        }
        self.ensure_registry_loaded().await;
        let agent = self
            .registry_cache
            .as_ref()
            .expect("registry cache populated by ensure_registry_loaded")
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .cloned()
            .ok_or_else(|| RouterError::UnknownAgentId(agent_id.to_string()))?;
        let spec = npx_spawn_spec(&agent).ok_or_else(|| RouterError::NoLaunchableDistribution {
            profile: "bridge".to_string(),
            agent_id: agent_id.to_string(),
        })?;
        self.supervisor.register(agent_id.to_string(), spec);
        Ok(())
    }

    /// Discover one adapter's runtime model and permission selectors without
    /// creating an ACPX gateway session. The temporary backend session is
    /// always closed before the result is cached.
    pub async fn probe_adapter_capabilities(
        &mut self,
        agent_id: &str,
        cwd: &str,
    ) -> Result<acpx_registry::AdapterCapabilities, RouterError> {
        self.ensure_agent_enabled(agent_id).await?;
        if self.supervisor.spec(agent_id).is_none() {
            self.ensure_registry_agent_registered(agent_id).await?;
        }

        let adapter_version = {
            self.ensure_registry_loaded().await;
            self.registry_cache
                .as_ref()
                .and_then(|registry| registry.agents.iter().find(|agent| agent.id == agent_id))
                .map(|agent| agent.version.clone())
        };
        let cache_key = acpx_registry::CapabilityCacheKey::new(agent_id, adapter_version.clone());
        if let Some(capabilities) = self.capability_cache.get(&cache_key, Instant::now()) {
            return Ok(capabilities);
        }

        let backend = self.supervisor.ensure_running(agent_id).await?;
        let capabilities = {
            let mut backend = backend.lock().await;
            // Bug fix: this previously hardcoded `BackendCallPolicy::default()`
            // (no `auth_method_id`), so a capability probe against any
            // backend that advertises non-empty `authMethods` at
            // `initialize` time always failed with
            // `BackendRequiresAuthentication`, even when the router was
            // configured with `Router::with_native_auth_method_id` (or a
            // profile-level `auth_method_id`) specifically to answer that.
            // Every other `ensure_backend_initialized` call site already
            // passes the resolved `self.call_policy(profile)` -- the probe
            // path is native/unmanaged (no profile), so `self.call_policy
            // (None)` is the same policy real `session/new` dispatch would
            // use for this agent.
            let call_policy = self.call_policy(None);
            ensure_backend_initialized(&mut backend, call_policy).await?;
            let initialize_result = backend.agent_capabilities.clone().unwrap_or_default();

            let new_id = serde_json::json!("acpx-capability-probe-new");
            write_backend_value_locked(
                &mut backend,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": new_id,
                    "method": "session/new",
                    "params": {"cwd": cwd, "mcpServers": []}
                }),
            )
            .await?;
            let (response, _, _) =
                read_matching_response(&mut backend, &new_id, BackendCallPolicy::default(), None)
                    .await?;
            let backend_session_id = extract_backend_session_id(&response)?;
            let capabilities = acpx_registry::AdapterCapabilities::from_acp(
                agent_id,
                &initialize_result,
                &response["result"],
            );

            let close_id = serde_json::json!("acpx-capability-probe-close");
            write_backend_value_locked(
                &mut backend,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": close_id,
                    "method": "session/close",
                    "params": {"sessionId": backend_session_id}
                }),
            )
            .await?;
            if let Err(error) =
                read_matching_response(&mut backend, &close_id, BackendCallPolicy::default(), None)
                    .await
            {
                tracing::warn!(%error, %agent_id, "capability probe could not close disposable backend session");
            }
            capabilities
        };

        self.capability_cache.invalidate_adapter(agent_id);
        self.capability_cache
            .put(cache_key, capabilities.clone(), Instant::now());
        Ok(capabilities)
    }

    /// Seed a provider config, overwriting any existing entry of the same
    /// name. Server-side-only seam -- there is deliberately no
    /// `providers/*` JSON-RPC method a remote client can call (per the
    /// task draft's "keys are maintained by this intermediate proxy": a
    /// provider's `base_url` plus whatever key a profile references are
    /// gateway-provisioned configuration, not something a client should
    /// ever be able to set for itself). `acpx-server`'s `main.rs` is the
    /// intended caller, loading providers from its own startup config;
    /// tests use it directly too.
    pub fn register_provider(&mut self, provider: crate::provider::ProviderConfig) {
        let name = provider.name.clone();
        let json = serde_json::to_value(&provider).expect("ProviderConfig always serializes");
        if self.providers.update(provider.clone()).is_err() {
            let _ = self.providers.create(provider);
        }
        // Best-effort durability mirror -- fire-and-forget `tokio::spawn`
        // since this method is sync (no runtime `providers/*` JSON-RPC
        // endpoint exists to make a stronger await-and-propagate promise
        // to, see this method's own doc comment) and providers are
        // low-frequency, startup/provisioning-time writes. `enable_
        // durable_config`'s own load path re-applies `ACPX_CONFIG_FILE`
        // providers every boot regardless, so this only closes the gap
        // for providers registered outside that file (e.g. directly by
        // an embedding caller).
        if let Some(persistence) = self.persistence.clone() {
            tokio::spawn(async move {
                if let Err(error) = persistence.upsert_provider(name.clone(), json).await {
                    tracing::warn!(%error, provider = %name, "failed to persist provider config");
                }
            });
        }
    }

    /// Store a raw secret, returning its opaque [`crate::keystore::KeyRef`]
    /// for a [`crate::profile::Profile::key_ref`]. Same server-side-only
    /// rationale as [`Self::register_provider`] -- see that method's doc
    /// comment.
    pub fn store_key(&mut self, secret: impl Into<String>) -> crate::keystore::KeyRef {
        self.keystore.store(secret)
    }

    /// Test/observability seam: live-process status for a given
    /// supervisor key (a native mode agent id, or a profile's
    /// `"profile:<name>"` key -- see `resolve_profile`). Distinct from the
    /// `agents/status` JSON-RPC method, which answers a different
    /// question (whether the runtime/binary needed to launch an agent is
    /// present at all, via `crate::detect`), not "is a process currently
    /// running under this exact supervisor key right now".
    pub fn process_status(
        &mut self,
        supervisor_key: &str,
    ) -> acpx_conductor::supervisor::ProcessStatus {
        self.supervisor.status(supervisor_key)
    }

    /// Test/observability seam for a live backend process id.
    pub async fn process_id(&self, supervisor_key: &str) -> Option<u32> {
        self.supervisor.process_id(supervisor_key).await
    }

    /// Test/observability seam: the supervisor key `gateway_session_id`
    /// was created under. Lets a test assert on `process_status` (which
    /// takes `&mut self`) *after* the session itself has already been
    /// removed from the registry (e.g. post-reap), when
    /// `process_id_for_session` can no longer resolve it.
    pub fn supervisor_key_for_session(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
    ) -> Option<String> {
        Some(
            self.sessions
                .resolve(
                    tenant_id,
                    &acpx_proto::session::GatewaySessionId(gateway_session_id.to_string()),
                )?
                .agent_id
                .clone(),
        )
    }

    /// Test/observability seam: the live backend process id for whatever
    /// supervisor key `gateway_session_id` was created under, following
    /// the exact same lookup real proxied calls use (`entry.agent_id`).
    /// Primarily exists so `ACPX_SESSION_PROCESS_ISOLATION` tests can
    /// assert on a specific session's dedicated process without needing
    /// to reconstruct its randomly-minted supervisor key by hand.
    pub async fn process_id_for_session(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
    ) -> Option<u32> {
        let agent_id = self
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id.to_string()),
            )?
            .agent_id
            .clone();
        self.process_id(&agent_id).await
    }

    /// True if `supervisor_key` was minted by this router's
    /// `session_process_isolation` path (see `dispatch_session_new`'s doc
    /// comment) -- i.e. it is provably exclusive to exactly one session,
    /// so stopping its backend process on that session's close can never
    /// affect any other still-open session. A profile/tenant-only key
    /// (the pre-existing shared-process default) never matches this and
    /// is therefore never auto-stopped on ordinary session close/reap,
    /// preserving that mode's whole point: keeping the process warm
    /// across session churn.
    fn is_session_scoped_supervisor_key(supervisor_key: &str) -> bool {
        supervisor_key.contains(":session:")
    }

    /// Stops `supervisor_key`'s backend process iff it is session-scoped
    /// (see [`Self::is_session_scoped_supervisor_key`]) -- a no-op for
    /// every pre-existing shared-process key, so ordinary session
    /// close/reap under the default (or tenant-isolated) process model is
    /// completely unaffected. Errors are logged, not propagated: a
    /// session is already being torn down by the caller regardless of
    /// whether its now-orphaned dedicated process manages to stop
    /// cleanly, and `Supervisor::stop` on an already-dead process is
    /// itself a no-op, not an error.
    async fn stop_if_session_scoped(&mut self, supervisor_key: &str) {
        if !Self::is_session_scoped_supervisor_key(supervisor_key) {
            return;
        }
        if let Err(error) = self.supervisor.stop(supervisor_key).await {
            tracing::warn!(
                %error,
                %supervisor_key,
                "failed to stop a session-isolated backend process on session close"
            );
        }
    }

    /// **`connector_reference_lifecycle`.** Called after any session
    /// removal (reap, explicit `session/close`, or persistence-failure
    /// rollback): if `supervisor_key` is no longer referenced by any
    /// live session, and connector-idle-shutdown is enabled, records
    /// when it first became unreferenced (a no-op if already recorded --
    /// the timer starts once, not on every later check). A session-
    /// scoped key (`stop_if_session_scoped` already stopped it
    /// unconditionally, so it is never a real candidate) is skipped, and
    /// a bare native/unmanaged agent id (never resolved through a
    /// profile) is also skipped -- shutting down a directly-`register_
    /// agent`-registered backend that a client may reference again at
    /// any moment via native mode (no `session/new` failure to recover
    /// from the way a profile-backed respawn has) is out of scope for
    /// this opt-in feature.
    fn mark_unreferenced_if_idle(&mut self, supervisor_key: &str) {
        let Some(_ttl) = self.lifecycle.connector_idle_shutdown_ttl else {
            return;
        };
        if Self::is_session_scoped_supervisor_key(supervisor_key)
            || !supervisor_key.starts_with("profile:")
        {
            return;
        }
        if self.sessions.count_by_agent_id(supervisor_key) == 0 {
            self.unreferenced_backends
                .entry(supervisor_key.to_string())
                .or_insert_with(std::time::Instant::now);
        }
    }

    /// Cancels a pending idle-shutdown for `supervisor_key` -- called
    /// whenever a session is (re-)registered under it, so a backend that
    /// gains a new referencing session before its grace period elapses
    /// is never stopped out from under that session.
    fn cancel_unreferenced_shutdown(&mut self, supervisor_key: &str) {
        self.unreferenced_backends.remove(supervisor_key);
    }

    /// **`connector_reference_lifecycle`.** Stops every supervisor key
    /// that has been continuously unreferenced (zero live sessions) for
    /// at least `LifecycleConfig::connector_idle_shutdown_ttl`. No-op
    /// (returns `0` immediately) unless that config is `Some` --
    /// intended to be polled periodically by the same caller driving
    /// [`Self::reap_expired_sessions`] (see `acpx-server`'s `main.rs`),
    /// but is itself independent of it: this only ever stops a process,
    /// never touches `SessionRegistry` (there is nothing left in it
    /// referencing an already-unreferenced key by definition).
    /// Double-checks the reference count at stop time (not just at the
    /// original observation) in case a new session raced in after
    /// `mark_unreferenced_if_idle` recorded it but before this call --
    /// that session's own `cancel_unreferenced_shutdown` call may not
    /// yet have run if this is invoked concurrently, so this is the
    /// final, authoritative check.
    pub async fn reap_unreferenced_backends(&mut self, now: std::time::Instant) -> usize {
        let Some(ttl) = self.lifecycle.connector_idle_shutdown_ttl else {
            return 0;
        };
        let expired: Vec<String> = self
            .unreferenced_backends
            .iter()
            .filter(|(_, since)| now.saturating_duration_since(**since) >= ttl)
            .map(|(key, _)| key.clone())
            .collect();
        let mut stopped = 0;
        for key in expired {
            self.unreferenced_backends.remove(&key);
            if self.sessions.count_by_agent_id(&key) != 0 {
                continue; // Raced with a new session -- leave it running.
            }
            if let Err(error) = self.supervisor.stop(&key).await {
                tracing::warn!(
                    %error,
                    supervisor_key = %key,
                    "failed to stop an idle, unreferenced backend process"
                );
                continue;
            }
            stopped += 1;
        }
        stopped
    }

    /// **`active_turn_deadline`.** Best-effort bounded recovery for a
    /// turn that has been continuously in-flight for at least
    /// `LifecycleConfig::active_turn_deadline` (see
    /// `SessionRegistry::stuck_in_flight_candidates`). For each stuck
    /// session: sends the backend the exact same `session/cancel`
    /// notification `Router::dispatch_session_cancel` would (through
    /// `Supervisor::cancel_writer`, independent of the per-process lock
    /// the stuck `dispatch_proxied`/`dispatch_proxied_shared` call is
    /// still holding -- see that method's doc comment for why that
    /// separate writer handle is what makes delivery possible at all
    /// while a call is still blocked reading a response), then
    /// force-clears `in_flight` so the session is no longer
    /// unconditionally skipped by every future `reap_expired_sessions`/
    /// `stuck_in_flight_candidates` pass.
    ///
    /// Deliberately does **not** itself close, remove, or touch the
    /// persisted session row -- the still-running backend call this is
    /// racing may yet resolve normally (this only ever asks the backend
    /// to *stop*, it cannot force an in-progress `read_matching_response`
    /// await in another task to return early), and forcibly tearing down
    /// the gateway session out from under that still-live call would
    /// turn one stuck turn into a second, worse bug: a response the
    /// original caller is still awaiting racing a session the registry
    /// no longer knows about. Once cleared, the *next* reaper tick's
    /// ordinary idle-TTL pass becomes the real backstop if the backend
    /// never actually confirms the cancellation -- exactly the "recovery
    /// policy" half of this report, not just "cancellation".
    pub async fn cancel_stuck_turns(&mut self, now: std::time::Instant) -> usize {
        let candidates = self
            .sessions
            .stuck_in_flight_candidates(now, &self.lifecycle);
        let mut cancelled = 0;
        for (tenant_id, gateway_id) in candidates {
            let Some(entry) = self.sessions.resolve(&tenant_id, &gateway_id).cloned() else {
                continue;
            };
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": entry.backend_session_id.0 }
            });
            if let Some(writer) = self.supervisor.cancel_writer(&entry.agent_id) {
                let write = async { writer.lock().await.write_value(&notification).await };
                match tokio::time::timeout(CANCEL_WRITE_TIMEOUT, write).await {
                    Ok(Err(error)) => {
                        tracing::warn!(
                            %error,
                            gateway_session_id = %gateway_id.0,
                            "failed to deliver best-effort session/cancel for a stuck turn"
                        );
                    }
                    Err(_) => {
                        // See `CANCEL_WRITE_TIMEOUT`'s doc comment: dropping
                        // `write` here releases the writer-pipe lock it may
                        // have acquired -- without this bound, a backend not
                        // draining its own stdin blocks this call (and the
                        // global router mutex the caller holds around the
                        // whole reaper pass) forever.
                        tracing::warn!(
                            gateway_session_id = %gateway_id.0,
                            agent_id = %entry.agent_id,
                            timeout_secs = CANCEL_WRITE_TIMEOUT.as_secs(),
                            "best-effort session/cancel write for a stuck turn timed out; \
                             skipping this candidate rather than blocking the router lock \
                             every other tenant/session depends on"
                        );
                    }
                    Ok(Ok(())) => {}
                }
            }
            self.spawn_transcript(gateway_id.0.clone(), Direction::ClientToAgent, notification);
            self.sessions.set_in_flight(&tenant_id, &gateway_id, 0);
            tracing::warn!(
                gateway_session_id = %gateway_id.0,
                "cancelled a turn that exceeded the active-turn deadline"
            );
            cancelled += 1;
        }
        cancelled
    }

    /// Fire-and-forget one transcript append, if persistence is attached.
    /// Never awaited by the caller -- spawned onto the runtime so a slow
    /// sqlite write can't add latency to the client-visible request path.
    fn spawn_transcript(
        &self,
        gateway_session_id: impl Into<String>,
        direction: Direction,
        payload: serde_json::Value,
    ) {
        spawn_transcript_fn(
            self.persistence.clone(),
            gateway_session_id,
            direction,
            payload,
        );
    }

    async fn persist_session_recovery(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
        entry: &crate::session_registry::SessionEntry,
        effective_params: serde_json::Value,
    ) -> Result<(), RouterError> {
        let Some(store) = self.persistence.as_ref() else {
            return Ok(());
        };
        store
            .record_session_with_recovery(
                gateway_session_id,
                entry.agent_id.clone(),
                entry.backend_session_id.0.clone(),
                entry.profile_name.clone(),
                now_rfc3339(),
                tenant_id.0.clone(),
                RecoveryMetadata {
                    cwd: entry.cwd.clone(),
                    recovery_params: Some(effective_params),
                    status: RecoveryStatus::Active,
                    recovery_method: RecoveryMethod::Load,
                    last_recovery_error: None,
                    created_at_unix_nanos: Some(now_unix_nanos()),
                    last_activity_at_unix_nanos: Some(now_unix_nanos()),
                    pinned: entry.pinned,
                    bridge_session_id: None,
                    bridge_model_alias: None,
                    bridge_config_options: None,
                },
            )
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_session_persistence(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: impl Into<String>,
        agent_id: impl Into<String>,
        backend_session_id: impl Into<String>,
        profile_name: Option<String>,
        client_request: serde_json::Value,
        agent_response: serde_json::Value,
    ) {
        spawn_session_persistence_fn(
            self.persistence.clone(),
            tenant_id.0.clone(),
            gateway_session_id,
            agent_id,
            backend_session_id,
            profile_name,
            client_request,
            agent_response,
        );
    }

    /// Restore all durable, open sessions before the router begins serving
    /// prompts. Failed rows remain durable for later inspection/retry but
    /// are intentionally never added to the live registry.
    ///
    /// "Retry" here means client-triggered, on-demand retry only (an
    /// explicit `session/load`/`session/resume` against that exact id) --
    /// `list_recoverable_sessions` itself excludes already-`RecoveryFailed`
    /// rows from this eager batch, so a permanently-doomed row (backend
    /// rejects the resume every time, e.g. no underlying rollout ever
    /// existed) is only ever attempted once here, not on every subsequent
    /// restart forever.
    pub async fn recover_open_sessions(&mut self) -> Result<StartupRecoveryReport, RouterError> {
        let Some(store) = self.persistence.clone() else {
            return Ok(StartupRecoveryReport::default());
        };
        let mut report = StartupRecoveryReport::default();

        for record in store.list_recoverable_sessions().await? {
            if record.recovery_method == RecoveryMethod::None {
                report.skipped += 1;
                continue;
            }
            let tenant_id = TenantId(record.tenant_id.clone());
            let gateway_id =
                acpx_proto::session::GatewaySessionId(record.gateway_session_id.clone());
            if self.sessions.resolve(&tenant_id, &gateway_id).is_some() {
                report.skipped += 1;
                continue;
            }

            store
                .update_recovery_status(
                    record.gateway_session_id.clone(),
                    RecoveryStatus::Restoring,
                    None,
                )
                .await?;

            let result = self.restore_open_session(&record).await;
            match result {
                Ok((tenant_id, entry, admission)) => {
                    store
                        .update_recovery_status(
                            record.gateway_session_id.clone(),
                            RecoveryStatus::Restored,
                            None,
                        )
                        .await?;
                    // See `dispatch_session_new`'s identical cancellation
                    // -- this startup-restore path re-registers a session
                    // against `entry.agent_id`'s supervisor key too.
                    let supervisor_key_for_cancel = entry.agent_id.clone();
                    self.sessions.insert(
                        &tenant_id,
                        acpx_proto::session::GatewaySessionId(record.gateway_session_id.clone()),
                        entry,
                    );
                    self.cancel_unreferenced_shutdown(&supervisor_key_for_cancel);
                    admission.commit();
                    report.restored += 1;
                }
                Err(error) => {
                    let error = error.to_string();
                    store
                        .update_recovery_status(
                            record.gateway_session_id.clone(),
                            RecoveryStatus::RecoveryFailed,
                            Some(error),
                        )
                        .await?;
                    report.failed += 1;
                }
            }
        }

        Ok(report)
    }

    /// Safely closes and removes sessions whose native lifecycle retention
    /// deadline has elapsed. Candidates are selected only when unpinned and
    /// not already executing; each is marked in-flight before any backend
    /// I/O so another reaper pass cannot race it.
    pub async fn reap_expired_sessions(&mut self, now: std::time::Instant) -> LifecycleReapReport {
        let candidates = self.sessions.reap_candidates(now, &self.lifecycle);
        let mut report = LifecycleReapReport::default();

        for (tenant_id, gateway_id) in candidates {
            let Some(entry) = self.sessions.resolve(&tenant_id, &gateway_id).cloned() else {
                report.skipped += 1;
                continue;
            };
            if entry.pinned || entry.in_flight != 0 {
                report.skipped += 1;
                continue;
            }
            self.sessions.set_in_flight(&tenant_id, &gateway_id, 1);
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 999_998,
                "method": "session/close",
                "params": {"sessionId": entry.backend_session_id.0}
            });
            let call = async {
                let backend = self.supervisor.ensure_running(&entry.agent_id).await?;
                let call_policy = self
                    .call_policy_for(entry.profile_name.as_deref(), &entry.agent_id)
                    .await;
                let mut backend = backend.lock().await;
                ensure_backend_initialized(&mut backend, call_policy.clone()).await?;
                backend.writer.lock().await.write_value(&request).await?;
                let (response, _, _) = read_matching_response(
                    &mut backend,
                    &serde_json::json!(999_998),
                    call_policy,
                    None,
                )
                .await?;
                if let Some(error) = response.get("error") {
                    return Err(RouterError::BackendSessionNewError(error.clone()));
                }
                Ok::<_, RouterError>(())
            };
            let result = match tokio::time::timeout(REAP_BACKEND_CALL_TIMEOUT, call).await {
                Ok(result) => result,
                Err(_) => {
                    // Dropping `call` here releases the per-process
                    // `BackendProcess` lock it may have acquired inside
                    // `ensure_backend_initialized`/`read_matching_response`,
                    // so a stuck backend can never hold this whole
                    // reaper (and the global router mutex the caller
                    // holds around it) hostage indefinitely. See
                    // `REAP_BACKEND_CALL_TIMEOUT`'s doc comment for the
                    // live incident this closes.
                    tracing::warn!(
                        gateway_session_id = %gateway_id.0,
                        agent_id = %entry.agent_id,
                        timeout_secs = REAP_BACKEND_CALL_TIMEOUT.as_secs(),
                        "lifecycle reaper's session/close timed out; leaving this session \
                         live and retrying on a later reap pass instead of blocking every \
                         other tenant/session behind the router lock"
                    );
                    Err(RouterError::ReapBackendCallTimeout(
                        gateway_id.0.clone(),
                        REAP_BACKEND_CALL_TIMEOUT,
                    ))
                }
            };
            if result.is_err() {
                self.sessions.set_in_flight(&tenant_id, &gateway_id, 0);
                report.failed += 1;
                continue;
            }
            if let Some(store) = self.persistence.clone() {
                if store
                    .close_session(gateway_id.0.clone(), now_rfc3339())
                    .await
                    .is_err()
                {
                    self.sessions.set_in_flight(&tenant_id, &gateway_id, 0);
                    report.failed += 1;
                    continue;
                }
            }
            if let Some(removed) = self.sessions.remove(&tenant_id, &gateway_id) {
                self.release_live_session(&tenant_id);
                self.stop_if_session_scoped(&removed.agent_id).await;
                self.mark_unreferenced_if_idle(&removed.agent_id);
                report.closed += 1;
            } else {
                report.skipped += 1;
            }
        }
        report
    }

    async fn restore_open_session(
        &mut self,
        record: &crate::persistence::SessionRecord,
    ) -> Result<
        (
            TenantId,
            crate::session_registry::SessionEntry,
            SessionAdmissionPermit,
        ),
        RouterError,
    > {
        let job = self.prepare_open_session_recovery(record).await?;
        let job = execute_open_session_recovery(job).await?;
        Ok((job.tenant_id, job.entry, job.admission))
    }

    async fn prepare_open_session_recovery(
        &mut self,
        record: &crate::persistence::SessionRecord,
    ) -> Result<PreparedRecoveryJob, RouterError> {
        let tenant_id = TenantId(record.tenant_id.clone());

        let mut params = record
            .recovery_params
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let params_object = params
            .as_object_mut()
            .ok_or_else(|| RouterError::InvalidRecoveryParams(record.gateway_session_id.clone()))?;
        params_object.insert(
            "sessionId".to_string(),
            serde_json::Value::String(record.backend_session_id.clone()),
        );
        if let Some(cwd) = &record.cwd {
            params_object.insert("cwd".to_string(), serde_json::Value::String(cwd.clone()));
        }

        let agent_id = if let Some(profile_name) = record.profile_name.as_deref() {
            self.resolve_profile(profile_name, &tenant_id).await?.0
        } else {
            record.agent_id.clone()
        };
        let admission = self.admit_session(&tenant_id)?;
        // **Startup-recovery agent registration fix.** Registry-backed
        // agent ids (e.g. a bridge session's concrete `codex-acp`, as
        // opposed to the one statically registered `default_agent_id` at
        // startup -- see `main.rs`) only ever get a `SpawnSpec` via this
        // same lazy call from a *live* bridge session/capability-probe
        // path (`acp_bridge.rs`'s model selection, `probe_adapter_capabilities`).
        // Startup recovery runs before any client has connected, so
        // without this call every persisted session whose `agent_id`
        // isn't `default_agent_id` failed deterministically, 100% of the
        // time, on every single restart, with
        // `no spawn spec registered for agent <id>` -- confirmed live via
        // `last_recovery_error` across 7 consecutive restarts, 0
        // successful recoveries ever. Idempotent/cheap when the spec is
        // already registered (`ensure_registry_agent_registered` itself
        // guards on `self.supervisor.spec(agent_id).is_some()` first).
        self.ensure_registry_agent_registered(&agent_id).await?;
        let backend = self.supervisor.ensure_running(&agent_id).await?;
        let call_policy = self
            .call_policy_for(record.profile_name.as_deref(), &agent_id)
            .await;
        let request_id = format!("acpx-startup-recovery:{}", record.gateway_session_id);
        let request_id_value = serde_json::Value::String(request_id.clone());
        let recovery_method = match record.recovery_method {
            RecoveryMethod::Load => "session/load",
            RecoveryMethod::Resume => "session/resume",
            RecoveryMethod::None => unreachable!("filtered before recovery"),
        };
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id_value,
            "method": recovery_method,
            "params": params,
        });

        Ok(PreparedRecoveryJob {
            tenant_id,
            entry: crate::session_registry::SessionEntry {
                agent_id,
                backend_session_id: BackendSessionId(record.backend_session_id.clone()),
                profile_name: record.profile_name.clone(),
                cwd: record.cwd.clone(),
                created_at: restore_lifecycle_instant(record.created_at_unix_nanos),
                last_activity_at: restore_lifecycle_instant(record.last_activity_at_unix_nanos),
                in_flight: 0,
                in_flight_since: None,
                pinned: record.pinned,
                custom_idle_ttl: record
                    .custom_idle_ttl_seconds
                    .map(|secs| std::time::Duration::from_secs(secs.max(0) as u64)),
            },
            admission,
            backend,
            call_policy,
            request,
            request_id: request_id_value,
        })
    }

    fn recovery_supervisor_key(&self, record: &crate::persistence::SessionRecord) -> String {
        let tenant_id = TenantId(record.tenant_id.clone());
        match record.profile_name.as_deref() {
            Some(profile_name) => self.profile_supervisor_key(profile_name, &tenant_id),
            None => record.agent_id.clone(),
        }
    }

    /// Ensure the registry cache is populated, fetching (live, falling
    /// back to the bundled snapshot on any error) if it hasn't been yet.
    /// See `acpx_registry::fetch_registry_or_fallback` -- this never
    /// itself fails, matching that function's contract.
    async fn ensure_registry_loaded(&mut self) -> &acpx_registry::Registry {
        if self.registry_cache.is_none() {
            self.registry_cache = Some(acpx_registry::fetch_registry_or_fallback(&self.http).await);
        }
        self.registry_cache.as_ref().expect("just populated")
    }

    /// Auto-seed one native (no `provider`/`key_ref`) [`Profile`] per
    /// ACP-registry agent this host can actually launch
    /// (`AgentStatus::Installed` per [`crate::detect::detect`]), named
    /// after the agent's registry id (`claude-acp`, `codex-acp`,
    /// `gemini`, ...) -- so `_acpx.profile` (and `profiles/list`) surface
    /// every backend the host already supports with zero
    /// `ACPX_CONFIG_FILE`/`profiles/create` setup required, instead of an
    /// empty `ProfileStore` until an operator explicitly provisions one.
    /// This closes the gap where `agents/list` could report `Installed`
    /// for claude/codex/gemini while `profiles/list` -- the thing
    /// `session/new`'s `_acpx.profile` actually resolves against --
    /// stayed empty regardless.
    ///
    /// An explicitly created/provisioned profile of the same name always
    /// wins (this only fills in names nobody has claimed yet -- see the
    /// `self.profiles.get(&agent.id).is_some()` skip below); a synthetic
    /// profile carries no provider/key, so it composes with an
    /// already-completed `codex login`/`claude login` on this host via
    /// plain ambient-env inheritance, exactly like `default_agent_id`'s
    /// native/unmanaged mode already does (see `crate::launch`'s doc
    /// comment).
    ///
    /// Idempotent and self-healing rather than a one-shot/cached flag:
    /// re-run on every `profiles/list` call and every `_acpx.profile`
    /// resolution. Cheap either way -- `ensure_registry_loaded` caches
    /// the registry fetch after the first call, and detection is just a
    /// handful of `<runtime> --version` subprocess spawns (three agents
    /// in the bundled fallback registry as of this writing) -- so an
    /// agent that becomes installed only after this process started
    /// (e.g. `node` added to `PATH` later) still gets picked up without
    /// a restart, with no separate cache-invalidation path to get wrong.
    async fn ensure_default_profiles_seeded(&mut self) {
        self.ensure_registry_loaded().await;
        let agents = self
            .registry_cache
            .as_ref()
            .expect("just loaded")
            .agents
            .clone();
        for agent in agents {
            if self.profiles.get(&agent.id).is_some() {
                continue;
            }
            if crate::detect::detect(&agent.id, &agent.distribution) != AgentStatus::Installed {
                continue;
            }
            let profile = Profile {
                name: agent.id.clone(),
                agent_id: agent.id.clone(),
                // Not `Profile::default()`'s `false`/`false`: every
                // registry-listed agent (`claude-acp`/`codex-acp`/`gemini`)
                // is a locally-spawned subprocess that already has
                // unrestricted host filesystem/process access on its own,
                // with or without ACPX's client-mediated `fs`/`terminal`
                // capability -- declaring `false` here buys these agents
                // no real isolation, it only breaks them. Verified against
                // real backends: a real `claude-agent-acp` process silently
                // stops using the ACP terminal/permission round trip at all
                // (executes Bash directly, no `session/request_permission`,
                // no `WaitingForConfirmation` a client could ever act on)
                // when `terminal` is declared `false`; a real `codex-acp`
                // process outright rejects `session/new` in its default
                // `agent` mode when the client can't back that mode's
                // terminal use. `Profile::default()`'s opt-in security
                // reasoning is still correct for a hand-authored profile
                // meant to sandbox an untrusted/remote backend -- it just
                // doesn't apply to this auto-seeded, install-detected,
                // always-local-subprocess case.
                allow_fs_access: true,
                allow_terminal_access: true,
                ..Profile::default()
            };
            // Best-effort: `create` only fails on a name collision, which
            // the `get`/skip check above already rules out for any path
            // that reaches here (single `&mut self` access, no
            // concurrent seeding possible) -- ignored rather than
            // `.expect()`-ed so a future concurrent-seeding change can't
            // turn a benign race into a panic.
            let _ = self.profiles.create(profile);
        }
    }

    /// Dispatch one JSON-RPC request, returning the JSON-RPC response to
    /// send back to the client that issued it.
    pub async fn dispatch(
        &mut self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        self.dispatch_for_tenant(&TenantId::default_tenant(), request)
            .await
    }

    /// **Phase B (`acpx-tenant-isolation`).** Tenant-aware entry point --
    /// the real dispatch logic, now scoped to `tenant_id`'s own
    /// `SessionRegistry` submap throughout. [`Self::dispatch`] is kept as
    /// a thin wrapper defaulting to [`TenantId::default_tenant`] so every
    /// pre-existing (tenant-unaware) caller -- most of this workspace's
    /// own test suite included -- keeps working byte-for-byte unchanged;
    /// only `acpx-server`'s transports, which actually extract a real
    /// `X-Acpx-Tenant` header, call this directly.
    pub async fn dispatch_for_tenant(
        &mut self,
        tenant_id: &TenantId,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or(RouterError::MissingMethod)?
            .to_string();
        match classify(&method) {
            MethodClass::Hybrid => self.dispatch_session_new(tenant_id, request).await,
            MethodClass::Proxied => self.dispatch_proxied(tenant_id, request).await,
            MethodClass::SessionFork => self.dispatch_session_fork(tenant_id, request).await,
            MethodClass::GatewayNative => self.dispatch_native(tenant_id, &method, request).await,
            MethodClass::Unknown => Err(RouterError::UnknownMethod(method)),
        }
    }

    async fn dispatch_session_new(
        &mut self,
        tenant_id: &TenantId,
        mut request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;

        // **Phase 13 addition.** Captured before any further mutation
        // below (the `_acpx` strip and `mcpServers` merge never touch
        // `cwd`) so it can be threaded into `SessionEntry::cwd` --
        // real per-backend `session/list`'s `SessionInfo.cwd` is a
        // *required* field, so acpx's own gateway-scoped aggregate needs
        // to actually know it to include it honestly.
        let cwd = params
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string);

        // Precedence per 02-architecture.md: an explicit `_acpx.profile`
        // selects managed mode (Phase 3: profile -> agent/provider/key
        // resolution, see `resolve_profile`); omitting it stays
        // native/unmanaged, using `default_agent_id`'s already-registered
        // spawn spec unchanged. Either way `_acpx` is stripped before
        // forwarding -- session/new stays a raw-ACP drop-in for a client
        // that never set it.
        let profile_name = params
            .get("_acpx")
            .and_then(|ext| ext.get("profile"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        let explicit_agent_id = params
            .get("_acpx")
            .and_then(|ext| ext.get("agentId"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        if profile_name.is_some() && explicit_agent_id.is_some() {
            return Err(RouterError::ConflictingSessionSelection);
        }
        let selected_agent_id = match (&profile_name, explicit_agent_id.as_deref()) {
            (Some(name), None) => {
                self.ensure_default_profiles_seeded().await;
                self.profiles
                    .get(name)
                    .map(|profile| profile.agent_id.clone())
                    .ok_or_else(|| RouterError::UnknownProfile(name.clone()))?
            }
            (None, Some(agent_id)) => agent_id.to_owned(),
            (None, None) => self.default_agent_id.clone(),
            (Some(_), Some(_)) => unreachable!("checked above"),
        };
        // Policy is checked before profile/connector resolution and before
        // capacity reservation, so a disabled definition cannot start a
        // process or consume a session slot.
        self.ensure_agent_enabled(&selected_agent_id).await?;
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }

        let (agent_id, profile) = match (&profile_name, explicit_agent_id) {
            (Some(name), None) => {
                let (supervisor_key, profile) = self.resolve_profile(name, tenant_id).await?;
                (supervisor_key, Some(profile))
            }
            (None, Some(agent_id)) => {
                self.ensure_custom_agent_registered(&agent_id).await?;
                (agent_id, None)
            }
            (None, None) => {
                let agent_id = self.default_agent_id.clone();
                self.ensure_custom_agent_registered(&agent_id).await?;
                (agent_id, None)
            }
            (Some(_), Some(_)) => unreachable!("checked before _acpx stripping"),
        };

        // **`backend_process_model` hardening, `acp-gateway-daemon` plan.**
        // Opt-in per-session backend process isolation: when enabled and
        // this is a *managed* session (a profile resolved above, native/
        // unmanaged mode is unaffected), fold a freshly-minted gateway id
        // into the already-resolved profile[/tenant] supervisor key so
        // this exact session gets its own dedicated backend process
        // instead of sharing the profile's one process with every other
        // session using it. The id has to be minted *now*, before
        // spawning, since it becomes part of the supervisor key itself;
        // `self.sessions.register_with_id` (below) then reuses this same
        // id rather than minting another one, so the gateway id the
        // client sees is identical either way. `":session:"` is a safe,
        // collision-free marker (a UUID's string form has no colons) --
        // see [`is_session_scoped_supervisor_key`] for where this
        // encoding is later decoded to know when it's safe to stop a
        // session's backend process on close, following the same
        // string-encoded-supervisor-key idiom `tenant_process_isolation`
        // already established (`profile_supervisor_key`/`stop_prefix`).
        let mut pre_minted_gateway_id: Option<String> = None;
        let agent_id = if self.session_process_isolation && profile.is_some() {
            let gid = uuid::Uuid::new_v4().to_string();
            let session_scoped_key = format!("{agent_id}:session:{gid}");
            if let Some(spec) = self.supervisor.spec(&agent_id).cloned() {
                self.supervisor.register(session_scoped_key.clone(), spec);
            }
            pre_minted_gateway_id = Some(gid);
            session_scoped_key
        } else {
            agent_id
        };

        // Merge the resolved profile's centrally-registered MCP servers
        // into whatever the client itself sent, client entries winning on
        // name collision -- see `crate::mcp_servers::merge_mcp_servers`.
        // A no-op (params untouched) when the profile has no attached
        // servers, so native mode and profiles with `mcp_servers: []`
        // never see an `mcpServers` field appear out of nowhere.
        if let Some(profile) = &profile {
            if !profile.mcp_servers.is_empty() {
                let central = self.mcp_servers.list_named(&profile.mcp_servers);
                let params = request
                    .get_mut("params")
                    .ok_or(RouterError::MissingParams)?;
                let client_servers = params
                    .get("mcpServers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let merged = crate::mcp_servers::merge_mcp_servers(&client_servers, &central);
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("mcpServers".to_string(), serde_json::json!(merged));
                }
            }
        }

        let admission = self.admit_session(tenant_id)?;
        let backend = self.supervisor.ensure_running(&agent_id).await?;
        // Native/unmanaged mode (no `_acpx.profile`, `profile` still `None`
        // here) still benefits from whatever profile
        // `ensure_default_profiles_seeded` auto-seeded under this exact
        // `agent_id` -- see that seeding's own doc comment for why. Only
        // borrowed for `call_policy`; `profile` itself (what gets persisted
        // as this session's `profile_name` below, and what the `mcpServers`
        // merge above already ran against) is untouched, so recovery/
        // `session/list` semantics for native-mode sessions don't change.
        // Doesn't call `ensure_default_profiles_seeded` itself -- see
        // `Router::warm_default_profiles`'s doc comment for why that must
        // happen once at startup, never inline here.
        let call_policy_profile = profile
            .clone()
            .or_else(|| self.profiles.get(&agent_id).cloned());
        let call_policy = self.call_policy(call_policy_profile.as_ref());
        let mut response = {
            let mut backend = backend.lock().await;
            ensure_backend_initialized(&mut backend, call_policy.clone()).await?;
            write_backend_value_locked(&mut backend, &request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response_with_idle_timeout(
                    &mut backend,
                    &id,
                    call_policy,
                    None,
                    SESSION_ESTABLISH_IDLE_READ_TIMEOUT,
                )
                .await?;
            attach_session_new_extras(
                response,
                notifications,
                agent_requests,
                backend.agent_capabilities.clone(),
            )
        };

        let backend_session_id = extract_backend_session_id(&response)?;
        // **`connector_reference_lifecycle`.** A freshly-registered
        // session references `agent_id`'s supervisor key again -- cancel
        // any pending idle-shutdown timer that started while it had zero
        // referencing sessions (a no-op when the feature is disabled or
        // no timer is pending). Cloned before the match below moves
        // `agent_id` into whichever registration branch runs.
        let supervisor_key_for_cancel = agent_id.clone();
        let gateway_id = match pre_minted_gateway_id {
            Some(gid) => self.sessions.register_with_id(
                tenant_id,
                acpx_proto::session::GatewaySessionId(gid),
                agent_id,
                BackendSessionId(backend_session_id),
                profile.as_ref().map(|p| p.name.clone()),
                cwd,
            ),
            None => self.sessions.register(
                tenant_id,
                agent_id,
                BackendSessionId(backend_session_id),
                profile.as_ref().map(|p| p.name.clone()),
                cwd,
            ),
        };
        self.cancel_unreferenced_shutdown(&supervisor_key_for_cancel);

        // Rewrite the backend's own session id into the gateway-issued one
        // before it ever reaches the client -- the client only ever sees
        // gateway session ids, never a raw backend id.
        let gateway_session_id_str = gateway_id.0.clone();
        if let Some(result) = response.get_mut("result") {
            result["sessionId"] = serde_json::Value::String(gateway_id.0);
        }
        let entry = self
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id_str.clone()),
            )
            .cloned()
            .expect("session was just registered");
        let effective_params = request
            .get("params")
            .cloned()
            .ok_or(RouterError::MissingParams)?;
        if let Err(error) = self
            .persist_session_recovery(tenant_id, &gateway_session_id_str, &entry, effective_params)
            .await
        {
            if let Some(removed) = self.sessions.remove(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id_str),
            ) {
                self.stop_if_session_scoped(&removed.agent_id).await;
                self.mark_unreferenced_if_idle(&removed.agent_id);
            }
            return Err(error);
        }
        admission.commit();
        self.spawn_transcript(
            gateway_session_id_str.clone(),
            Direction::ClientToAgent,
            request,
        );
        self.spawn_transcript(
            gateway_session_id_str,
            Direction::AgentToClient,
            response.clone(),
        );
        Ok(response)
    }

    /// **Phase 13 addition.** The real, spec-shaped half of dual-mode
    /// `session/list` (see `dispatch_native`'s `"session/list"` arm for
    /// the branching, and `session_list_selector`/`SessionListSelector`
    /// for how a client opts into this path). Resolves the requested
    /// backend exactly like `dispatch_session_new` does (`_acpx.profile`
    /// via `resolve_profile`, or a raw `_acpx.agentId` naming an
    /// already-registered supervisor key directly -- e.g. `default_agent_id`
    /// in native/unmanaged mode), forwards a real `session/list` request
    /// (params minus `_acpx`) to that one backend, and translates every
    /// returned `SessionInfo.sessionId` from the backend's own native id
    /// into a gateway id via `translate_or_register_backend_session` --
    /// without that translation step the response would hand the client
    /// ids it could never use against any other acpx method again,
    /// defeating the entire point of listing sessions through a proxy in
    /// the first place.
    async fn dispatch_session_list_real(
        &mut self,
        tenant_id: &TenantId,
        id: serde_json::Value,
        selector: SessionListSelector,
        mut params: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }
        let (agent_id, profile) = match selector {
            SessionListSelector::Profile(name) => {
                let (key, profile) = self.resolve_profile(&name, tenant_id).await?;
                (key, Some(profile))
            }
            SessionListSelector::AgentId(explicit_id) => {
                self.ensure_agent_enabled(&explicit_id).await?;
                self.ensure_custom_agent_registered(&explicit_id).await?;
                (explicit_id, None)
            }
        };
        let profile_name = profile.as_ref().map(|p| p.name.clone());
        let call_policy = self.call_policy(profile.as_ref());
        let backend = self.supervisor.ensure_running(&agent_id).await?;

        let outbound = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/list",
            "params": params,
        });

        let response = {
            let mut proc = backend.lock().await;
            ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
            write_backend_value_locked(&mut proc, &outbound).await?;
            let (response, _notifications, _agent_requests) =
                read_matching_response(&mut proc, &id, call_policy, None).await?;
            response
        };

        if let Some(error) = response.get("error") {
            return Err(RouterError::BackendSessionListError(error.clone()));
        }

        let mut result = response
            .get("result")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "sessions": [] }));
        if let Some(raw_sessions) = result.get("sessions").and_then(|s| s.as_array()) {
            let mut filtered = Vec::with_capacity(raw_sessions.len());
            for session in raw_sessions.iter().cloned() {
                let mut session = session;
                let Some(backend_sid) = session
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
                else {
                    filtered.push(session);
                    continue;
                };
                let session_cwd = session
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
                // **Phase B leak fix.** `None` means this exact backend
                // session is already owned by a *different* tenant (see
                // `Self::translate_or_register_backend_session`'s doc
                // comment) -- it is dropped from the response entirely,
                // not just left untranslated, so the requesting tenant
                // never learns the backend-native id or anything else
                // about a session it doesn't own.
                let Some(gateway_id) = self.translate_or_register_backend_session(
                    tenant_id,
                    &agent_id,
                    &backend_sid,
                    profile_name.clone(),
                    session_cwd,
                ) else {
                    continue;
                };
                session["sessionId"] = serde_json::Value::String(gateway_id.clone());
                self.spawn_session_persistence(
                    tenant_id,
                    gateway_id,
                    agent_id.clone(),
                    backend_sid,
                    profile_name.clone(),
                    outbound.clone(),
                    response.clone(),
                );
                filtered.push(session);
            }
            if let Some(obj) = result.as_object_mut() {
                obj.insert("sessions".to_string(), serde_json::json!(filtered));
            }
        }
        Ok(result)
    }

    /// See [`Self::dispatch_session_list_real`]'s doc comment and
    /// `SessionRegistry::find_by_backend`'s. Reuses an already-known
    /// gateway id for this exact `(agent_id, backend_session_id)` pair if
    /// one exists (e.g. a session acpx itself opened earlier in this
    /// process's lifetime via `session/new`); otherwise mints and
    /// registers a fresh one on the spot -- the same "recover a backend
    /// session into the live registry" move `rehydrate_session` makes for
    /// `session/load`, just triggered by discovery through `session/list`
    /// rather than an explicit client-supplied gateway id. From this
    /// point on the returned id is a first-class gateway session,
    /// `session/load`-able (and, once loaded, promptable) exactly like
    /// any other -- **the concrete, testable proof this isn't just a
    /// cosmetic id swap.**
    ///
    /// **Phase B (`acpx-tenant-isolation`) addition -- closes a real
    /// cross-tenant leak, corrected from this plan's original
    /// `01-architecture.md` draft during implementation (see that plan's
    /// updated text): a naive "never auto-register unless already known
    /// to *this* tenant" rule was found, against
    /// `session_list_real_test.rs`'s existing
    /// `session_list_with_a_selector_proxies_to_the_real_backend_and_
    /// translates_ids` test, to regress phase 13's own tested
    /// first-discovery behavior (a session created directly against a
    /// shared backend, never before seen by *any* tenant, must still be
    /// discoverable and usable -- that is the entire point of this
    /// function existing). The corrected rule: reuse an already-known id
    /// if *this* tenant already owns it; if some *other* tenant already
    /// owns this exact `(agent_id, backend_session_id)` pair
    /// ([`SessionRegistry::find_owner`]), refuse -- return `None`, never
    /// silently adopt someone else's session; only truly novel (nobody's)
    /// backend sessions get freshly registered, and always under the
    /// *requesting* tenant. `None` means "filter this entry out of the
    /// `session/list` response entirely" to the caller.
    fn translate_or_register_backend_session(
        &mut self,
        tenant_id: &TenantId,
        agent_id: &str,
        backend_session_id: &str,
        profile_name: Option<String>,
        cwd: Option<String>,
    ) -> Option<String> {
        if let Some(existing) =
            self.sessions
                .find_by_backend(tenant_id, agent_id, backend_session_id)
        {
            return Some(existing.0);
        }
        if let Some(owner) = self.sessions.find_owner(agent_id, backend_session_id) {
            if owner != tenant_id {
                return None;
            }
        }
        let admission = self.admit_session(tenant_id).ok()?;
        let gateway_id = self.sessions.register(
            tenant_id,
            agent_id.to_string(),
            BackendSessionId(backend_session_id.to_string()),
            profile_name,
            cwd,
        );
        admission.commit();
        Some(gateway_id.0)
    }

    /// Resolve `_acpx.profile` for `session/new`'s managed mode: look up
    /// the named `Profile`, resolve its `provider`/`key_ref` (if any) into
    /// concrete env vars via `crate::launch::build_launch_env`, and
    /// (re-)register a `SpawnSpec` for it under a per-profile supervisor
    /// key (`"profile:<name>"`, distinct from any native-mode agent id so
    /// the two never share -- or fight over -- one supervised process).
    /// Returns `(supervisor_key, profile)` for the caller to spawn/proxy
    /// against and to read `mcp_servers`/`name` off of.
    ///
    /// **Known Phase 3 gap** (see `05-open-risks.md`'s "one process per
    /// backend vs. one process per session" item): re-resolving an
    /// already-running profile's env here does *not* restart its
    /// supervised process -- `Supervisor::ensure_running` only spawns a
    /// fresh process when none is currently running under this key, so a
    /// `profiles/update` that changes a profile's provider/key only takes
    /// effect for the *next* profile that has no live process yet, not for
    /// an already-running one. Not fixed here; flagged, not silently
    /// wrong.
    async fn resolve_profile(
        &mut self,
        profile_name: &str,
        tenant_id: &TenantId,
    ) -> Result<(String, Profile), RouterError> {
        self.ensure_default_profiles_seeded().await;
        let profile = self
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| RouterError::UnknownProfile(profile_name.to_string()))?;

        let provider = match &profile.provider {
            Some(name) => Some(self.providers.get(name).cloned().ok_or_else(|| {
                RouterError::UnknownProviderRef {
                    profile: profile.name.clone(),
                    provider: name.clone(),
                }
            })?),
            None => None,
        };
        let resolved_key = match &profile.key_ref {
            Some(key_ref) => Some(self.keystore.resolve(key_ref)?.to_string()),
            None => None,
        };
        let env =
            crate::launch::build_launch_env(&profile, provider.as_ref(), resolved_key.as_deref());

        // Prefer an already-registered `SpawnSpec` for `profile.agent_id`
        // (e.g. the native default agent, or anything an operator/test
        // registered directly via `Router::register_agent`) as the base
        // to layer env onto -- only fall back to a fresh registry lookup
        // (building an `npx`/`uvx` `SpawnSpec` from scratch) when nothing
        // is registered under that id yet. This keeps profiles usable
        // against both registry-listed agents (the common case) and
        // manually-configured/non-registry backends, without forcing a
        // registry fetch on every `session/new` for the latter.
        self.ensure_agent_enabled(&profile.agent_id).await?;
        self.ensure_custom_agent_registered(&profile.agent_id)
            .await?;
        let mut spec = match self.supervisor.spec(&profile.agent_id).cloned() {
            Some(spec) => spec,
            None => {
                self.ensure_registry_loaded().await;
                let agent = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .iter()
                    .find(|a| a.id == profile.agent_id)
                    .cloned()
                    .ok_or_else(|| RouterError::UnknownAgentId(profile.agent_id.clone()))?;
                npx_spawn_spec(&agent).ok_or_else(|| RouterError::NoLaunchableDistribution {
                    profile: profile.name.clone(),
                    agent_id: profile.agent_id.clone(),
                })?
            }
        };
        // Overlay (not replace) so a manually-registered base spec's own
        // env (if any) survives for any var the profile doesn't itself
        // derive/override.
        for (key, value) in env {
            spec.env.insert(key, value);
        }

        let supervisor_key = self.profile_supervisor_key(&profile.name, tenant_id);
        self.supervisor.register(supervisor_key.clone(), spec);
        Ok((supervisor_key, profile))
    }

    fn profile_supervisor_key(&self, profile_name: &str, tenant_id: &TenantId) -> String {
        if self.tenant_process_isolation {
            format!("profile:{profile_name}:tenant:{}", tenant_id.0)
        } else {
            format!("profile:{profile_name}")
        }
    }

    /// **Phase 8 addition -- closes a real gap.** `session/load` (and its
    /// close cousin `session/resume`) exist in the real ACP spec
    /// specifically so a client can resume a session it learned about
    /// through some *other* channel than "I just called `session/new` in
    /// this exact process's lifetime" -- most obviously, reconnecting
    /// after the agent process (here, acpx itself) restarted and its
    /// in-memory `SessionRegistry` was wiped clean. Before this phase,
    /// every `Proxied` method -- `session/load` included -- required the
    /// gateway session id to already be a live key in that in-memory
    /// map, which made `session/load` in this gateway strictly *less*
    /// capable than in a real single-agent ACP agent: it could only ever
    /// re-request an already-open session, never genuinely resume one
    /// that outlived acpx's own process. That defeats the entire purpose
    /// of the method existing separately from `session/new`.
    ///
    /// This only fires as a fallback (the in-memory registry is always
    /// checked first, unchanged, by both call sites) and, as of the
    /// `acpx-session-transparent-revival` fix, for *every* `Proxied`/
    /// `SessionFork` method reachable through `dispatch_proxied`/
    /// `dispatch_proxied_shared`/`dispatch_session_fork(_shared)` --
    /// not just `session/load`/`session/resume`/`session/delete`/
    /// `session/fork`. Originally this was scoped to resumption-shaped
    /// methods only, on the theory that silently reviving a session on,
    /// say, an ordinary `session/prompt` would paper over a real client
    /// bug (a stale/typo'd session id) instead of surfacing it. Real
    /// production traffic disproved that: ACPX's own idle-TTL lifecycle
    /// reaper (`reap_expired_sessions`) evicts a session's in-memory
    /// entry -- while leaving its durable row alone, exactly like a
    /// restart -- the moment a client goes quiet for
    /// `session_idle_ttl_secs`, with no ACP-spec mechanism to push that
    /// invalidation to the client. A real, spec-only client (Zed) has no
    /// idea this happened and has no reason to re-issue `session/load`
    /// before its next ordinary `session/prompt` against a thread the
    /// user never closed -- so that first post-idle prompt hit
    /// `UnknownSession` even though the exact same durable row a
    /// `session/load` call would have happily revived was sitting right
    /// there. That is not a client bug being surfaced; it is a gap
    /// between ACPX's own idle-close policy and what a compliant client
    /// can reasonably be expected to do. A gateway session id that is
    /// merely *unknown* (never created, or belonging to another tenant)
    /// still fails exactly as before -- `get_session` below returns
    /// `None` regardless of `method`, so this widening only changes
    /// behavior for ids ACPX itself durably tracked.
    ///
    /// Requires `ACPX_DB_PATH`/`Router::with_persistence` to have been
    /// configured; without it there is nowhere durable to recover from,
    /// so this errors clearly (`SessionNotPersisted`) rather than
    /// silently behaving as if the session never existed at all vs.
    /// "recovery wasn't even possible here" -- the two are genuinely
    /// different failure modes worth distinguishing for whoever reads
    /// the error.
    async fn rehydrate_session(
        &mut self,
        tenant_id: &TenantId,
        method: &str,
        gateway_session_id: &str,
    ) -> Result<crate::session_registry::SessionEntry, RouterError> {
        if !matches!(classify(method), MethodClass::Proxied | MethodClass::SessionFork) {
            return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
        }
        if !self.on_demand_recovery_enabled {
            return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
        }
        let store = match self.persistence.clone() {
            Some(store) => store,
            None => {
                // Same implicit-vs-explicit distinction as the
                // `RecoveryFailed` gate below: only `session/load`/
                // `session/resume` get the specific, actionable
                // `SessionNotPersisted` diagnostic ("configure
                // ACPX_DB_PATH") -- that message only makes sense as a
                // reply to an explicit reload attempt. Every other
                // `Proxied`/`SessionFork` method (`session/prompt`,
                // `session/cancel`, ...) must fail with the same plain
                // `UnknownSession` a deployment with no persistence at
                // all has always returned for these methods, matching
                // this widening's own doc comment ("a gateway session id
                // that is merely unknown ... still fails exactly as
                // before"). Without this, a cross-tenant id guess against
                // a persistence-less deployment leaked a distinguishable
                // "not persisted" error instead of the generic "no
                // session registered" every other unknown-id case gets.
                if method == "session/load" || method == "session/resume" {
                    return Err(RouterError::SessionNotPersisted(
                        gateway_session_id.to_string(),
                    ));
                }
                return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
            }
        };
        let record = store
            .get_session(gateway_session_id.to_string())
            .await
            .map_err(|err| {
                RouterError::SessionRehydrationFailed(gateway_session_id.to_string(), err)
            })?
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.to_string()))?;
        // **Phase C (`acpx-tenant-isolation`).** A persisted row surviving
        // a daemon restart belongs to whichever tenant created it (see
        // `SessionRecord::tenant_id`'s doc comment) -- without this check
        // any tenant could rehydrate *any other* tenant's session purely
        // by guessing/reusing a gateway session id, completely bypassing
        // the in-memory `SessionRegistry` nesting Phase A/B built,
        // because `session/load` et al. only reach this path once the
        // in-memory registry has already missed. Deliberately returned as
        // the same `SessionNotPersisted` a genuinely-never-persisted id
        // would produce (not a distinct "forbidden" error) -- matching
        // `translate_or_register_backend_session`'s established rule
        // that a cross-tenant hit must never be distinguishable from a
        // cross-tenant miss.
        if record.tenant_id != tenant_id.0 {
            return Err(RouterError::SessionNotPersisted(
                gateway_session_id.to_string(),
            ));
        }
        if record.status == RecoveryStatus::Restoring {
            return Err(RouterError::SessionRestoring(
                gateway_session_id.to_string(),
            ));
        }
        // **Recovery-outage gate.** A record startup recovery (or a
        // previous on-demand attempt) already gave up on stays
        // unavailable to every *implicit* touch (`session/prompt`,
        // `session/cancel`, ...) -- those never carried an explicit
        // "please bring this back" intent, so they must fail exactly
        // like a genuinely-unknown session id would (`UnknownSession`,
        // "no session registered"), without retrying the connector at
        // all. Only `session/load`/`session/resume` -- the two methods
        // whose entire purpose is an explicit reload request -- are
        // allowed to retry a `RecoveryFailed` record here, so a caller
        // polling `session/load` after a connector outage clears can
        // still recover it (see `real_binary_survives_a_recovery_
        // connector_outage`). Without this gate, an ordinary
        // `session/prompt` right after a failed startup recovery would
        // silently attempt a fresh backend spawn and surface whatever
        // low-level connector error that spawn attempt produces (e.g. a
        // `Supervisor` crash-backoff message) instead of the same clean,
        // stable `UnknownSession` a client can already recognize.
        if record.status == RecoveryStatus::RecoveryFailed
            && method != "session/load"
            && method != "session/resume"
        {
            return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
        }
        let mut entry = crate::session_registry::SessionEntry {
            agent_id: record.agent_id,
            backend_session_id: BackendSessionId(record.backend_session_id),
            profile_name: record.profile_name,
            cwd: record.cwd,
            created_at: restore_lifecycle_instant(record.created_at_unix_nanos),
            last_activity_at: restore_lifecycle_instant(record.last_activity_at_unix_nanos),
            in_flight: 0,
            in_flight_since: None,
            pinned: record.pinned,
            custom_idle_ttl: record
                .custom_idle_ttl_seconds
                .map(|secs| std::time::Duration::from_secs(secs.max(0) as u64)),
        };
        // **The real, second half of this bug.** `entry.agent_id` here is
        // actually the *supervisor key* `profile:{name}` minted by
        // `resolve_profile`/`dispatch_session_new` at the time this
        // session was first created -- not a raw registry agent id. This
        // process's own `Supervisor` has never seen that key before (it
        // never ran the `session/new` that originally registered it), so
        // `ensure_running` would otherwise fail with "no spawn spec
        // registered for agent <key>" even though the session row itself
        // resolved correctly. Re-running `resolve_profile` (idempotent --
        // it just re-registers the same `SpawnSpec` under the same key,
        // exactly like every ordinary `session/new` call already does)
        // fixes that; a `None` `profile_name` (native/unmanaged mode)
        // needs no such step since `default_agent_id`'s spec is already
        // registered unconditionally at process startup. Caught by
        // `ambient_claude_session_load_survives_a_real_gateway_restart`
        // actually spawning a *second*, independent `acpx-server`
        // process -- an in-process-only test would never have exercised
        // a `Supervisor` that legitimately never saw this profile before.
        if let Some(name) = entry.profile_name.as_deref() {
            entry.agent_id = self.resolve_profile(name, tenant_id).await?.0;
        }
        let admission = self.admit_session(tenant_id)?;
        self.sessions.insert(
            tenant_id,
            acpx_proto::session::GatewaySessionId(gateway_session_id.to_string()),
            entry.clone(),
        );
        self.cancel_unreferenced_shutdown(&entry.agent_id);
        admission.commit();
        Ok(entry)
    }

/// Extracts and strips the optional `_acpx.bg` background-mode override
/// from `request.params` -- see `LifecycleConfig::background_mode`'s doc
/// comment for the full feature. Mirrors `transport::live::
/// take_resume_cursor`'s existing convention exactly: additive,
/// namespaced under `_acpx` (never part of the upstream ACP `session/
/// close` schema, so forwarding it verbatim would make a strict backend
/// choke on an unrecognized field), and surgically removed -- only the
/// `bg` key, not the whole `_acpx` object, so any other `_acpx.*` field
/// a caller also sent survives untouched. Accepts a JSON boolean or the
/// strings `"on"`/`"off"` (case-insensitive); anything else is treated
/// as absent (no override) rather than an error, matching `take_resume_
/// cursor`'s "malformed input is fresh state, not a hard failure"
/// precedent.
fn take_background_override(request: &mut serde_json::Value) -> Option<bool> {
    let params = request.get_mut("params")?.as_object_mut()?;
    let extension = params.get_mut("_acpx")?.as_object_mut()?;
    let override_value = extension.get("bg").and_then(|value| match value {
        serde_json::Value::Bool(flag) => Some(*flag),
        serde_json::Value::String(text) if text.eq_ignore_ascii_case("off") => Some(false),
        serde_json::Value::String(text) if text.eq_ignore_ascii_case("on") => Some(true),
        _ => None,
    });
    extension.remove("bg");
    if extension.is_empty() {
        params.remove("_acpx");
    }
    override_value
}

    /// **`background_mode` (bg-mode `session/close` override).** See
    /// `LifecycleConfig::background_mode`'s doc comment for the full
    /// feature. Returns `Ok(None)` when this call should proceed
    /// through the normal `session/close` path unchanged (background
    /// mode is off for this call, whether by deployment default or by
    /// an explicit `_acpx.bg` override); returns `Ok(Some(response))`
    /// when the caller should return that JSON-RPC success response
    /// immediately instead, having done nothing to the session. The
    /// session id is still validated -- including the same
    /// restart-survival `rehydrate_session` fallback every other
    /// `Proxied` method gets -- so closing an unknown session id is
    /// still a real error, not a silently-swallowed success.
    async fn maybe_suppress_close(
        &mut self,
        tenant_id: &TenantId,
        request: &mut serde_json::Value,
    ) -> Result<Option<serde_json::Value>, RouterError> {
        let override_bg = Router::take_background_override(request);
        if !override_bg.unwrap_or(self.lifecycle.background_mode) {
            return Ok(None);
        }
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let gateway_session_id = request
            .get("params")
            .and_then(|p| p.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingSessionId)?
            .to_string();
        if self
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
            )
            .is_none()
        {
            self.rehydrate_session(tenant_id, "session/close", &gateway_session_id)
                .await?;
        }
        Ok(Some(
            // `_acpx.backgroundClose` is an additive, ignorable response
            // marker (a real ACP client never checks it) -- but the
            // strict `/acp` bridge's `close_or_delete` (`acpx-server`'s
            // `transport::acp_bridge`) *does* check it, to distinguish
            // "the underlying gateway session is still alive" (this
            // path) from a genuinely forwarded close whose backend
            // response happens to also be `{}`. Without this, the
            // bridge would still unconditionally drop its own virtual
            // session-id mapping on any successful `session/close`
            // response, leaving a client that keeps using the same
            // session id after a suppressed close (exactly what
            // background mode exists to support) hitting "bridge
            // session not found" on its very next call -- found live,
            // not theoretical, running a real Zed-shaped WS round trip
            // against this feature.
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {},
                "_acpx": {"backgroundClose": true}
            }),
        ))
    }

    async fn dispatch_proxied(
        &mut self,
        tenant_id: &TenantId,
        mut request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or(RouterError::MissingMethod)?
            .to_string();
        // **Phase 7:** `session/cancel` is not shaped like every other
        // `Proxied` method -- see `Self::dispatch_session_cancel`'s doc
        // comment for the two real bugs this branch closes (a
        // spec-compliant client's notification-shaped call, with no
        // `id`, getting rejected by the generic `MissingId` check below;
        // and the generic path's blocking wait for a reply the backend
        // is never supposed to send). Must be checked before that `id`
        // extraction, not after.
        if method == "session/cancel" {
            return self.dispatch_session_cancel(tenant_id, request).await;
        }
        if method == "session/close" {
            if let Some(response) = self.maybe_suppress_close(tenant_id, &mut request).await? {
                return Ok(response);
            }
        }
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let gateway_session_id = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingSessionId)?
            .to_string();

        let entry = match self.sessions.resolve(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
        ) {
            Some(entry) => entry.clone(),
            None => {
                self.rehydrate_session(tenant_id, &method, &gateway_session_id)
                    .await?
            }
        };
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        let profile_name = entry.profile_name.clone();
        let call_policy = self
            .call_policy_for(profile_name.as_deref(), &agent_id)
            .await;

        // Rewrite gateway id -> backend id in place; everything else in
        // `params` is forwarded untouched, per the proxied-method contract
        // in 02-architecture.md.
        params["sessionId"] = serde_json::Value::String(backend_session_id);

        self.spawn_transcript(
            gateway_session_id.clone(),
            Direction::ClientToAgent,
            request.clone(),
        );

        let backend = self.supervisor.ensure_running(&agent_id).await?;
        let response = {
            let mut backend = backend.lock().await;
            ensure_backend_initialized(&mut backend, call_policy.clone()).await?;
            write_backend_value_locked(&mut backend, &request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response_with_idle_timeout(
                    &mut backend,
                    &id,
                    call_policy,
                    None,
                    session_establish_or_default_idle_timeout(&method),
                )
                .await?;
            attach_updates(response, notifications, agent_requests)
        };
        self.sessions.touch(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
        );
        if let Some(store) = self.persistence.clone() {
            store
                .update_session_activity(gateway_session_id.clone(), now_unix_nanos())
                .await?;
        }
        mark_successful_recovery_retry(self.persistence.clone(), &gateway_session_id, &method)
            .await?;
        self.spawn_transcript(
            gateway_session_id.clone(),
            Direction::AgentToClient,
            response.clone(),
        );
        if method == "session/close" {
            if let Some(store) = self.persistence.clone() {
                store
                    .close_session(gateway_session_id.clone(), now_rfc3339())
                    .await?;
            }
            // Evict the closed session from the in-memory registry --
            // **real bug fix**: this used to never happen, so every
            // session ever opened over a long-running daemon's lifetime
            // stayed in `SessionRegistry`'s `HashMap` forever (an
            // unbounded memory leak) and `session/list` kept reporting
            // closed sessions as still live indefinitely. `remove` already
            // existed on `SessionRegistry` but was never called from
            // anywhere in this file until now.
            // **Second real bug fix, found while auditing the
            // `connector_reference_lifecycle` gap:** this evicted the
            // `SessionRegistry` entry but never called
            // `stop_if_session_scoped` -- unlike the reap path just above
            // in this file, which does. Under `ACPX_SESSION_PROCESS_
            // ISOLATION`, an explicit client `session/close` (the common
            // case -- a well-behaved client closes when done, rather than
            // waiting for the idle reaper) leaked that session's
            // dedicated backend process forever, with no other code path
            // ever able to stop it again (its supervisor key is unique to
            // this one session, so no other session/reap ever revisits
            // it). A no-op for the default shared-process model.
            if let Some(removed) = self.sessions.remove(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
            ) {
                self.release_live_session(tenant_id);
                self.stop_if_session_scoped(&removed.agent_id).await;
                self.mark_unreferenced_if_idle(&removed.agent_id);
            }
        }
        Ok(response)
    }

    /// Real ACP `session/fork` -- see [`MethodClass::SessionFork`]'s doc
    /// comment for the full rationale (this method was entirely
    /// unclassified/unimplemented before that fix). Resolves the
    /// *source* session named by `params.sessionId` exactly like
    /// `dispatch_proxied` does (including `rehydrate_session` fallback
    /// for a source session that only survives in persistence), forwards
    /// the fork request to that same backend process with the gateway id
    /// rewritten to the backend-native one, then -- mirroring
    /// `dispatch_session_new`'s own "mint a fresh gateway id for
    /// whatever backend session id came back" handling -- registers the
    /// backend's newly-forked session id under a *brand new* gateway
    /// session id (same tenant/agent/profile as the source session,
    /// `cwd` taken from the fork request's own `params.cwd` since a
    /// forked session's working directory is independently specified,
    /// not necessarily inherited) and rewrites the response's
    /// `result.sessionId` accordingly before it ever reaches the client.
    async fn dispatch_session_fork(
        &mut self,
        tenant_id: &TenantId,
        mut request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let gateway_session_id = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingSessionId)?
            .to_string();
        // The *new* forked session's cwd, per `ForkSessionRequest`'s own
        // schema -- distinct from (and not inherited from) the source
        // session's cwd, so this is read off the fork request itself,
        // exactly like `dispatch_session_new` reads it off `session/
        // new`'s own params.
        let cwd = params
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string);

        // Reserve the fork before potentially rehydrating its persisted
        // source. If either source or fork cannot fit, rehydration remains
        // non-mutating rather than leaving a source-only live entry behind.
        let admission = self.admit_session(tenant_id)?;
        let entry = match self.sessions.resolve(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
        ) {
            Some(entry) => entry.clone(),
            None => {
                self.rehydrate_session(tenant_id, "session/fork", &gateway_session_id)
                    .await?
            }
        };
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        let profile_name = entry.profile_name.clone();
        let call_policy = self
            .call_policy_for(profile_name.as_deref(), &agent_id)
            .await;

        params["sessionId"] = serde_json::Value::String(backend_session_id);

        let backend = self.supervisor.ensure_running(&agent_id).await?;
        let mut response = {
            let mut backend = backend.lock().await;
            ensure_backend_initialized(&mut backend, call_policy.clone()).await?;
            write_backend_value_locked(&mut backend, &request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response(&mut backend, &id, call_policy, None).await?;
            attach_updates(response, notifications, agent_requests)
        };

        let forked_backend_session_id = extract_backend_session_id(&response)?;
        let forked_gateway_id = self.sessions.register(
            tenant_id,
            agent_id,
            BackendSessionId(forked_backend_session_id),
            profile_name.clone(),
            cwd,
        );
        admission.commit();
        let forked_gateway_session_id_str = forked_gateway_id.0.clone();
        if let Some(result) = response.get_mut("result") {
            result["sessionId"] = serde_json::Value::String(forked_gateway_id.0.clone());
        }

        // Persisted as the *new* forked session's own inaugural
        // transcript (client request that created it, agent's response
        // minting it) -- mirrors `dispatch_session_new`'s own persistence
        // exactly, since a fork is a fresh session from a persistence
        // point of view, just one whose backend process happens to
        // already be running (reused, not freshly spawned) and whose
        // conversation history the backend itself carried over. Nothing
        // is recorded against the *source* `gateway_session_id` here --
        // `session/fork` doesn't add a message to the source session's
        // own conversation.
        if let Some(entry) = self.sessions.resolve(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(forked_gateway_session_id_str.clone()),
        ) {
            self.spawn_session_persistence(
                tenant_id,
                forked_gateway_session_id_str,
                entry.agent_id.clone(),
                entry.backend_session_id.0.clone(),
                profile_name,
                request,
                response.clone(),
            );
        }
        Ok(response)
    }

    /// Real ACP `session/cancel` -- a client-sent *notification* per
    /// `agentclientprotocol.com`'s `CancelNotification` schema: no `id`,
    /// no reply expected on the wire for it directly. The agent's only
    /// observable reaction is that the already-in-flight `session/prompt`
    /// call it's meant to interrupt eventually resolves with
    /// `stopReason: "cancelled"` on its own -- that resolution flows back
    /// through the *original* `session/prompt` call's own already-running
    /// `dispatch_proxied`/`dispatch_proxied_shared` invocation, not
    /// through anything this method does.
    ///
    /// **Two real, previously-undiscovered bugs this closes** (found
    /// re-deriving the ACP spec surface for phase 7's recheck, not from a
    /// test failure -- this workspace had zero tests exercising
    /// `session/cancel` at all before this phase, despite it being one of
    /// four methods the spec calls out as a baseline MUST for every
    /// agent): (1) every other `Proxied` method unconditionally required
    /// an `id` (`RouterError::MissingId` otherwise) -- a spec-compliant
    /// client sending this as a true notification (no `id` at all) would
    /// have been rejected before the request ever reached a backend; (2)
    /// the generic proxied path blocks on `read_matching_response`
    /// waiting for a reply carrying the forwarded request's own id -- a
    /// spec-compliant backend never replies to `session/cancel` directly,
    /// so that would hang forever: a real deadlock against any
    /// correctly-implemented backend, not a hypothetical. Same category
    /// of bug as phase 2's `session/request_permission` fix, in the
    /// opposite direction: there, an agent-initiated *request* was
    /// mistaken for a notification; here, a client-sent *notification*
    /// was mistaken for a request awaiting a reply.
    ///
    /// **Third, deeper bug this closes -- the reason this isn't just a
    /// shape fix:** even with (1)/(2) fixed, routing this through the
    /// same per-process lock every other proxied method uses
    /// (`SharedBackendProcess`'s `Arc<Mutex<BackendProcess>>`) would
    /// still leave cancellation practically useless -- a `session/prompt`
    /// call already in flight against this exact backend process holds
    /// that lock for its *entire* duration (the whole point of the
    /// "real multi-agent concurrency" design), so a cancel routed through
    /// it could only ever be delivered *after* the very call it's meant
    /// to interrupt has already finished, at which point cancelling is
    /// moot. This writes through
    /// `acpx_conductor::supervisor::Supervisor::cancel_writer` instead --
    /// a handle independent of that per-process lock (see its and
    /// `BackendProcess::writer`'s doc comments) -- so the notification
    /// genuinely reaches the backend's stdin *while* the in-flight call
    /// is still blocked reading, not only after.
    ///
    /// Writes the real ACP notification shape verbatim
    /// (`{jsonrpc, method, params: {sessionId}}`, deliberately no `id`
    /// key at all, regardless of whatever shape the client's own call
    /// used) and returns immediately once that write succeeds, echoing
    /// the client's own `id` back (or `null` if the client sent a true
    /// notification) -- acpx's own client-facing transports are all
    /// request/response-shaped regardless of what ACP itself calls this
    /// method, so some reply is always sent, but nothing about it is a
    /// real backend acknowledgment (there isn't one to wait for).
    async fn dispatch_session_cancel(
        &mut self,
        tenant_id: &TenantId,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let client_id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let gateway_session_id = request
            .get("params")
            .and_then(|p| p.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingSessionId)?
            .to_string();
        let entry = self
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
            )
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.clone()))?;
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();

        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": backend_session_id }
        });
        self.spawn_transcript(
            gateway_session_id,
            Direction::ClientToAgent,
            notification.clone(),
        );

        // `None` means this agent's process was never spawned (or was
        // `stop`ped) -- nothing is in flight to cancel, a benign no-op
        // rather than an error: a client cancelling a session whose
        // backend isn't even running has, definitionally, nothing left
        // to interrupt.
        if let Some(writer) = self.supervisor.cancel_writer(&agent_id) {
            write_cancel_notification_best_effort(&writer, &notification).await;
        }

        Ok(serde_json::json!({ "jsonrpc": "2.0", "id": client_id, "result": {} }))
    }

    async fn dispatch_native(
        &mut self,
        tenant_id: &TenantId,
        method: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let result = match method {
            // **Phase 6:** acpx's own client-facing `initialize` --
            // distinct from `ensure_backend_initialized`'s backend-facing
            // handshake (that one negotiates with whatever process a
            // profile spawns; this one is acpx itself answering as the
            // ACP agent its clients think they're talking to). Real
            // schema defaults confirmed against agentclientprotocol.com/
            // protocol/schema: `agentCapabilities` defaults to
            // `{loadSession:false, mcpCapabilities:{http:false,sse:false},
            // promptCapabilities:{audio:false,embeddedContext:false,
            // image:false}}`, `authMethods` to `[]`. acpx declares the
            // permissive end of each flag instead of the spec's
            // conservative defaults: as a transparent multiplexing proxy,
            // acpx itself never inspects, transforms, or strips prompt
            // content blocks, `mcpServers` transport kinds, or
            // `session/load` calls -- it forwards every one of them
            // verbatim to whichever backend a later `session/new`
            // resolves to (see `classify`'s `Proxied`/`Hybrid` buckets),
            // so it imposes no restriction of its own to advertise here.
            // This is honestly *not* a promise that every backend a
            // client might later select via `_acpx.profile` actually
            // supports all of these -- that per-backend truth is only
            // knowable after `session/new`, and already surfaced there
            // via `_acpx.agentCapabilities` (phase 1). `authMethods`
            // stays the spec default `[]`: acpx-server's own access
            // control is transport-level (HTTP bearer token / WS auth,
            // enforced before a request ever reaches this dispatcher),
            // not an ACP-level `authenticate` exchange, so there is
            // genuinely no method id to advertise.
            "initialize" => serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {
                        "image": true,
                        "audio": true,
                        "embeddedContext": true
                    },
                    "mcpCapabilities": {
                        "http": true,
                        "sse": true
                    },
                    // **Phase 9 addition, `list` added phase 13.** Per
                    // the real v1 stable schema's `SessionCapabilities`,
                    // advertises `close`/`delete`/`resume`/`list` as
                    // supported -- honest, because all four are
                    // genuinely forwarded to whatever real backend a
                    // caller selects (see `classify` for `close`/
                    // `delete`/`resume`, and `dispatch_native`'s
                    // `"session/list"` arm plus `session_list_selector`
                    // for `list`'s dual-mode split). `list` specifically:
                    // phase 9 through 12 deliberately omitted it because
                    // acpx's own `session/list` answered *only* from its
                    // gateway-scoped `SessionRegistry` (no `cwd`, no
                    // per-backend `SessionInfo[]` shape at all) -- a
                    // genuine, tracked divergence from the real schema.
                    // Phase 13 closed that: `session/list` now forwards
                    // to a real backend's own `session/list` (translating
                    // returned session ids into gateway ids so they stay
                    // usable through acpx afterward) whenever the caller
                    // supplies the same `_acpx` backend-selector
                    // convention `session/new`'s `_acpx.profile` already
                    // established; an unqualified call keeps answering
                    // the gateway-wide aggregate instead (no single real
                    // backend could ever honestly answer *that* question
                    // -- it's the entire reason a multiplexing gateway
                    // exists), which is why this capability flag is an
                    // honest "can be spec-conformant, not that every
                    // unqualified call is" the same way `loadSession`/
                    // `promptCapabilities` above already are for their
                    // own per-backend caveats. `additionalDirectories` is
                    // still omitted: acpx forwards whatever a client
                    // sends verbatim but never itself inspects/validates
                    // that field, so there's no acpx-level claim to make
                    // about it either way.
                    "sessionCapabilities": {
                        "close": {},
                        "delete": {},
                        "resume": {},
                        "list": {},
                        // **ACP compatibility gap closed post-review.**
                        // `session/fork` is now genuinely forwarded to
                        // whatever backend a caller selects (see
                        // `MethodClass::SessionFork`'s doc comment) --
                        // same honesty rule as `close`/`delete`/
                        // `resume`/`list` above -- so it belongs here
                        // too, not left implicitly unsupported the way
                        // it silently was before that fix. A
                        // spec-compliant client checks this capability
                        // before ever calling `session/fork`, so leaving
                        // it unset would have meant no compliant client
                        // could discover acpx supports it even though
                        // the dispatch path itself works correctly.
                        "fork": {}
                    }
                },
                "authMethods": [],
                "agentInfo": {
                    "name": "acpx",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            // **Phase 6:** a compliant client only ever calls
            // `authenticate` in response to a non-empty `authMethods` in
            // `initialize`'s result -- acpx's own `initialize` (just
            // above) always advertises `[]`, so any `authenticate` call
            // acpx receives here is, by definition, requesting a method
            // id that was never offered. Reply with a clear, specific
            // error rather than either silently succeeding (which would
            // misrepresent that some real authentication happened) or a
            // bare method-not-found (which would misrepresent
            // `authenticate` itself as unsupported, when it's the
            // *methodId* that's the problem).
            "authenticate" => {
                let method_id = request
                    .get("params")
                    .and_then(|p| p.get("methodId"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                return Err(RouterError::NoAuthMethodsAdvertised(method_id));
            }
            // **Phase 9 addition**, same reasoning as `authenticate`
            // just above: acpx's own `initialize` response deliberately
            // never sets `agentCapabilities.auth.logout` (omitted
            // entirely, meaning "not supported" per the real schema's
            // own stated default), because acpx-server's own access
            // control is transport-level (HTTP bearer token / WS auth)
            // and there is no ACP-level authenticated state at the
            // *gateway* layer for a client-facing `logout` to
            // meaningfully terminate -- forwarding it to some arbitrary
            // one backend among potentially many active profiles would
            // be actively misleading (which one?), and silently
            // succeeding as a no-op would misrepresent that something
            // real happened. A compliant client checks the capability
            // before calling; one that calls anyway gets a clear,
            // specific error instead of a bare method-not-found.
            "logout" => {
                return Err(RouterError::LogoutNotSupported);
            }
            // **Phase 13.** `session/list` is dual-mode, distinguished by
            // whether the client supplies a backend selector via the
            // established `_acpx` extension convention (same one
            // `session/new` already uses for `_acpx.profile`): with a
            // selector (`_acpx.profile` or `_acpx.agentId`), this is a
            // real, spec-shaped `Proxied` forward to that one specific
            // backend's own `session/list` (see
            // `Self::dispatch_session_list_real`); without one, it stays
            // acpx's original gateway-scoped aggregate view across every
            // backend this process manages -- the reason a multiplexing
            // gateway is worth having in the first place, and something
            // no single real backend's `session/list` could ever answer
            // on its own. See `COVERAGE.md`'s phase 13 entry for the
            // full rationale on why this is a split, not a rename or a
            // one-or-the-other tradeoff.
            "session/list" => {
                let params = request
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                match session_list_selector(&params) {
                    Some(selector) => {
                        self.dispatch_session_list_real(tenant_id, id.clone(), selector, params)
                            .await?
                    }
                    None => {
                        let sessions: Vec<serde_json::Value> = self
                            .sessions
                            .list(tenant_id)
                            .map(|(gateway_id, entry)| {
                                serde_json::json!({
                                    "sessionId": gateway_id,
                                    "agentId": entry.agent_id,
                                    "cwd": entry.cwd,
                                })
                            })
                            .collect();
                        serde_json::json!({ "sessions": sessions })
                    }
                }
            }
            "agents/list" => {
                self.ensure_registry_loaded().await;
                let agents = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .clone();
                let mut entries: Vec<serde_json::Value> = Vec::with_capacity(agents.len());
                for agent in agents {
                    let enabled = match &self.agent_enablement {
                        Some(enablement) => enablement.is_enabled(&agent.id).await?,
                        None => true,
                    };
                    entries.push(serde_json::json!({
                        "id": agent.id,
                        "name": agent.name,
                        "version": agent.version,
                        "status": crate::detect::detect(&agent.id, &agent.distribution),
                        "enabled": enabled,
                        "source": AgentSource::Registry,
                    }));
                }
                if let Some(custom_agents) = &self.custom_agents {
                    for agent in custom_agents.list().await? {
                        let enabled = match &self.agent_enablement {
                            Some(enablement) => enablement.is_enabled(&agent.id).await?,
                            None => true,
                        };
                        entries.push(serde_json::json!({
                            "id": agent.id,
                            "name": agent.name,
                            "version": "custom",
                            "status": AgentStatus::Configured,
                            "enabled": enabled,
                            "source": AgentSource::Custom,
                        }));
                    }
                }
                serde_json::json!({ "agents": entries })
            }
            "agents/status" => {
                let agent_id = request
                    .get("params")
                    .and_then(|p| p.get("id"))
                    .and_then(|i| i.as_str())
                    .ok_or(RouterError::MissingParams)?
                    .to_string();
                if let Some(custom_agents) = &self.custom_agents {
                    if custom_agents.get(&agent_id).await?.is_some() {
                        return Ok(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"id": agent_id, "status": AgentStatus::Configured}
                        }));
                    }
                }
                self.ensure_registry_loaded().await;
                let agent = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .iter()
                    .find(|a| a.id == agent_id)
                    .cloned()
                    .ok_or(RouterError::UnknownAgentId(agent_id))?;
                let status = crate::detect::detect(&agent.id, &agent.distribution);
                serde_json::json!({ "id": agent.id, "status": status })
            }
            "agents/install" => {
                let agent_id = request
                    .get("params")
                    .and_then(|p| p.get("id"))
                    .and_then(|i| i.as_str())
                    .ok_or(RouterError::MissingAgentId)?
                    .to_string();
                self.ensure_registry_loaded().await;
                let agent = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .iter()
                    .find(|a| a.id == agent_id)
                    .cloned()
                    .ok_or(RouterError::UnknownAgentId(agent_id))?;
                let outcome = acpx_registry::install(&agent).await?;
                serde_json::json!({ "id": agent.id, "outcome": format!("{outcome:?}") })
            }
            "profiles/create" | "profiles/update" => {
                let params = request
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let mut profile: Profile = serde_json::from_value(params.clone())
                    .map_err(|_| RouterError::MissingParams)?;
                // A raw `secret` field (never itself echoed back, see below)
                // is stored via `Keystore::store` and the resulting
                // opaque `KeyRef` wins over any `key_ref` the caller sent
                // directly -- `profiles/create`/`update` is the only entry
                // point for getting a secret into the keystore at all
                // (Phase 3 scoped no separate `keys/*` JSON-RPC namespace,
                // see `04-phased-plan.md` step 13/14).
                if let Some(secret) = params.get("secret").and_then(|s| s.as_str()) {
                    profile.key_ref = Some(self.keystore.store(secret));
                    if let Some(key_ref) = profile.key_ref.clone() {
                        self.persist_secret_if_durable(&key_ref, secret).await?;
                    }
                }
                if method == "profiles/create" {
                    self.profiles.create(profile.clone())?;
                } else {
                    self.profiles.update(profile.clone())?;
                }
                self.persist_profile_if_durable(&profile).await?;
                redact_launch_overrides(
                    serde_json::to_value(&profile).expect("Profile always serializes"),
                )
            }
            "profiles/list" => {
                self.ensure_default_profiles_seeded().await;
                let profiles: Vec<serde_json::Value> = self
                    .profiles
                    .list()
                    .map(|p| {
                        redact_launch_overrides(
                            serde_json::to_value(p).expect("Profile always serializes"),
                        )
                    })
                    .collect();
                serde_json::json!({ "profiles": profiles })
            }
            "profiles/delete" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .ok_or(RouterError::MissingParams)?
                    .to_string();
                self.profiles.delete(&name)?;
                self.delete_persisted_profile_if_durable(&name).await?;
                // **Real bug fix**: this used to only remove the
                // `ProfileStore` entry, leaving whatever backend process
                // was spawned for it (under supervisor key
                // `"profile:<name>"`, see `resolve_profile`) running
                // forever with no way to ever stop it again -- an orphaned
                // OS child process leaked on every `profiles/delete` call
                // against a profile that had ever actually been used in a
                // `session/new`. Best-effort: `Supervisor::stop` is a
                // no-op (not an error) if no process was ever spawned
                // under this key, so this is safe to call unconditionally
                // regardless of whether the profile was ever used.
                let supervisor_key = format!("profile:{name}");
                if let Err(err) = self.supervisor.stop(&supervisor_key).await {
                    tracing::warn!(%err, profile = %name, "failed to stop profile's backend process on delete");
                }
                let tenant_prefix = format!("{supervisor_key}:tenant:");
                if let Err(err) = self.supervisor.stop_prefix(&tenant_prefix).await {
                    tracing::warn!(%err, profile = %name, "failed to stop tenant-isolated profile backend processes on delete");
                }
                serde_json::json!({ "name": name, "deleted": true })
            }
            "mcp_servers/create" | "mcp_servers/update" => {
                let entry = request
                    .get("params")
                    .cloned()
                    .ok_or(RouterError::MissingParams)?;
                if method == "mcp_servers/create" {
                    self.mcp_servers.create(entry.clone())?;
                } else {
                    self.mcp_servers.update(entry.clone())?;
                }
                if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                    self.persist_mcp_server_if_durable(name, &entry).await?;
                }
                entry
            }
            "mcp_servers/list" => {
                serde_json::json!({ "servers": self.mcp_servers.list() })
            }
            "mcp_servers/delete" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .ok_or(RouterError::MissingParams)?
                    .to_string();
                self.mcp_servers.delete(&name)?;
                self.delete_persisted_mcp_server_if_durable(&name).await?;
                serde_json::json!({ "name": name, "deleted": true })
            }
            "session/retention/get" => {
                let gateway_session_id = request
                    .get("params")
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|s| s.as_str())
                    .ok_or(RouterError::MissingSessionId)?
                    .to_string();
                self.retention_info_json(tenant_id, &gateway_session_id)?
            }
            "session/retention/list" => {
                let sessions: Vec<serde_json::Value> = self
                    .sessions
                    .list_for_tenant(tenant_id)
                    .into_iter()
                    .map(|(gateway_id, entry)| retention_entry_json(&gateway_id.0, &entry))
                    .collect();
                serde_json::json!({ "sessions": sessions })
            }
            "session/retention/pin" => {
                let gateway_session_id = request
                    .get("params")
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|s| s.as_str())
                    .ok_or(RouterError::MissingSessionId)?
                    .to_string();
                self.set_session_pinned_administered(tenant_id, &gateway_session_id, true)
                    .await?;
                tracing::info!(
                    tenant_id = %tenant_id.0,
                    gateway_session_id = %gateway_session_id,
                    "session pinned via session/retention/pin"
                );
                self.retention_info_json(tenant_id, &gateway_session_id)?
            }
            "session/retention/unpin" => {
                let gateway_session_id = request
                    .get("params")
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|s| s.as_str())
                    .ok_or(RouterError::MissingSessionId)?
                    .to_string();
                self.set_session_pinned_administered(tenant_id, &gateway_session_id, false)
                    .await?;
                tracing::info!(
                    tenant_id = %tenant_id.0,
                    gateway_session_id = %gateway_session_id,
                    "session unpinned via session/retention/unpin"
                );
                self.retention_info_json(tenant_id, &gateway_session_id)?
            }
            "session/retention/set_ttl" => {
                let params = request
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let gateway_session_id = params
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .ok_or(RouterError::MissingSessionId)?
                    .to_string();
                // `null`/absent clears the override; a present number
                // sets it -- distinguished via `Option<Option<u64>>`-
                // shaped parsing so "field omitted" and "field explicitly
                // null" both mean "clear", matching every other optional
                // JSON-RPC param in this dispatcher.
                let idle_ttl_seconds = params.get("idleTtlSeconds").and_then(|v| v.as_u64());
                let ttl = idle_ttl_seconds.map(std::time::Duration::from_secs);
                self.set_session_custom_ttl(tenant_id, &gateway_session_id, ttl)
                    .await?;
                tracing::info!(
                    tenant_id = %tenant_id.0,
                    gateway_session_id = %gateway_session_id,
                    idle_ttl_seconds = ?idle_ttl_seconds,
                    "session idle TTL override changed via session/retention/set_ttl"
                );
                self.retention_info_json(tenant_id, &gateway_session_id)?
            }
            other => return Err(RouterError::UnknownMethod(other.to_string())),
        };
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }
}

/// Read backend messages until one whose `id` matches the request we just
/// sent, collecting every unmatched message seen along the way (almost
/// always `session/update` notifications -- an agent-initiated request
/// with no matching id is vanishingly rare and, if it ever happens, gets
/// collected the same way rather than silently dropped either).
///
/// **Reverse-direction routing (closes the former "Known Phase 2 gap",
/// see `acpx/COVERAGE.md`'s "real ACP content delivery" section for the
/// full story of why this mattered in practice):** real ACP agents (every
/// adapter checked against a real published npx package, not just the
/// synthetic stand-ins used elsewhere in this workspace's tests) deliver
/// the actual assistant reply text via `session/update`
/// `agent_message_chunk` notifications streamed *during* a `session/
/// prompt` call -- the call's own JSON-RPC result is just `{stopReason,
/// usage}`, with no message content at all. Silently dropping every
/// notification (this function's pre-fix behavior) meant a client talking
/// to a real adapter through acpx got back a result with no actual answer
/// in it, ever -- a correctness bug serious enough that it made "acpx
/// client working end to end against a real backend" false regardless of
/// anything else in this gateway working correctly.
///
/// The fix returns every collected notification alongside the matched
/// response; every caller below folds them into the JSON-RPC envelope's
/// `_acpx.updates` field (additive, namespaced, ignorable by any raw ACP
/// client that doesn't know about it) rather than a true live push --
/// see `dispatch_proxied`/`dispatch_proxied_shared`'s doc comments for why
/// aggregation-into-the-response is the right fit for this gateway's
/// request/response-shaped transports (HTTP chief among them) rather than
/// a separate out-of-band push channel.
/// Fixed request id for the one-time ACP `initialize` handshake performed
/// against a backend process the first time it's used (see
/// [`ensure_backend_initialized`]'s doc comment). Numeric (not a string
/// id) deliberately -- this workspace's synthetic `sh -c '...'` stand-in
/// backends (used by roughly a dozen pre-existing tests) extract the
/// request id with a numeric-only shell regex (`grep -o '"id":[0-9]*'`)
/// and echo it back verbatim; a string id would make every one of those
/// scripts emit malformed JSON (`"id":`) in reply. Never collides with a
/// real client's own request id in practice: this handshake always
/// completes (or fails the whole dispatch) before the actual client
/// request is ever written to the same backend.
const INITIALIZE_REQUEST_ID: i64 = 0;

/// Fixed request id for the ACP `authenticate` round trip performed
/// against a backend process that advertised a non-empty `authMethods`
/// in its `initialize` response -- same rationale as
/// `INITIALIZE_REQUEST_ID` (numeric, for the synthetic `sh -c '...'`
/// stand-in backends' regex-based id echo), distinct value so a test
/// double can tell the two handshake requests apart if it needs to.
const AUTHENTICATE_REQUEST_ID: i64 = -1;

/// Everything about how `read_matching_response`/`ensure_backend_initialized`
/// should answer a backend's mid-call agent-initiated requests on a given
/// call's behalf, bundled so the growing set of "what is this profile
/// allowed to auto-decide" knobs doesn't turn into an ever-longer
/// parameter list at every one of the four dispatch call sites. Computed
/// once per call from the resolved `Profile` (or defaulted for
/// native/unmanaged mode, where there is no profile to consult at all).
#[derive(Debug, Clone, Default)]
struct BackendCallPolicy {
    permission_policy: PermissionPolicy,
    allow_fs_access: bool,
    allow_terminal_access: bool,
    auth_method_id: Option<String>,
}

impl BackendCallPolicy {
    fn from_profile(profile: Option<&Profile>) -> Self {
        match profile {
            Some(p) => Self {
                permission_policy: p.permission_policy,
                allow_fs_access: p.allow_fs_access,
                allow_terminal_access: p.allow_terminal_access,
                auth_method_id: p.auth_method_id.clone(),
            },
            None => Self::default(),
        }
    }
}

/// Perform the real ACP `initialize` request/response round trip against
/// `proc` if it hasn't already happened for this exact process instance
/// (`BackendProcess::handshake_done`, reset to `false` on every fresh
/// spawn -- see that field's doc comment).
///
/// **Real bug this fixes** (found driving `real_claude_multi_agent_test.rs`
/// against a real, published `@agentclientprotocol/claude-agent-acp`
/// adapter, not a synthetic stand-in): every dispatch path wrote
/// `session/new` as the very first message on a freshly spawned backend's
/// stdio, with no ACP `initialize` handshake ever performed first. This
/// workspace's ~120 pre-existing tests never caught it because their
/// synthetic `sh -c '...'` stand-in backends answer *any* request
/// uniformly regardless of ordering. A real adapter does not: verified
/// against claude-agent-acp, it silently omits `result.sessionId` from
/// its `session/new` response if `initialize` was never called first,
/// which acpx surfaced as an opaque `RouterError::MissingBackendSessionId`
/// with no indication the real problem was protocol ordering, not the
/// request itself.
/// **Phase 5 addition:** after the `initialize` handshake (whether
/// performed just now or previously, per `handshake_done`), also check
/// the backend's cached `authMethods` (from `proc.agent_capabilities`,
/// so this never re-sends `initialize` on a retry) and drive a real ACP
/// `authenticate` round trip if the backend requires one and hasn't
/// already succeeded (`proc.authenticated`). No pre-configured
/// `Profile::auth_method_id` -- or a backend that rejects the one
/// configured -- surfaces as a clear [`RouterError`] instead of acpx
/// either guessing a method id or silently proceeding to `session/new`
/// and letting the backend's own downstream rejection stand in for a
/// real error message about *why*.
/// Hard ceiling on writing one JSON-RPC line to a backend process's stdin
/// pipe.
///
/// **Why this exists.** Every dispatch path either holds the per-process
/// `BackendProcess` lock across its `write_value` call (non-demux path,
/// and every `process_reader_demux` path's `if proc.pending.is_none()`
/// setup block) or serializes on the shared `writer` handle alone (the
/// demux "write-then-register" fast path). Neither was ever bounded: a
/// backend that stops draining its own stdin (wedged, deadlocked, or
/// swapped/suspended) leaves `ChildStdin::write_all`'s kernel-pipe-buffer
/// wait blocking forever, and with it every other session sharing this
/// exact process -- the same "one stuck backend freezes the whole
/// gateway" class of incident `BACKEND_IDLE_READ_TIMEOUT`/
/// `BACKEND_HANDSHAKE_TIMEOUT`/`CANCEL_WRITE_TIMEOUT` already close on
/// the read side and the best-effort-cancel side. Same reasoning as
/// `CANCEL_WRITE_TIMEOUT`'s doc comment: writing a few hundred bytes to a
/// healthy process's stdin pipe completes in microseconds, so a multi-
/// second bound is generous, not aggressive.
const BACKEND_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Writes one JSON-RPC value onto `proc`'s stdin, bounded by
/// `BACKEND_WRITE_TIMEOUT`. `proc` is already exclusively held by the
/// caller (a `BackendProcess` guard or `&mut` reference), so on timeout
/// this kills it directly and returns `RouterError::BackendWriteTimeout`
/// instead of leaving every other session sharing `proc`'s lock queued
/// up behind a wedged stdin pipe forever.
async fn write_backend_value_locked(
    proc: &mut acpx_conductor::BackendProcess,
    value: &serde_json::Value,
) -> Result<(), RouterError> {
    let writer = proc.writer_handle();
    let write = async move { writer.lock().await.write_value(value).await };
    match tokio::time::timeout(BACKEND_WRITE_TIMEOUT, write).await {
        Ok(result) => result.map_err(RouterError::from),
        Err(_) => {
            tracing::warn!(
                timeout_secs = BACKEND_WRITE_TIMEOUT.as_secs(),
                "backend stdin write timed out; killing the wedged process so every other \
                 session sharing its lock isn't blocked forever"
            );
            let _ = proc.kill().await;
            Err(RouterError::BackendWriteTimeout(BACKEND_WRITE_TIMEOUT))
        }
    }
}

/// Same bound as [`write_backend_value_locked`], for the
/// `process_reader_demux` fast path where the per-process lock (`proc`)
/// was already dropped in favor of a standalone `writer` handle before
/// this write -- exactly the shape `Router::dispatch_session_list_real`
/// and friends use once they register a pending-response slot. Since
/// `proc` isn't held here, killing on timeout re-acquires `backend`'s
/// own lock first (a wedged stdin pipe means the process is already
/// unusable, so a short re-lock to kill it is safe and cannot itself
/// deadlock: nothing else holds `backend` for longer than one register/
/// write/read step in this codebase).
async fn write_backend_value_via_handle(
    backend: &acpx_conductor::supervisor::SharedBackendProcess,
    writer: &std::sync::Arc<tokio::sync::Mutex<acpx_conductor::framing::FramedWriter>>,
    value: &serde_json::Value,
) -> Result<(), RouterError> {
    let write = async { writer.lock().await.write_value(value).await };
    match tokio::time::timeout(BACKEND_WRITE_TIMEOUT, write).await {
        Ok(result) => result.map_err(RouterError::from),
        Err(_) => {
            tracing::warn!(
                timeout_secs = BACKEND_WRITE_TIMEOUT.as_secs(),
                "backend stdin write timed out (process-reader-demux path); killing the \
                 wedged process so every other session sharing it is unblocked"
            );
            let mut proc = backend.lock().await;
            let _ = proc.kill().await;
            Err(RouterError::BackendWriteTimeout(BACKEND_WRITE_TIMEOUT))
        }
    }
}

/// Best-effort variant for the standalone `Supervisor::cancel_writer`
/// pipe (`session/cancel` notifications), which has no `BackendProcess`
/// handle to kill on timeout -- mirrors `Router::cancel_stuck_turns`'s
/// existing `CANCEL_WRITE_TIMEOUT` handling exactly: `session/cancel` is
/// fire-and-forget with no confirmation in the ACP wire contract, so
/// timing out here logs and moves on rather than failing the whole
/// dispatch (a stuck cancel write shouldn't turn into a stuck cancel
/// *request*).
async fn write_cancel_notification_best_effort(
    writer: &std::sync::Arc<tokio::sync::Mutex<acpx_conductor::framing::FramedWriter>>,
    value: &serde_json::Value,
) {
    let write = async { writer.lock().await.write_value(value).await };
    match tokio::time::timeout(CANCEL_WRITE_TIMEOUT, write).await {
        Ok(Err(error)) => {
            tracing::warn!(%error, "failed to deliver best-effort session/cancel");
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = CANCEL_WRITE_TIMEOUT.as_secs(),
                "best-effort session/cancel write timed out; skipping rather than blocking \
                 this dispatch on a wedged stdin pipe"
            );
        }
        Ok(Ok(())) => {}
    }
}

async fn ensure_backend_initialized(
    proc: &mut acpx_conductor::BackendProcess,
    call_policy: BackendCallPolicy,
) -> Result<(), RouterError> {
    ensure_backend_initialized_with_handshake_timeout(proc, call_policy, BACKEND_HANDSHAKE_TIMEOUT)
        .await
}

/// [`ensure_backend_initialized`]'s real body, parameterized on the
/// handshake timeout so a unit test can exercise the kill-on-expiry path
/// in milliseconds instead of waiting out the real 30-second production
/// value -- same pattern as [`read_matching_response`]/[`read_matching_
/// response_with_idle_timeout`] and `acp_bridge::refresh_models`/
/// `refresh_models_with_config`.
async fn ensure_backend_initialized_with_handshake_timeout(
    proc: &mut acpx_conductor::BackendProcess,
    call_policy: BackendCallPolicy,
    handshake_timeout: Duration,
) -> Result<(), RouterError> {
    if !proc.handshake_done {
        let allow_fs_access = call_policy.allow_fs_access;
        let allow_terminal_access = call_policy.allow_terminal_access;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_REQUEST_ID,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    // Real -- not aspirational -- as of ACP compatibility
                    // hardening phase 3: `read_matching_response` genuinely
                    // implements both methods now (real disk I/O against
                    // acpx's own host filesystem) when `allow_fs_access` is
                    // `true` for the profile this process belongs to. `false`
                    // (the default -- see `Profile::allow_fs_access`'s doc
                    // comment for why opt-in, not opt-out) keeps declaring
                    // both `false`, byte-for-byte the pre-phase-3 behavior.
                    "fs": { "readTextFile": allow_fs_access, "writeTextFile": allow_fs_access },
                    // Phase 4: same treatment for the `terminal` capability
                    // group -- all five sub-methods tied to one profile-level
                    // opt-in (`Profile::allow_terminal_access`), since
                    // granular per-sub-method opt-in has no real security
                    // value (they're meaningless without each other).
                    "terminal": {
                        "create": allow_terminal_access,
                        "output": allow_terminal_access,
                        "waitForExit": allow_terminal_access,
                        "kill": allow_terminal_access,
                        "release": allow_terminal_access
                    }
                }
            }
        });
        write_backend_value_locked(proc, &request).await?;
        // Bounded by `BACKEND_HANDSHAKE_TIMEOUT` (see its own doc comment
        // for the live incident this closes): a bare, unbounded
        // `proc.reader.read_value().await` loop here left this call --
        // and the per-process `BackendProcess` lock every caller holds
        // around it -- hanging forever against a backend that never
        // answers `initialize`.
        let handshake = async {
            loop {
                let value = proc.reader_mut().read_value().await?;
                if value.get("id").and_then(|v| v.as_i64()) == Some(INITIALIZE_REQUEST_ID) {
                    return Ok::<_, RouterError>(value);
                }
                // A well-behaved adapter shouldn't emit anything unprompted
                // before answering `initialize`, but stay defensive rather
                // than assuming the very first line back is necessarily the
                // match -- `read_value`'s own `FramingError::Eof` on a
                // closed pipe is still the hard stop if the backend never
                // answers at all.
            }
        };
        match tokio::time::timeout(handshake_timeout, handshake).await {
            Ok(Ok(value)) => {
                // Capture the backend's real `initialize` result -- its
                // actual `agentCapabilities`/`authMethods`/negotiated
                // `protocolVersion` -- instead of discarding it. Surfaced to
                // gateway clients via `session/new`'s `_acpx.agentCapabilities`
                // (see `attach_session_new_extras`) so a client can find out
                // what a given backend genuinely supports rather than acpx
                // silently assuming. **Real gap this closes** (found during
                // an ACP-compatibility self-review, not from a test failure):
                // every dispatch path before this fix threw the `initialize`
                // response away entirely once the id matched, so acpx never
                // knew -- and never told a client -- whether a backend
                // supports e.g. `loadSession`, image content, or any auth
                // method at all.
                proc.agent_capabilities = value.get("result").cloned();
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                tracing::warn!(
                    timeout_secs = handshake_timeout.as_secs(),
                    "backend produced no response to the initialize handshake within the \
                     timeout window; killing the wedged process and failing this call so \
                     the per-process lock it held is freed for every other session on this \
                     agent"
                );
                let _ = proc.kill().await;
                return Err(RouterError::BackendHandshakeTimeout(
                    "initialize",
                    handshake_timeout,
                ));
            }
        }
        proc.handshake_done = true;
    }

    // Real ACP `authenticate` -- driven off the *cached* `initialize`
    // result (`proc.agent_capabilities`), not a re-send of `initialize`
    // itself, so this branch is safe to re-run on every call until it
    // succeeds (e.g. after an operator fixes a misconfigured profile)
    // without ever sending a second `initialize` on the same process,
    // which a real adapter has no obligation to tolerate.
    if !proc.authenticated {
        let auth_methods = proc
            .agent_capabilities
            .as_ref()
            .and_then(|r| r.get("authMethods"))
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        if !auth_methods.is_empty() {
            let Some(method_id) = call_policy.auth_method_id.as_deref() else {
                return Err(RouterError::BackendRequiresAuthentication(
                    serde_json::Value::Array(auth_methods),
                ));
            };
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": AUTHENTICATE_REQUEST_ID,
                "method": "authenticate",
                "params": { "methodId": method_id }
            });
            write_backend_value_locked(proc, &request).await?;
            // Same `BACKEND_HANDSHAKE_TIMEOUT` bound as the `initialize`
            // handshake just above -- this read was the other half of the
            // same unbounded-hang gap.
            let handshake = async {
                loop {
                    let value = proc.reader_mut().read_value().await?;
                    if value.get("id").and_then(|v| v.as_i64()) == Some(AUTHENTICATE_REQUEST_ID) {
                        return Ok::<_, RouterError>(value);
                    }
                    // Same defensive stance as the `initialize` loop above --
                    // a well-behaved adapter shouldn't emit anything
                    // unprompted before answering `authenticate` either.
                }
            };
            match tokio::time::timeout(handshake_timeout, handshake).await {
                Ok(Ok(value)) => {
                    if let Some(error) = value.get("error") {
                        return Err(RouterError::BackendAuthenticationError(error.clone()));
                    }
                    proc.authenticated = true;
                }
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = handshake_timeout.as_secs(),
                        "backend produced no response to the authenticate handshake within \
                         the timeout window; killing the wedged process and failing this \
                         call so the per-process lock it held is freed for every other \
                         session on this agent"
                    );
                    let _ = proc.kill().await;
                    return Err(RouterError::BackendHandshakeTimeout(
                        "authenticate",
                        handshake_timeout,
                    ));
                }
            }
        } else {
            // No auth required at all -- vacuously "authenticated" so
            // this branch short-circuits on every subsequent call
            // without re-deriving `auth_methods` from JSON each time.
            proc.authenticated = true;
        }
    }

    Ok(())
}

/// Pulls `result.sessionId` out of a `session/new` response, or a proper
/// [`RouterError::BackendSessionNewError`] carrying the backend's own
/// JSON-RPC `error` object if it sent one -- **discovered as a real
/// debugging pain point** driving `real_claude_multi_agent_test.rs`: the
/// old code only ever checked for a missing `result.sessionId`, so a
/// backend that legitimately rejected the request (e.g. claude-agent-acp
/// returning a real `-32602 Invalid params` for a `session/new` missing
/// its required `mcpServers` field) surfaced through acpx as an opaque
/// "no result.sessionId" with the actual rejection reason silently
/// dropped, not forwarded.
fn extract_backend_session_id(response: &serde_json::Value) -> Result<String, RouterError> {
    if let Some(error) = response.get("error") {
        return Err(RouterError::BackendSessionNewError(error.clone()));
    }
    response
        .get("result")
        .and_then(|r| r.get("sessionId"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or(RouterError::MissingBackendSessionId)
}

/// Build the client's real ACP `session/request_permission` reply for a
/// given `policy` -- the schema is `agentclientprotocol.com/protocol/
/// schema`'s `RequestPermissionResponse`: `result.outcome` is a
/// discriminated union, either `{"outcome": "selected", "optionId": ..}`
/// or `{"outcome": "cancelled"}`. See [`crate::profile::PermissionPolicy`]'s
/// doc comment for why acpx answers this automatically per profile
/// config rather than leaving it unanswered (ACP's own spec explicitly
/// allows this).
fn build_permission_reply(
    request: &serde_json::Value,
    policy: PermissionPolicy,
) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let options: Vec<serde_json::Value> = request
        .get("params")
        .and_then(|p| p.get("options"))
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let kind_prefix = match policy {
        PermissionPolicy::AutoAllow => "allow_",
        PermissionPolicy::AutoReject => "reject_",
    };
    let by_kind = options.iter().find(|opt| {
        opt.get("kind")
            .and_then(|k| k.as_str())
            .map(|k| k.starts_with(kind_prefix))
            .unwrap_or(false)
    });
    // `AutoAllow` falls back to the backend's first offered option if
    // none is explicitly labeled `allow_*` (matching the reference Go
    // SDK's own "no preference -> first option" behavior) -- this policy
    // is already an explicit opt-in to acpx deciding "yes" on the
    // client's behalf. `AutoReject` never does the equivalent fallback:
    // guessing an unlabeled option under the *safety-conservative*
    // default policy could easily select something that isn't actually a
    // rejection, so it replies `cancelled` instead when no `reject_*`
    // option was offered.
    let chosen = by_kind.or_else(|| match policy {
        PermissionPolicy::AutoAllow => options.first(),
        PermissionPolicy::AutoReject => None,
    });
    let outcome = match chosen.and_then(|opt| opt.get("optionId").and_then(|o| o.as_str())) {
        Some(option_id) => serde_json::json!({"outcome": "selected", "optionId": option_id}),
        None => serde_json::json!({"outcome": "cancelled"}),
    };
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"outcome": outcome}
    })
}

/// Answer a real `fs/read_text_file` or `fs/write_text_file` request
/// against acpx's own host filesystem -- real disk I/O, not a stub.
/// Schema per `agentclientprotocol.com/protocol/file-system`:
/// `fs/read_text_file`'s params are `{sessionId, path, line?, limit?}`
/// (`line`: 1-indexed line to start from; `limit`: max number of lines),
/// result `{content}`; `fs/write_text_file`'s params are
/// `{sessionId, path, content}`, result `{}` (no data, success is the
/// signal). `path` is used exactly as sent -- real ACP clients (editors)
/// always send absolute paths, and acpx has no separate notion of a
/// session's own "workspace root" to resolve a relative one against
/// today, so a backend sending a relative path gets whatever
/// `std::env::current_dir` resolves it against, same as any other
/// process. Only reached when `allow_fs_access` is already `true` for
/// the calling profile -- callers must check that first (see
/// `read_matching_response`), since this function does no permission
/// check of its own.
async fn handle_fs_request(request: &serde_json::Value, method: &str) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let params = request.get("params");
    let path = match params.and_then(|p| p.get("path")).and_then(|p| p.as_str()) {
        Some(path) => path.to_string(),
        None => {
            return serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "missing required 'path' param"}
            })
        }
    };
    match method {
        "fs/read_text_file" => {
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    let line = params.and_then(|p| p.get("line")).and_then(|l| l.as_u64());
                    let limit = params.and_then(|p| p.get("limit")).and_then(|l| l.as_u64());
                    let content = match (line, limit) {
                        (None, None) => content,
                        (line, limit) => {
                            // `line` is 1-indexed per the ACP schema; absent
                            // means start from the top. `limit` caps the
                            // number of lines returned; absent means the
                            // rest of the file.
                            let start = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
                            let lines: Vec<&str> = content.lines().collect();
                            let end = match limit {
                                Some(n) => (start + n as usize).min(lines.len()),
                                None => lines.len(),
                            };
                            lines.get(start..end).unwrap_or(&[]).join("\n")
                        }
                    };
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": content}})
                }
                Err(err) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32001, "message": format!("fs/read_text_file: {err}"), "data": {"path": path}}
                }),
            }
        }
        "fs/write_text_file" => {
            let content = params
                .and_then(|p| p.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            match tokio::fs::write(&path, content).await {
                Ok(()) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                Err(err) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32001, "message": format!("fs/write_text_file: {err}"), "data": {"path": path}}
                }),
            }
        }
        // Unreachable in practice -- `read_matching_response` only calls
        // this for the two methods above -- but a `match` without a
        // catch-all here would be a silent trap for a future third `fs/*`
        // method added to that call site without updating this function.
        other => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("acpx gateway does not implement '{other}'")}
        }),
    }
}

/// Hard ceiling on `terminal/wait_for_exit`'s blocking wait.
///
/// **Why this exists.** `handle_terminal_request` runs with `proc: &mut
/// BackendProcess` -- the *same* per-process lock every other session
/// sharing this backend depends on (every caller of `handle_unmatched_
/// frame`, this function's own caller, holds it for the entire call --
/// see that function's own doc comment). Real ACP `terminal/wait_for_
/// exit` semantics are genuinely blocking-until-exit by design (unlike
/// `terminal/output`'s non-blocking snapshot), so a client that starts a
/// long-running command and immediately awaits its exit is not itself
/// misbehaving -- but nothing about that call should be allowed to
/// freeze every *other* session on this same backend agent process for
/// however long that command happens to run. Bounding it here means a
/// command that legitimately runs longer than this window still keeps
/// running (this does not kill the terminal, only fails *this specific
/// RPC call*) -- a well-behaved caller falls back to polling `terminal/
/// output`/`terminal/kill` instead, the same pattern a non-blocking ACP
/// client already needs for any command it isn't sure will finish
/// quickly. Sized the same as `BACKEND_IDLE_READ_TIMEOUT` -- both exist
/// for the identical reason (bound how long one call may hold this
/// exact lock hostage on every other session's behalf).
const TERMINAL_WAIT_FOR_EXIT_TIMEOUT: Duration = BACKEND_IDLE_READ_TIMEOUT;

/// Answer a real `terminal/*` request against acpx's own host, backed by
/// `acpx_conductor::TerminalHandle` (see that module's doc comment).
/// Schema per `agentclientprotocol.com/protocol/v1/terminals`:
/// `terminal/create`'s params are `{sessionId, command, args?, env?,
/// cwd?, outputByteLimit?}` (`env` is ACP's usual array-of-`{name,value}`
/// shape, not a JSON object map) -> `{terminalId}`; `terminal/output` ->
/// `{output, truncated, exitStatus?}` (`truncated` is a **required**
/// field per the real schema -- phase 10 fix, was silently omitted
/// before); `terminal/wait_for_exit` -> `{exitStatus}`;
/// `terminal/kill`/`terminal/release` -> `{}`. `exitStatus` is
/// `{exitCode, signal}` (either may be `null`). Needs `&mut proc` (unlike
/// `handle_fs_request`) since terminal state lives in
/// `BackendProcess::terminals`, keyed by the terminal id acpx mints in
/// `terminal/create`'s reply and the backend passes back on every
/// subsequent call. Only reached when `allow_terminal_access` is already
/// `true` for the calling profile -- see `read_matching_response`.
async fn handle_terminal_request(
    proc: &mut acpx_conductor::BackendProcess,
    request: &serde_json::Value,
    method: &str,
) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let params = request.get("params");
    let error = |code: i64, message: String| serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});

    if method == "terminal/create" {
        let Some(command) = params
            .and_then(|p| p.get("command"))
            .and_then(|c| c.as_str())
        else {
            return error(-32602, "missing required 'command' param".to_string());
        };
        let args: Vec<String> = params
            .and_then(|p| p.get("args"))
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // ACP's `env` is an array of `{name, value}` objects (matching
        // its use elsewhere in the schema), not a JSON object map.
        let env: HashMap<String, String> = params
            .and_then(|p| p.get("env"))
            .and_then(|e| e.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let name = entry.get("name")?.as_str()?.to_string();
                        let value = entry.get("value")?.as_str()?.to_string();
                        Some((name, value))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cwd = params.and_then(|p| p.get("cwd")).and_then(|c| c.as_str());
        let output_byte_limit = params
            .and_then(|p| p.get("outputByteLimit"))
            .and_then(|l| l.as_u64())
            .map(|l| l as usize);

        return match acpx_conductor::TerminalHandle::spawn(
            command,
            &args,
            &env,
            cwd,
            output_byte_limit,
        )
        .await
        {
            Ok(handle) => {
                let terminal_id = format!("term-{}", uuid::Uuid::new_v4());
                proc.terminals.insert(terminal_id.clone(), handle);
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"terminalId": terminal_id}})
            }
            Err(err) => error(-32001, format!("terminal/create: {err}")),
        };
    }

    // Every other `terminal/*` method references an existing terminal by
    // id.
    let Some(terminal_id) = params
        .and_then(|p| p.get("terminalId"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
    else {
        return error(-32602, "missing required 'terminalId' param".to_string());
    };

    match method {
        "terminal/output" => match proc.terminals.get(&terminal_id) {
            Some(handle) => {
                let (output, truncated, exit_status) = handle.output().await;
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "output": String::from_utf8_lossy(&output),
                        "truncated": truncated,
                        "exitStatus": exit_status.map(|s| serde_json::json!({"exitCode": s.exit_code, "signal": s.signal})),
                    }
                })
            }
            None => error(-32602, format!("unknown terminalId '{terminal_id}'")),
        },
        "terminal/wait_for_exit" => match proc.terminals.get_mut(&terminal_id) {
            Some(handle) => {
                match tokio::time::timeout(TERMINAL_WAIT_FOR_EXIT_TIMEOUT, handle.wait_for_exit())
                    .await
                {
                    Ok(Ok(status)) => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"exitStatus": {"exitCode": status.exit_code, "signal": status.signal}}
                    }),
                    Ok(Err(err)) => error(-32001, format!("terminal/wait_for_exit: {err}")),
                    Err(_elapsed) => {
                        // See `TERMINAL_WAIT_FOR_EXIT_TIMEOUT`'s doc
                        // comment: the command keeps running (not
                        // killed) -- only this call fails, freeing the
                        // per-process lock for every other session
                        // sharing this backend instead of holding it
                        // hostage for the command's entire runtime.
                        tracing::warn!(
                            timeout_secs = TERMINAL_WAIT_FOR_EXIT_TIMEOUT.as_secs(),
                            %terminal_id,
                            "terminal/wait_for_exit exceeded its timeout; the command is \
                             still running (not killed) but this call is failing so it \
                             doesn't hold the per-process lock hostage for every other \
                             session sharing this backend -- poll terminal/output or \
                             terminal/kill instead"
                        );
                        error(
                            -32001,
                            format!(
                                "terminal/wait_for_exit exceeded {TERMINAL_WAIT_FOR_EXIT_TIMEOUT:?}; \
                                 the command is still running -- poll terminal/output or call \
                                 terminal/kill instead of waiting again"
                            ),
                        )
                    }
                }
            }
            None => error(-32602, format!("unknown terminalId '{terminal_id}'")),
        },
        "terminal/kill" => match proc.terminals.get_mut(&terminal_id) {
            Some(handle) => match handle.kill().await {
                Ok(()) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                Err(err) => error(-32001, format!("terminal/kill: {err}")),
            },
            None => error(-32602, format!("unknown terminalId '{terminal_id}'")),
        },
        "terminal/release" => {
            // Per spec, the id becomes invalid for every other terminal/*
            // method after this -- dropping it from the map (which also
            // drops the `TerminalHandle`, killing the child via
            // `kill_on_drop` if it's still running) achieves exactly that.
            if proc.terminals.remove(&terminal_id).is_some() {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                error(-32602, format!("unknown terminalId '{terminal_id}'"))
            }
        }
        other => error(-32601, format!("acpx gateway does not implement '{other}'")),
    }
}

/// **Phase 14 addition.** Context needed to route a `session/update`
/// notification live to a subscribed transport connection instead of
/// buffering it for `_acpx.updates` -- see `crate::notify`'s module doc
/// comment for the full rationale. Only constructed by the `_shared`
/// dispatch family (`dispatch_session_new_shared`/`dispatch_proxied_
/// shared`), the production path every `acpx-server` transport actually
/// uses. The plain `&mut self` dispatch path (`Router::dispatch_session_
/// new`/`Router::dispatch_proxied`, used by most of this crate's own
/// in-process tests, see e.g. `session_update_forwarding_test.rs`) has no
/// `SharedRouterHandle` available at its call sites and keeps the
/// pre-phase-14 buffer-only behavior unchanged by passing `None` -- this
/// is a deliberate scope decision (those tests assert on `_acpx.updates`
/// directly), not an oversight.
struct LiveNotifyCtx {
    router: SharedRouterHandle,
    agent_id: String,
    /// **Phase B (`acpx-tenant-isolation`) addition.** `try_deliver_live`
    /// resolves the backend-native session id back to a gateway id via
    /// `SessionRegistry::find_by_backend`, which is now tenant-scoped --
    /// without this, a session created under a non-default tenant would
    /// never be found (only the default tenant's submap would ever be
    /// searched), silently breaking live delivery for every non-default
    /// tenant. `Some` for a call-scoped context (`dispatch_proxied_shared`
    /// knows the exact tenant the in-flight call belongs to); `None` for
    /// the phase-15 idle-scavenger background task
    /// ([`spawn_idle_scavenger_if_new`]), which runs once per physical
    /// backend process (potentially shared across tenants) with no
    /// per-call tenant context -- `None` means "search every tenant" via
    /// `SessionRegistry::find_by_backend_any_tenant`.
    tenant_id: Option<TenantId>,
    /// **Interactive relay addition.** A clone of the same
    /// `AgentRequestHub` `Router::agent_request_hub()` hands to
    /// transports, so `read_matching_response` can attempt a live relay
    /// for an agent-initiated request without needing to re-acquire the
    /// router lock mid-backend-I/O.
    agent_relay: AgentRequestHub,
    /// **Interactive relay addition.** The *gateway* session id already
    /// known at this ctx's construction site, when there is one --
    /// `try_deliver_live`'s backend-id -> gateway-id translation exists
    /// because a bare `session/update` notification only ever carries
    /// the backend's own session id, but every agent-initiated *request*
    /// this relay targets arrives strictly mid an already-dispatched
    /// call whose caller already resolved the gateway id for its own
    /// bookkeeping (see `dispatch_proxied_shared`) -- reusing that
    /// instead of re-deriving it avoids a second registry lookup per
    /// request. `None` at the two sites that can't cheaply know it yet
    /// (`spawn_idle_scavenger_if_new`'s ctx, and `session/new`, which
    /// mints its gateway id only *after* this ctx would have been built
    /// -- see the "No `LiveNotifyCtx` here" comment at that call site):
    /// a relay attempt against `None` is skipped and falls straight
    /// through to the policy auto-answer, same as `live: None` entirely.
    gateway_session_id: Option<String>,
    /// **Interactive fs/terminal approval + terminal streaming
    /// addition.** A clone of the same `NotificationHub`
    /// `Router::notification_hub()` hands to transports -- reused here
    /// (rather than re-locking `router` to fetch it, `try_deliver_live`'s
    /// own pattern) so `spawn_terminal_output_stream` can push directly
    /// without an extra async round trip through the router lock on
    /// every poll tick.
    notification_hub: NotificationHub,
    /// **Terminal streaming addition.** The exact physical backend
    /// process this call is dispatched against -- `Arc` clone, cheap.
    /// `terminal/create`'s success arm in [`read_matching_response`]
    /// uses this to spawn [`spawn_terminal_output_stream`], which needs
    /// its own independent lock acquisitions on the same
    /// `BackendProcess` (to reach `terminals`) across many poll ticks
    /// long after the `session/prompt` call that created the terminal
    /// has already returned -- it cannot borrow the `&mut proc` guard
    /// the in-flight call itself holds.
    backend: acpx_conductor::supervisor::SharedBackendProcess,
}

/// Bounded, per-`(tenant_id, gateway_session_id)` queue of
/// `session/update` notifications the demux consumer observed but had
/// nobody live-subscribed to hand them to -- see the `pending_updates`
/// field doc comment on [`Router`] for the full "why this exists" story.
/// `push` is best-effort and self-bounding (`MAX_BUFFERED_PER_SESSION`),
/// deliberately dropping the *oldest* entry once a session's queue is
/// full rather than growing unbounded, so a long-idle HTTP-only session
/// behind a chatty backend can never leak memory -- a caller that never
/// comes back to drain simply loses the oldest updates first, matching
/// every other best-effort delivery path in this file (e.g.
/// `NotificationHub::publish`'s own "best effort, no delivery guarantee"
/// contract). `drain` removes and returns everything queued, leaving the
/// entry empty (not present) behind -- the next `push` for that same
/// session starts a fresh queue, same as if nothing had ever been
/// buffered.
#[derive(Clone, Default)]
struct PendingUpdates {
    inner: std::sync::Arc<tokio::sync::Mutex<HashMap<(TenantId, String), Vec<serde_json::Value>>>>,
}

/// Cap on how many undelivered `session/update` notifications
/// [`PendingUpdates`] queues per gateway session before dropping the
/// oldest -- generous enough to cover a real turn's worth of streamed
/// chunks (a chatty backend can emit dozens per turn) without letting an
/// HTTP client that never polls back leak memory indefinitely.
const MAX_BUFFERED_UPDATES_PER_SESSION: usize = 256;

impl PendingUpdates {
    fn new() -> Self {
        Self::default()
    }

    async fn push(&self, tenant_id: &TenantId, gateway_session_id: &str, value: serde_json::Value) {
        let mut inner = self.inner.lock().await;
        let queue = inner
            .entry((tenant_id.clone(), gateway_session_id.to_string()))
            .or_default();
        queue.push(value);
        while queue.len() > MAX_BUFFERED_UPDATES_PER_SESSION {
            queue.remove(0);
        }
    }

    async fn drain(&self, tenant_id: &TenantId, gateway_session_id: &str) -> Vec<serde_json::Value> {
        let mut inner = self.inner.lock().await;
        inner
            .remove(&(tenant_id.clone(), gateway_session_id.to_string()))
            .unwrap_or_default()
    }
}

/// Resolve `backend_session_id` (a backend-native session id straight off
/// an agent's own frame) back to its acpx gateway session id and owning
/// tenant. Scoped to `ctx.tenant_id` when the caller already knows it
/// (every per-call `LiveNotifyCtx`, e.g. `dispatch_proxied_shared`);
/// searched across every tenant when it doesn't
/// (`spawn_demux_consumer`'s and `spawn_idle_scavenger_if_new`'s
/// process-wide ctx, see their own `tenant_id: None` doc comments -- a
/// physical backend process may be shared across tenants, and neither of
/// those two background consumers has a single call's tenant context to
/// scope to).
///
/// Shared by every "answer this backend-initiated frame on behalf of a
/// live client" path ([`try_deliver_live`], [`try_forward_interaction`],
/// [`try_relay_agent_request`], and the `terminal/create` live-streaming
/// spawn in [`handle_unmatched_frame`]) so all four agree on one
/// tenant-resolution rule instead of each hand-rolling its own. Before
/// this helper existed, only `try_deliver_live` had the any-tenant
/// fallback; `try_forward_interaction` required `ctx.tenant_id` to
/// already be `Some` and `try_relay_agent_request` required
/// `ctx.gateway_session_id` to already be `Some` -- both are always
/// `None` on `spawn_demux_consumer`'s ctx (it is built once per shared
/// backend *process*, before any particular session's id is known), so
/// once `process_reader_demux` activates for a process, every
/// `InteractionHub`/`AgentRequestHub` relay and every live terminal
/// stream for every session sharing that process silently stopped
/// working -- `session/request_permission` (and `fs/*`/`terminal/create`
/// approval) requests fell straight through to the profile's static
/// auto-answer instead of ever reaching a live client such as Zed (via
/// the `/acp` bridge's `InteractionHub` binding), and `terminal/create`
/// never started its live output stream. This was a real, reproducible
/// regression, not a hypothetical -- fixed by having each of those four
/// call sites resolve the gateway session per-frame, from the frame's
/// own `params.sessionId`, exactly like `try_deliver_live` already did.
async fn resolve_gateway_session(
    ctx: &LiveNotifyCtx,
    backend_session_id: &str,
) -> Option<(TenantId, acpx_proto::session::GatewaySessionId)> {
    let r = ctx.router.lock().await;
    match &ctx.tenant_id {
        Some(tenant_id) => r
            .sessions
            .find_by_backend(tenant_id, &ctx.agent_id, backend_session_id)
            .map(|gateway_id| (tenant_id.clone(), gateway_id)),
        None => r
            .sessions
            .find_by_backend_any_tenant(&ctx.agent_id, backend_session_id),
    }
}

/// Attempt to deliver a real `session/update` notification (`value`,
/// straight off a backend's stdout, still carrying its *backend-native*
/// `params.sessionId`) live to whichever gateway session it belongs to,
/// via `ctx`'s `NotificationHub`. Returns `true` if it was actually
/// delivered (a live subscriber was registered for the translated gateway
/// session id and the send succeeded) -- the caller must not also buffer
/// `value` into the `_acpx.updates` fallback in that case, or the same
/// client would see it twice.
///
/// Briefly re-locks `ctx.router` just to look up the backend-id ->
/// gateway-id translation (`SessionRegistry::find_by_backend`) and clone
/// the (cheaply cloneable) `NotificationHub` out -- consistent with every
/// other `_shared` dispatch function's "lock briefly for a lookup, release
/// before any actual I/O" convention in this file. The lock is held only
/// for a synchronous `HashMap` lookup, never across the backend I/O this
/// function itself doesn't perform.
async fn try_deliver_live(ctx: &LiveNotifyCtx, value: &serde_json::Value) -> bool {
    let Some(backend_session_id) = value
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
    else {
        return false;
    };
    let Some((tenant_id, gateway_id)) = resolve_gateway_session(ctx, backend_session_id).await
    else {
        return false;
    };
    let mut translated = value.clone();
    if let Some(session_id_field) = translated
        .get_mut("params")
        .and_then(|p| p.get_mut("sessionId"))
    {
        *session_id_field = serde_json::Value::String(gateway_id.0.clone());
    }
    ctx.notification_hub.publish(&tenant_id, &gateway_id.0, translated).await
}

/// Forward a backend-initiated request to the persistent client that owns
/// this session. `Ok(None)` means this dispatch has no bound client and the
/// caller must use its existing profile-policy fallback.
async fn try_forward_interaction(
    ctx: &LiveNotifyCtx,
    value: &serde_json::Value,
) -> Result<Option<serde_json::Value>, crate::InteractionError> {
    let Some(backend_session_id) = value
        .get("params")
        .and_then(|params| params.get("sessionId"))
        .and_then(|session_id| session_id.as_str())
    else {
        return Ok(None);
    };
    let Some((tenant_id, gateway_id)) = resolve_gateway_session(ctx, backend_session_id).await
    else {
        return Ok(None);
    };
    let interaction_hub = { ctx.router.lock().await.interaction_hub.clone() };

    let mut request = value.clone();
    if let Some(session_id) = request
        .get_mut("params")
        .and_then(|params| params.get_mut("sessionId"))
    {
        *session_id = serde_json::Value::String(gateway_id.0.clone());
    }
    interaction_hub
        .request(&tenant_id, &gateway_id.0, request, DEFAULT_INTERACTION_TIMEOUT)
        .await
}

/// **Phase 15.** The idle/background-reader gap phase 14 documented and
/// deliberately left open: `read_matching_response`'s read loop only ever
/// runs while one client call is in flight against a given backend, so a
/// notification a backend emits while nothing is currently in flight
/// against it sits unread in the OS pipe buffer until the next call
/// happens to drain it -- and never arrives at all if no further call is
/// ever made. One instance of this task is spawned (via
/// [`Router::spawn_idle_scavenger_if_new`]) the first time each physical
/// backend process is seen, and keeps running for that exact process
/// instance's whole lifetime.
///
/// **How it avoids racing `read_matching_response`.** Both this task and
/// every in-flight call read from the exact same `BackendProcess::reader`
/// -- one child process's stdout is a single stream, so only one reader
/// may ever be draining it at a time, or frames get corrupted/misrouted
/// between two concurrent readers. `backend.try_lock()` is the mechanism
/// that guarantees this: an in-flight call already holds this exact
/// process's own lock for its entire `read_matching_response` loop (by
/// design, see that function's doc comment), so `try_lock()` fails
/// (`Err`) for the whole time a real call owns this backend, and this
/// task simply backs off and retries later -- it never touches `reader`
/// except during the strictly-idle windows where no call holds the lock
/// at all. Conversely, while this task *does* hold the lock (briefly, one
/// non-blocking drain pass), a new call's own `backend.lock().await`
/// simply queues behind it for that same bounded moment, never anything
/// close to a whole call's real-LLM-latency duration -- this preserves
/// the "no lock held across backend I/O" discipline every other function
/// in this file follows, it just adds one more brief, bounded holder of
/// the same lock.
///
/// **What it does with what it finds.** Only a bare notification (a
/// `method`, no `id`) is actionable outside of any call context; an
/// id-bearing frame (an agent-initiated request, or a stray response with
/// no waiting caller) has no in-flight call here to answer or hand it to,
/// so it's logged and dropped rather than guessed at -- in practice this
/// should never happen, since every agent-initiated request this
/// codebase knows how to answer (`session/request_permission`, `fs/*`,
/// `terminal/*`) is only ever sent by a well-behaved backend mid an
/// already in-flight `session/prompt`, which means a real call already
/// holds this exact lock throughout, so this task would never observe
/// one in the first place; logging (not silently discarding) covers the
/// case where that assumption turns out to be wrong against some real
/// adapter. A `session/update` is delivered live via
/// [`try_deliver_live`], the exact same path/translation/hub a call
/// in-flight would have used -- if that succeeds, this closes the phase
/// 14 gap precisely: an update that arrived between prompt turns now
/// reaches a subscribed stdio/WS connection instead of waiting, possibly
/// forever, for the next call to that backend. If no live subscriber is
/// registered (or the notification isn't `session/update`) there is
/// still nothing to buffer it into -- no in-flight call's `_acpx.updates`
/// exists right now -- so it's logged and discarded, same as it always
/// effectively was pre-phase-15 (silently sitting unread forever), except
/// now it's observed rather than invisible. Extending idle notifications
/// to also feed the *next* call's `_acpx.updates` bundle (for `POST
/// /rpc`-style clients with no live connection to push to at all) is left
/// out of scope on purpose -- `POST /rpc` was already excluded from live
/// delivery entirely in phase 14 for the same "no persistent connection
/// to push to" reason, and this phase's gap statement is specifically
/// about the two transports capable of a live push in the first place.
async fn backend_idle_scavenger(
    backend: acpx_conductor::supervisor::SharedBackendProcess,
    ctx: LiveNotifyCtx,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(75)).await;
        let Ok(mut proc) = backend.try_lock() else {
            // A real call owns this backend right now -- its own
            // `read_matching_response` loop is already draining
            // `reader`, so there is nothing for this tick to do.
            continue;
        };
        if proc.has_exited() {
            // This physical process instance is gone for good (a
            // respawn, if any, is a brand new `SharedBackendProcess`
            // with its own fresh scavenger, see `Router::
            // spawn_idle_scavenger_if_new`'s doc comment) -- nothing
            // left to scavenge, stop the task rather than spin forever.
            return;
        }
        if proc.pending.is_some() {
            // `process_reader_demux` activated for this exact process
            // instance sometime between this tick and the last (any
            // `_shared` dispatch path can call `start_demux`, taking
            // `proc.reader` and handing this process's frames to
            // `spawn_demux_consumer` instead). `reader_mut()` panics
            // once that has happened (its own doc comment) -- this was
            // observed in production as a real panic in this exact
            // task. `spawn_demux_consumer` already took over every
            // duty this scavenger existed for (idle live-notification
            // delivery included, see its own doc comment: "runs
            // continuously for the whole process lifetime, not only
            // during idle windows"), so stopping here is not a
            // regression -- it's ceding a now-redundant job to the
            // task that made it redundant.
            return;
        }
        // Drain every frame already sitting in the OS pipe buffer, but
        // never block waiting for one that hasn't arrived yet -- a
        // zero-duration `timeout` around one `read_value` call is this
        // function's "try a non-blocking read" idiom: data already
        // available resolves on the very first poll, exactly like a real
        // read would; anything not yet available times out immediately
        // instead of parking this task (and the process lock it's
        // holding) waiting for it.
        loop {
            let attempt =
                tokio::time::timeout(Duration::from_millis(0), proc.reader_mut().read_value()).await;
            let value = match attempt {
                Ok(Ok(value)) => value,
                Ok(Err(err)) => {
                    tracing::warn!(
                        agent_id = %ctx.agent_id,
                        %err,
                        "acpx idle scavenger's backend read errored; stopping this backend's scavenger"
                    );
                    return;
                }
                Err(_) => break, // nothing ready right now -- hand the lock back
            };
            if value.get("id").is_some() {
                tracing::warn!(
                    agent_id = %ctx.agent_id,
                    ?value,
                    "acpx idle scavenger saw an id-bearing frame with no in-flight caller; ignoring"
                );
                continue;
            }
            if value.get("method").and_then(|m| m.as_str()) == Some("session/update")
                && try_deliver_live(&ctx, &value).await
            {
                continue;
            }
            tracing::debug!(
                agent_id = %ctx.agent_id,
                ?value,
                "acpx idle scavenger drained a notification with no live subscriber to deliver it to; discarding"
            );
        }
    }
}

/// **`process_reader_demux`, phase 1.** Background consumer for one
/// physical backend process's stream of unmatched frames (bare
/// notifications and agent-initiated requests), fed by that process's
/// reader task (`BackendProcess::start_demux`) via `unmatched_rx`. Unlike
/// [`backend_idle_scavenger`], this runs continuously for the whole
/// process lifetime, not only during idle windows between calls -- it's
/// the fix for the race noted in `memory/acpx/tasks/zed_integration.yaml`
/// task 7 ("only forwards live updates while some caller happens to be
/// in the read loop"). Reuses [`handle_unmatched_frame`], the exact same
/// agent-request-answering/live-delivery logic the legacy read loop
/// uses, briefly re-locking `backend` per frame (never held across a
/// blocking read -- the reader task already owns that).
///
/// `policy` is fixed for this consumer's whole lifetime, captured from
/// whichever call first activated demux for this process. In practice
/// every session sharing one physical process already shares one
/// profile (`Supervisor` keys processes by agent id, optionally folded
/// with tenant/session id -- never by profile alone with differing call
/// policies on one key), so this matches what the legacy per-call read
/// loop effectively assumed too.
fn spawn_demux_consumer(
    router: SharedRouterHandle,
    agent_id: String,
    agent_relay: AgentRequestHub,
    notification_hub: NotificationHub,
    backend: acpx_conductor::supervisor::SharedBackendProcess,
    policy: BackendCallPolicy,
    mut unmatched_rx: tokio::sync::mpsc::Receiver<acpx_conductor::UnmatchedFrame>,
) {
    let ctx = LiveNotifyCtx {
        router,
        agent_id: agent_id.clone(),
        tenant_id: None,
        agent_relay,
        gateway_session_id: None,
        notification_hub,
        backend: std::sync::Arc::clone(&backend),
    };
    tokio::spawn(async move {
        while let Some(value) = unmatched_rx.recv().await {
            let mut proc = backend.lock().await;
            match handle_unmatched_frame(&mut proc, value, &policy, Some(&ctx)).await {
                Ok(UnmatchedOutcome::Notification(value)) => {
                    // No live WS/stdio subscriber delivered this
                    // `session/update` (or it wasn't one) -- this
                    // consumer has no in-flight call of its own to
                    // attach it to directly, unlike the legacy read
                    // loop's inline `_acpx.updates` fallback. Buffer it
                    // so the next `POST /rpc` call against this exact
                    // gateway session can still see it -- see
                    // `Router::pending_updates`'s field doc comment.
                    drop(proc);
                    if let Some(backend_session_id) = value
                        .get("params")
                        .and_then(|p| p.get("sessionId"))
                        .and_then(|s| s.as_str())
                    {
                        if let Some((tenant_id, gateway_id)) =
                            resolve_gateway_session(&ctx, backend_session_id).await
                        {
                            let mut translated = value.clone();
                            if let Some(field) = translated
                                .get_mut("params")
                                .and_then(|p| p.get_mut("sessionId"))
                            {
                                *field = serde_json::Value::String(gateway_id.0.clone());
                            }
                            let pending_updates =
                                { ctx.router.lock().await.pending_updates() };
                            pending_updates
                                .push(&tenant_id, &gateway_id.0, translated)
                                .await;
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        agent_id = %ctx.agent_id,
                        %err,
                        "acpx process-reader-demux consumer failed to handle an unmatched frame"
                    );
                }
            }
        }
        // `unmatched_rx` closed -- the reader task ended (process exited
        // or hit a read error). Nothing left to consume; this consumer's
        // job ends with the process it belongs to, matching
        // `backend_idle_scavenger`'s own `has_exited` early return.
    });
}

/// How long a relayed `session/request_permission` is allowed to wait
/// for a live client's decision before falling back to the profile's
/// static `permission_policy`. Generous on purpose -- this is a real
/// human decision point, not a network round trip -- but bounded so a
/// client that disconnects mid-decision (tab closed, panel crashed)
/// doesn't leave the backend's own `session/prompt` call hanging
/// forever; the backend always gets *an* answer.
const PERMISSION_RELAY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Attempt to relay an agent-initiated request to whichever transport
/// connection currently owns its gateway session, via `live`'s
/// `AgentRequestHub`. `None` (no `live` ctx at all, no known gateway
/// session id yet, no live subscriber, or a timeout) is the caller's cue
/// to fall back to the existing policy-based auto-answer -- see
/// `crate::agent_relay`'s module doc comment for the full contract.
/// Resolves `ctx.gateway_session_id` when the caller already knows it
/// (every per-call `LiveNotifyCtx`); falls back to resolving it from
/// `value`'s own `params.sessionId` via [`resolve_gateway_session`] when
/// it doesn't (`spawn_demux_consumer`'s process-wide ctx, always `None`
/// -- see that helper's doc comment for why this fallback exists at
/// all).
async fn try_relay_agent_request(
    live: Option<&LiveNotifyCtx>,
    value: &serde_json::Value,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let ctx = live?;
    let gateway_session_id = match ctx.gateway_session_id.as_deref() {
        Some(id) => id.to_string(),
        None => {
            let backend_session_id = value
                .get("params")
                .and_then(|p| p.get("sessionId"))
                .and_then(|s| s.as_str())?;
            resolve_gateway_session(ctx, backend_session_id)
                .await
                .map(|(_, gateway_id)| gateway_id.0)?
        }
    };
    ctx.agent_relay
        .relay(&gateway_session_id, value.clone(), timeout)
        .await
}

/// Ask a live client to approve or reject an `fs/read_text_file`,
/// `fs/write_text_file`, or `terminal/create` action that is already
/// gated on for this profile (`allow_fs_access`/`allow_terminal_access`
/// is `true`) -- the Coverage Matrix's "profile gate, approve/reject,
/// real disk result" / "approval, terminal ID, command metadata
/// sanitization" rows. Unlike [`try_relay_agent_request`] (which
/// forwards a live client's answer straight through as the *backend's*
/// own native ACP reply), this relays the same raw request frame but
/// expects back a small acpx-local decision envelope, `{"approved":
/// bool}` -- not a native `fs/*`/`terminal/*` result, since the
/// real disk/process I/O still happens here in acpx-server either way
/// (see the plan's Phase 1 progress-log note: the client only approves
/// or denies, it never performs the I/O itself). `None` covers every
/// "no live decision was made" case (no `live` ctx, no live subscriber,
/// timeout, or a malformed/missing `approved` field) -- callers must
/// treat that exactly like the pre-relay behavior: the profile's own
/// capability toggle being `true` is already itself an auto-allow,
/// unchanged. Only an explicit `Some(false)` denies the action.
async fn try_relay_approval(
    live: Option<&LiveNotifyCtx>,
    value: &serde_json::Value,
    timeout: Duration,
) -> Option<bool> {
    let ctx = live?;
    let gateway_session_id = ctx.gateway_session_id.as_deref()?;
    let decision = ctx
        .agent_relay
        .relay(gateway_session_id, value.clone(), timeout)
        .await?;
    decision.get("approved").and_then(|a| a.as_bool())
}

/// Build the JSON-RPC error reply for an `fs/*`/`terminal/create`
/// request a live client explicitly rejected via [`try_relay_approval`]
/// -- distinct error code/message from the "capability disabled for
/// this profile" arms above it, so a client-side log can tell "this
/// profile never allows it" apart from "a human said no this time".
fn build_approval_rejected_reply(request: &serde_json::Value, method: &str) -> serde_json::Value {
    let req_id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {
            "code": -32002,
            "message": format!("'{method}' was rejected by the interactive client"),
        }
    })
}

/// Poll interval for [`spawn_terminal_output_stream`] -- see that
/// function's doc comment for why polling (not a delta-subscribe API)
/// is this task's own mechanism.
const TERMINAL_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Live-stream `terminal_id`'s output to whichever connection is
/// currently subscribed to `gateway_session_id` (Coverage Matrix:
/// `terminal/output`'s "live chunks before turn end, truncation, exit
/// code" row) -- pushed as a bare `acpx/terminal_output` notification
/// (`{sessionId, terminalId, output, truncated, exitStatus}`, the exact
/// same result shape `handle_terminal_request`'s own `terminal/output`
/// reply already uses, so a client's parsing code is identical for
/// both) over the same `NotificationHub` `session/update` already uses.
/// `TerminalHandle` has no delta-subscribe API of its own (see its doc
/// comment), so this task re-polls its whole-buffer `output()` snapshot
/// on a fixed interval and republishes only when the buffer length,
/// truncation flag, or exit status actually changed since the last
/// tick -- this bounds publish volume without needing real byte-level
/// diffing (a client always receives the full current buffer, cheap to
/// just replace its displayed contents with). Stops permanently once
/// the process has exited and one final snapshot has gone out, or once
/// the terminal id is no longer present in `backend`'s registry at all
/// (released or killed-and-removed) -- whichever happens first. Not
/// gated on whether a subscriber is actually present: `NotificationHub::
/// publish` is a harmless no-op for an HTTP-only or momentarily-
/// unsubscribed session, exactly like every other live-notification
/// path in this file, and a short-lived command may exit before any
/// connection ever subscribes.
fn spawn_terminal_output_stream(
    backend: acpx_conductor::supervisor::SharedBackendProcess,
    hub: NotificationHub,
    tenant_id: TenantId,
    gateway_session_id: String,
    terminal_id: String,
) {
    tokio::spawn(async move {
        let mut last_len = 0usize;
        let mut last_truncated = false;
        loop {
            tokio::time::sleep(TERMINAL_STREAM_POLL_INTERVAL).await;
            let (output, truncated, exit_status) = {
                let mut proc = backend.lock().await;
                match proc.terminals.get_mut(&terminal_id) {
                    Some(handle) => {
                        // Non-blocking exit check every tick -- `output()`
                        // alone never observes exit on its own (see
                        // `TerminalHandle::output`'s doc comment); only
                        // `wait_for_exit`/`try_wait_for_exit` record it.
                        // Ignoring the `Err` here (a `waitpid`-level OS
                        // error, not "not exited yet") intentionally
                        // matches this poller's own best-effort delivery
                        // contract -- the same as every other
                        // `NotificationHub::publish` caller in this
                        // file, a failed tick just tries again next
                        // interval rather than tearing anything down.
                        let _ = handle.try_wait_for_exit().await;
                        handle.output().await
                    }
                    None => return,
                }
            };
            let changed = output.len() != last_len || truncated != last_truncated;
            let is_final = exit_status.is_some();
            if changed || is_final {
                last_len = output.len();
                last_truncated = truncated;
                hub.publish(
                    &tenant_id,
                    &gateway_session_id,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "acpx/terminal_output",
                        "params": {
                            "sessionId": gateway_session_id,
                            "terminalId": terminal_id,
                            "output": String::from_utf8_lossy(&output),
                            "truncated": truncated,
                            "exitStatus": exit_status.map(|s| serde_json::json!({
                                "exitCode": s.exit_code,
                                "signal": s.signal,
                            })),
                        }
                    }),
                )
                .await;
            }
            if is_final {
                return;
            }
        }
    });
}

/// **`acpx-connect-loading-feedback`.** [`dispatch_proxied`]/
/// [`dispatch_proxied_shared`] both handle every `Proxied` method through
/// one shared backend round trip -- `session/prompt` and `session/load`/
/// `session/resume` included -- so the idle-read budget has to be picked
/// per call, not baked into the call site. See
/// [`SESSION_ESTABLISH_IDLE_READ_TIMEOUT`]'s doc comment for why only
/// these two specifically (not `session/prompt`, `session/close`,
/// `session/set_config_option`, etc.) get the short budget.
fn session_establish_or_default_idle_timeout(method: &str) -> Duration {
    if matches!(method, "session/load" | "session/resume") {
        SESSION_ESTABLISH_IDLE_READ_TIMEOUT
    } else {
        BACKEND_IDLE_READ_TIMEOUT
    }
}

async fn read_matching_response(
    backend: &mut acpx_conductor::BackendProcess,
    id: &serde_json::Value,
    policy: BackendCallPolicy,
    live: Option<&LiveNotifyCtx>,
) -> Result<
    (
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    ),
    RouterError,
> {
    read_matching_response_with_idle_timeout(backend, id, policy, live, BACKEND_IDLE_READ_TIMEOUT)
        .await
}

/// [`read_matching_response`]'s real body, parameterized on the idle-read
/// timeout so tests can exercise the kill-on-expiry path in milliseconds
/// instead of waiting out the real 20-minute production value -- same
/// pattern as `acp_bridge::refresh_models`/`refresh_models_with_config`.
async fn read_matching_response_with_idle_timeout(
    backend: &mut acpx_conductor::BackendProcess,
    id: &serde_json::Value,
    policy: BackendCallPolicy,
    live: Option<&LiveNotifyCtx>,
    idle_read_timeout: Duration,
) -> Result<
    (
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    ),
    RouterError,
> {
    let mut notifications = Vec::new();
    let mut agent_requests = Vec::new();
    loop {
        // See `BACKEND_IDLE_READ_TIMEOUT`'s doc comment: a backend that
        // has stopped producing any output at all (wedged/deadlocked,
        // ignoring its own `session/cancel`) must not be allowed to hold
        // this loop -- and the per-process `BackendProcess` lock every
        // caller of this function holds around it -- forever. Killing on
        // expiry (rather than merely returning an error and leaving the
        // process running) prevents a stale late reply for this
        // abandoned `id` from later being misclassified as a
        // notification in some unrelated call's own read loop.
        let value = match tokio::time::timeout(
            idle_read_timeout,
            backend.reader_mut().read_value(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                tracing::warn!(
                    timeout_secs = idle_read_timeout.as_secs(),
                    "backend produced no output for the entire idle-read timeout window; \
                     killing the wedged process and failing this call so the per-process \
                     lock it held is freed for every other session on this agent"
                );
                let _ = backend.kill().await;
                return Err(RouterError::BackendIdleReadTimeout(
                    idle_read_timeout,
                ));
            }
        };
        if value.get("id") == Some(id) {
            return Ok((value, notifications, agent_requests));
        }
        // An agent-initiated *request* (has both its own `id` and a
        // `method`) is not a notification -- pre-fix, this loop treated
        // it as one, pushing it into `notifications` and never replying,
        // which left the backend deadlocked forever waiting for an
        // answer that would never come (verified as a real hang, not a
        // hypothetical, against `session/request_permission`: every real
        // adapter that asks permission mid-turn blocks its own response
        // to the *outer* call on getting one). `session/request_permission`
        // is the only such method acpx knows how to answer today (see
        // `build_permission_reply`); anything else gets a proper JSON-RPC
        // method-not-found error instead of silence, so a backend that
        // sends some other agent-initiated request acpx doesn't yet
        // support still gets *a* reply and can decide how to proceed
        // (e.g. treat it as declined) rather than hanging indefinitely.
        match handle_unmatched_frame(backend, value, &policy, live).await? {
            UnmatchedOutcome::AgentRequestAnswered(entry) => {
                agent_requests.push(entry);
                continue;
            }
            UnmatchedOutcome::Delivered => continue,
            UnmatchedOutcome::Notification(value) => {
                notifications.push(value);
            }
        }
    }
}

/// Outcome of routing one frame [`read_matching_response_with_idle_timeout`]'s
/// read loop (or the process-reader-demux consumer,
/// `spawn_demux_consumer`) observed that did not match the
/// response id its caller is waiting on.
enum UnmatchedOutcome {
    /// An agent-initiated request (`id` + `method`) was answered and the
    /// reply already written to `backend.writer`; record it for
    /// `_acpx.agentRequests`.
    AgentRequestAnswered(serde_json::Value),
    /// A `session/update` notification was delivered to a live subscriber;
    /// nothing further to do.
    Delivered,
    /// No live delivery happened (no `live` ctx, no subscriber, or not a
    /// `session/update`) -- caller should buffer it into `_acpx.updates`.
    Notification(serde_json::Value),
}

/// Answer or route one frame that didn't match the response id a caller
/// of `read_matching_response*` is waiting on. Pulled out of that
/// function's read loop into its own function -- byte-for-byte identical
/// logic, just given a call boundary -- so the process-reader-demux
/// consumer (phase 1, `memory/acpx/gen/acpx-concurrency-config-execution.
/// meta.json`) can reuse the exact same agent-request-answering and
/// live-notification-delivery behavior for frames it observes outside of
/// any in-flight caller's own read loop, instead of a second,
/// divergence-prone copy of this security-sensitive (permission/approval)
/// logic.
async fn handle_unmatched_frame(
    backend: &mut acpx_conductor::BackendProcess,
    value: serde_json::Value,
    policy: &BackendCallPolicy,
    live: Option<&LiveNotifyCtx>,
) -> Result<UnmatchedOutcome, RouterError> {
    // An agent-initiated *request* (has both its own `id` and a
    // `method`) is not a notification -- pre-fix, this loop treated
    // it as one, pushing it into `notifications` and never replying,
    // which left the backend deadlocked forever waiting for an
    // answer that would never come (verified as a real hang, not a
    // hypothetical, against `session/request_permission`: every real
    // adapter that asks permission mid-turn blocks its own response
    // to the *outer* call on getting one). `session/request_permission`
    // is the only such method acpx knows how to answer today (see
    // `build_permission_reply`); anything else gets a proper JSON-RPC
    // method-not-found error instead of silence, so a backend that
    // sends some other agent-initiated request acpx doesn't yet
    // support still gets *a* reply and can decide how to proceed
    // (e.g. treat it as declined) rather than hanging indefinitely.
    if let (Some(_), Some(method)) = (
        value.get("id"),
        value.get("method").and_then(|m| m.as_str()),
    ) {
        // A persistent transport may have a client bound to this exact
        // tenant/session. Give that client the first chance to answer
        // every backend-initiated request, with one carve-out:
        // `session/request_permission`/`fs/read_text_file`/
        // `fs/write_text_file`/`terminal/create` each have their own
        // dedicated `AgentRequestHub` relay below (real clients, e.g.
        // the panel's `acpx/agent_request`/`acpx/agent_response`
        // envelope, already depend on that exact wire contract) --
        // tried first for those four, with `InteractionHub` as the
        // fallback *within* each of those arms (not here) for a
        // connection that bound `InteractionHub` but never subscribed
        // to `AgentRequestHub` for this session (e.g. a strict ACP
        // bridge connection, per `strict_acp_ws_forwards_backend_
        // permission_requests_to_the_bound_client`). Every other
        // method (including the plain `terminal/output`/
        // `wait_for_exit`/`kill`/`release` polls below, which have no
        // relay concept of their own) keeps trying `InteractionHub`
        // here, unconditionally, same as before this carve-out
        // existed. Profile policy/direct handling remains the
        // deliberate fallback once every applicable live path returns
        // nothing.
        let has_dedicated_relay = matches!(
            method,
            "session/request_permission"
                | "fs/read_text_file"
                | "fs/write_text_file"
                | "terminal/create"
        );
        if !has_dedicated_relay {
            if let Some(live) = live {
                match try_forward_interaction(live, &value).await {
                    Ok(Some(mut reply)) => {
                        // The outer client sees ACPX's opaque interaction id;
                        // the backend must receive the id it originally sent.
                        reply["id"] = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                        write_backend_value_locked(backend, &reply).await?;
                        return Ok(UnmatchedOutcome::AgentRequestAnswered(
                            serde_json::json!({"request": value, "reply": reply}),
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let reply = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": value.get("id").cloned().unwrap_or(serde_json::Value::Null),
                            "error": {
                                "code": -32001,
                                "message": format!("acpx interactive client request failed: {error}"),
                            }
                        });
                        write_backend_value_locked(backend, &reply).await?;
                        return Ok(UnmatchedOutcome::AgentRequestAnswered(
                            serde_json::json!({"request": value, "reply": reply}),
                        ));
                    }
                }
            }
        }
        let reply = if method == "session/request_permission" {
                // **Interactive relay addition.** A live client (WS,
                // currently) that owns this gateway session gets first
                // say: it may take real user interaction to answer, so
                // this waits up to `PERMISSION_RELAY_TIMEOUT` before
                // falling back to the exact same static-policy answer
                // this arm always gave before the relay existed. An
                // HTTP-only client (`live: None`) or a WS client that
                // never subscribed to this session always falls straight
                // through to that same fallback, unchanged.
                //
                // **`InteractionHub` fallback.** A connection that bound
                // `InteractionHub` but never subscribed to
                // `AgentRequestHub` for this session (e.g. a strict ACP
                // bridge connection -- see `strict_acp_ws_forwards_
                // backend_permission_requests_to_the_bound_client`) gets
                // a second chance here before falling back to policy,
                // since `try_relay_agent_request` returns `None`
                // immediately for it (no `AgentRequestHub` subscriber).
                match try_relay_agent_request(live, &value, PERMISSION_RELAY_TIMEOUT).await {
                    Some(relayed) => relayed,
                    None => match live {
                        Some(live) => match try_forward_interaction(live, &value).await {
                            Ok(Some(mut reply)) => {
                                reply["id"] =
                                    value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                                reply
                            }
                            Ok(None) => build_permission_reply(&value, policy.permission_policy),
                            Err(error) => {
                                tracing::warn!(
                                    ?error,
                                    "interaction-hub permission forward failed, falling back to policy"
                                );
                                build_permission_reply(&value, policy.permission_policy)
                            }
                        },
                        None => build_permission_reply(&value, policy.permission_policy),
                    },
                }
            } else if (method == "fs/read_text_file" || method == "fs/write_text_file")
                && policy.allow_fs_access
            {
                // **Interactive approval addition.** Same relay
                // machinery as `session/request_permission` above, but
                // the client answers a lightweight `{"approved": bool}`
                // decision rather than a native ACP reply -- the real
                // disk I/O always happens here in acpx-server either
                // way (see `try_relay_approval`'s doc comment). No live
                // decision (`None`) preserves this arm's pre-existing
                // auto-allow-because-capability-is-on behavior exactly.
                match try_relay_approval(live, &value, PERMISSION_RELAY_TIMEOUT).await {
                    Some(false) => build_approval_rejected_reply(&value, method),
                    _ => handle_fs_request(&value, method).await,
                }
            } else if method == "fs/read_text_file" || method == "fs/write_text_file" {
                // Capability wasn't enabled for this profile -- declared
                // `false` in `initialize`, so a well-behaved backend
                // shouldn't be asking at all, but reply with a clear
                // "not enabled" error (not a plain method-not-found) if
                // one does anyway, distinguishing "acpx doesn't have this
                // handler" from "this profile turned it off".
                let req_id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("'{method}' is disabled for this profile (Profile::allow_fs_access is false)"),
                    }
                })
            } else if method == "terminal/create" && policy.allow_terminal_access {
                // **Interactive approval + live streaming addition.**
                // Same approval relay as the `fs/*` arm above; a
                // successful creation additionally starts
                // `spawn_terminal_output_stream` so a subscribed live
                // client sees this terminal's output as it happens,
                // not only via the backend's own polling
                // `terminal/output` calls.
                match try_relay_approval(live, &value, PERMISSION_RELAY_TIMEOUT).await {
                    Some(false) => build_approval_rejected_reply(&value, method),
                    _ => {
                        let reply = handle_terminal_request(backend, &value, method).await;
                        if let (Some(ctx), Some(terminal_id)) = (
                            live,
                            reply
                                .get("result")
                                .and_then(|r| r.get("terminalId"))
                                .and_then(|t| t.as_str()),
                        ) {
                            // Same `spawn_demux_consumer`-has-no-per-call
                            // tenant/session context gap as
                            // `try_relay_agent_request`/`try_forward_
                            // interaction` -- fall back to resolving from
                            // `value`'s own `params.sessionId` (the
                            // original `terminal/create` request, still
                            // in scope here) when `ctx`'s own fields are
                            // `None`, instead of silently never starting
                            // the live output stream. See
                            // `resolve_gateway_session`'s doc comment.
                            let resolved = match (
                                ctx.tenant_id.clone(),
                                ctx.gateway_session_id.clone(),
                            ) {
                                (Some(tenant_id), Some(gateway_session_id)) => {
                                    Some((tenant_id, gateway_session_id))
                                }
                                _ => {
                                    let backend_session_id = value
                                        .get("params")
                                        .and_then(|p| p.get("sessionId"))
                                        .and_then(|s| s.as_str());
                                    match backend_session_id {
                                        Some(backend_session_id) => {
                                            resolve_gateway_session(ctx, backend_session_id)
                                                .await
                                                .map(|(tenant_id, gateway_id)| {
                                                    (tenant_id, gateway_id.0)
                                                })
                                        }
                                        None => None,
                                    }
                                }
                            };
                            if let Some((tenant_id, gateway_session_id)) = resolved {
                                spawn_terminal_output_stream(
                                    std::sync::Arc::clone(&ctx.backend),
                                    ctx.notification_hub.clone(),
                                    tenant_id,
                                    gateway_session_id,
                                    terminal_id.to_string(),
                                );
                            }
                        }
                        reply
                    }
                }
            } else if (method == "terminal/output"
                || method == "terminal/wait_for_exit"
                || method == "terminal/kill"
                || method == "terminal/release")
                && policy.allow_terminal_access
            {
                handle_terminal_request(backend, &value, method).await
            } else if method == "terminal/create"
                || method == "terminal/output"
                || method == "terminal/wait_for_exit"
                || method == "terminal/kill"
                || method == "terminal/release"
            {
                // Same "disabled, not unsupported" distinction as the
                // `fs/*` arm above, gated on `Profile::allow_terminal_access`.
                let req_id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("'{method}' is disabled for this profile (Profile::allow_terminal_access is false)"),
                    }
                })
            } else {
                let req_id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("acpx gateway does not support agent-initiated method '{method}'"),
                    }
                })
            };
        write_backend_value_locked(backend, &reply).await?;
        return Ok(UnmatchedOutcome::AgentRequestAnswered(
            serde_json::json!({"request": value, "reply": reply}),
        ));
    }
    // **Phase 14.** A real notification (`method`, no `id`) -- try
    // live delivery first when a subscribed transport connection is
    // known (`live.is_some()`) and this is the one notification type
    // that's actually session-scoped and worth streaming live,
    // `session/update`. Anything not delivered live (no `live` ctx at
    // all, e.g. the plain `&mut self` dispatch path; no live
    // subscriber currently registered for this session, e.g. an
    // HTTP-only client; or a notification method other than
    // `session/update`) falls through to the pre-existing buffering
    // behavior unchanged, so `_acpx.updates` keeps working exactly as
    // before for every case this phase doesn't newly handle.
    if let Some(ctx) = live {
        if value.get("method").and_then(|m| m.as_str()) == Some("session/update")
            && try_deliver_live(ctx, &value).await
        {
            return Ok(UnmatchedOutcome::Delivered);
        }
    }
    Ok(UnmatchedOutcome::Notification(value))
}

/// Fold `notifications` and `agent_requests` (both as collected by
/// [`read_matching_response`]) into `response`'s `_acpx.updates`/
/// `_acpx.agentRequests` arrays, if there are any of either. `agentRequests`
/// is new: every `{request, reply}` pair `read_matching_response` had to
/// answer on the client's behalf mid-call (a `session/request_permission`
/// auto-decision per `crate::profile::PermissionPolicy`, or a method-not-
/// found error for any other agent-initiated request acpx doesn't yet
/// support) -- surfaced so a client can see what was decided without
/// acpx silently hiding it. No-op (response left byte-for-byte untouched)
/// when both are empty, so a stand-in backend that never emits either
/// (every synthetic test double in this workspace, until one is written
/// specifically to exercise this) produces identical response shapes to
/// before this fix -- verified by every pre-existing test in this
/// workspace continuing to pass unmodified.
fn attach_updates(
    mut response: serde_json::Value,
    notifications: Vec<serde_json::Value>,
    agent_requests: Vec<serde_json::Value>,
) -> serde_json::Value {
    if notifications.is_empty() && agent_requests.is_empty() {
        return response;
    }
    if let Some(obj) = response.as_object_mut() {
        let mut extras = serde_json::Map::new();
        if !notifications.is_empty() {
            extras.insert("updates".to_string(), serde_json::json!(notifications));
        }
        if !agent_requests.is_empty() {
            extras.insert(
                "agentRequests".to_string(),
                serde_json::json!(agent_requests),
            );
        }
        obj.insert("_acpx".to_string(), serde_json::Value::Object(extras));
    }
    response
}

/// `session/new`-specific twin of [`attach_updates`]: same additive,
/// namespaced `_acpx` merge, plus the backend's real `agentCapabilities`
/// (as captured by `ensure_backend_initialized`, see that function's doc
/// comment for why this exists) under `_acpx.agentCapabilities`. Kept as
/// a separate function rather than adding a third parameter to
/// `attach_updates` -- every other proxied method (`session/prompt` etc.)
/// has no use for a backend's `initialize`-time capabilities on every
/// single call, only the one call that starts the session at all.
/// No-op on both fronts (response left byte-for-byte untouched) when
/// there are no notifications and no captured capabilities, so every
/// pre-existing synthetic-backend test -- whose stand-in scripts don't
/// answer `initialize` with anything resembling a real `agentCapabilities`
/// object -- keeps producing identical response shapes.
fn attach_session_new_extras(
    mut response: serde_json::Value,
    notifications: Vec<serde_json::Value>,
    agent_requests: Vec<serde_json::Value>,
    agent_capabilities: Option<serde_json::Value>,
) -> serde_json::Value {
    if notifications.is_empty() && agent_requests.is_empty() && agent_capabilities.is_none() {
        return response;
    }
    if let Some(obj) = response.as_object_mut() {
        let mut extras = serde_json::Map::new();
        if !notifications.is_empty() {
            extras.insert("updates".to_string(), serde_json::json!(notifications));
        }
        if !agent_requests.is_empty() {
            extras.insert(
                "agentRequests".to_string(),
                serde_json::json!(agent_requests),
            );
        }
        if let Some(capabilities) = agent_capabilities {
            extras.insert("agentCapabilities".to_string(), capabilities);
        }
        obj.insert("_acpx".to_string(), serde_json::Value::Object(extras));
    }
    response
}

/// Free-function twin of `Router::spawn_transcript`, taking an already-
/// cloned `Option<PersistenceStore>` instead of `&self` -- shared by both
/// the original `&mut self` dispatch path and [`dispatch_shared`]'s
/// unlock-during-backend-I/O path below, so the two never drift apart.
fn spawn_transcript_fn(
    store: Option<PersistenceStore>,
    gateway_session_id: impl Into<String>,
    direction: Direction,
    payload: serde_json::Value,
) {
    let Some(store) = store else {
        return;
    };
    let gateway_session_id = gateway_session_id.into();
    tokio::spawn(async move {
        if let Err(err) = store
            .append_transcript(gateway_session_id, direction, payload, now_rfc3339())
            .await
        {
            tracing::warn!(%err, "failed to persist transcript entry");
        }
    });
}

/// Free-function twin of `Router::spawn_session_persistence` -- see
/// `spawn_transcript_fn`'s doc comment for why this split exists.
#[allow(clippy::too_many_arguments)]
fn spawn_session_persistence_fn(
    store: Option<PersistenceStore>,
    tenant_id: impl Into<String>,
    gateway_session_id: impl Into<String>,
    agent_id: impl Into<String>,
    backend_session_id: impl Into<String>,
    profile_name: Option<String>,
    client_request: serde_json::Value,
    agent_response: serde_json::Value,
) {
    let Some(store) = store else {
        return;
    };
    let gateway_session_id = gateway_session_id.into();
    let agent_id = agent_id.into();
    let backend_session_id = backend_session_id.into();
    let tenant_id = tenant_id.into();
    tokio::spawn(async move {
        if let Err(err) = store
            .record_session(
                gateway_session_id.clone(),
                agent_id,
                backend_session_id,
                profile_name,
                now_rfc3339(),
                tenant_id,
            )
            .await
        {
            tracing::warn!(%err, "failed to persist session metadata");
            return;
        }
        if let Err(err) = store
            .append_transcript(
                gateway_session_id.clone(),
                Direction::ClientToAgent,
                client_request,
                now_rfc3339(),
            )
            .await
        {
            tracing::warn!(%err, "failed to persist transcript entry");
        }
        if let Err(err) = store
            .append_transcript(
                gateway_session_id,
                Direction::AgentToClient,
                agent_response,
                now_rfc3339(),
            )
            .await
        {
            tracing::warn!(%err, "failed to persist transcript entry");
        }
    });
}

/// A failed proactive restore remains durable so a client can retry it with
/// ACP's native `session/load` or `session/resume`. Once any retry reaches
/// the backend successfully, clear the stale failure diagnostics without
/// changing unrelated active or closed session rows.
async fn mark_successful_recovery_retry(
    store: Option<PersistenceStore>,
    gateway_session_id: &str,
    method: &str,
) -> Result<(), RouterError> {
    if !matches!(method, "session/load" | "session/resume") {
        return Ok(());
    }
    let Some(store) = store else {
        return Ok(());
    };
    let Some(record) = store.get_session(gateway_session_id.to_string()).await? else {
        return Ok(());
    };
    if record.closed_at.is_none() && record.status == RecoveryStatus::RecoveryFailed {
        store
            .update_recovery_status(
                gateway_session_id.to_string(),
                RecoveryStatus::Restored,
                None,
            )
            .await?;
    }
    Ok(())
}

/// `Arc<tokio::sync::Mutex<Router>>` -- the handle type
/// `acpx-server`'s transports hold and pass to [`dispatch_shared`].
/// Re-exported here (rather than only living in `acpx-server`) so this
/// module can define `dispatch_shared` against it directly.
pub type SharedRouterHandle = std::sync::Arc<tokio::sync::Mutex<Router>>;

async fn execute_open_session_recovery(
    job: PreparedRecoveryJob,
) -> Result<PreparedRecoveryJob, RouterError> {
    let response = {
        let mut backend = job.backend.lock().await;
        ensure_backend_initialized(&mut backend, job.call_policy.clone()).await?;
        write_backend_value_locked(&mut backend, &job.request).await?;
        let (response, _, _) =
            read_matching_response(&mut backend, &job.request_id, job.call_policy.clone(), None)
                .await?;
        response
    };
    if let Some(error) = response.get("error") {
        return Err(RouterError::BackendSessionNewError(error.clone()));
    }
    Ok(job)
}

/// Restore durable sessions through the shared router without holding its
/// mutex during ACP backend I/O. Different connectors can therefore make
/// progress concurrently while each connector's own stdio remains serialized.
pub async fn recover_open_sessions_shared(
    router: &SharedRouterHandle,
    policy: StartupRecoveryPolicy,
) -> Result<StartupRecoveryReport, RouterError> {
    policy.validate()?;
    let store = {
        let router = router.lock().await;
        router.persistence.clone()
    };
    let Some(store) = store else {
        return Ok(StartupRecoveryReport::default());
    };

    let mut report = StartupRecoveryReport::default();
    let mut candidates = std::collections::VecDeque::new();
    for record in store.list_recoverable_sessions().await? {
        if record.recovery_method == RecoveryMethod::None {
            report.skipped += 1;
            continue;
        }
        let is_live = {
            let router = router.lock().await;
            let tenant_id = TenantId(record.tenant_id.clone());
            let gateway_id =
                acpx_proto::session::GatewaySessionId(record.gateway_session_id.clone());
            router.sessions.resolve(&tenant_id, &gateway_id).is_some()
        };
        if is_live {
            report.skipped += 1;
            continue;
        }
        store
            .update_recovery_status(
                record.gateway_session_id.clone(),
                RecoveryStatus::Restoring,
                None,
            )
            .await?;
        candidates.push_back(record);
    }

    if policy.fail_fast {
        while let Some(record) = candidates.pop_front() {
            let restored = run_recovery_candidate(
                router.clone(),
                store.clone(),
                record.clone(),
                policy.timeout,
            )
            .await?;
            if restored {
                report.restored += 1;
            } else {
                return Err(RouterError::RecoveryFailFast(record.gateway_session_id));
            }
        }
        return Ok(report);
    }

    let mut running = tokio::task::JoinSet::new();
    while running.len() < policy.concurrency {
        let Some(record) = candidates.pop_front() else {
            break;
        };
        running.spawn(run_recovery_candidate(
            router.clone(),
            store.clone(),
            record,
            policy.timeout,
        ));
    }
    while let Some(result) = running.join_next().await {
        match result.map_err(|error| RouterError::InvalidRecoveryPolicy(error.to_string()))?? {
            true => report.restored += 1,
            false => report.failed += 1,
        }
        if let Some(record) = candidates.pop_front() {
            running.spawn(run_recovery_candidate(
                router.clone(),
                store.clone(),
                record,
                policy.timeout,
            ));
        }
    }
    Ok(report)
}

async fn run_recovery_candidate(
    router: SharedRouterHandle,
    store: PersistenceStore,
    record: crate::persistence::SessionRecord,
    timeout: Duration,
) -> Result<bool, RouterError> {
    let outcome = tokio::time::timeout(timeout, async {
        let job = {
            let mut router = router.lock().await;
            router.prepare_open_session_recovery(&record).await?
        };
        execute_open_session_recovery(job).await
    })
    .await;

    let job = match outcome {
        Ok(Ok(job)) => job,
        Ok(Err(error)) => {
            store
                .update_recovery_status(
                    record.gateway_session_id,
                    RecoveryStatus::RecoveryFailed,
                    Some(error.to_string()),
                )
                .await?;
            return Ok(false);
        }
        Err(_) => {
            let supervisor_key = {
                let mut router = router.lock().await;
                let key = router.recovery_supervisor_key(&record);
                if let Err(error) = router.supervisor.stop(&key).await {
                    tracing::warn!(%error, gateway_session_id = %record.gateway_session_id, "failed to stop timed-out recovery backend");
                }
                key
            };
            tracing::warn!(
                gateway_session_id = %record.gateway_session_id,
                supervisor_key,
                "startup recovery timed out and the backend process was stopped"
            );
            let error = RouterError::RecoveryTimeout(record.gateway_session_id.clone());
            store
                .update_recovery_status(
                    record.gateway_session_id,
                    RecoveryStatus::RecoveryFailed,
                    Some(error.to_string()),
                )
                .await?;
            return Ok(false);
        }
    };

    store
        .update_recovery_status(
            record.gateway_session_id.clone(),
            RecoveryStatus::Restored,
            None,
        )
        .await?;
    let mut router = router.lock().await;
    // See `dispatch_session_new`'s identical cancellation.
    let supervisor_key_for_cancel = job.entry.agent_id.clone();
    router.sessions.insert(
        &job.tenant_id,
        acpx_proto::session::GatewaySessionId(record.gateway_session_id),
        job.entry,
    );
    router.cancel_unreferenced_shutdown(&supervisor_key_for_cancel);
    job.admission.commit();
    Ok(true)
}

/// Durable identity and drift state needed by a persistent transport before
/// it attaches a resumable session-update subscription.
#[derive(Debug, Clone, Default)]
pub struct StreamResumeState {
    pub backend_session_id: Option<String>,
    pub durable_state_changed: bool,
}

/// Inspect the session registry and optional persistence store without
/// holding the router mutex across SQLite I/O. A transcript count mismatch
/// means a non-ACPX writer changed durable history since the last observed
/// state and must invalidate any resume cursor.
pub async fn stream_resume_state_shared(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    gateway_session_id: &str,
) -> StreamResumeState {
    let (backend_session_id, persistence) = {
        let router = router.lock().await;
        let gateway_id = acpx_proto::session::GatewaySessionId(gateway_session_id.to_string());
        (
            router
                .sessions
                .resolve(tenant_id, &gateway_id)
                .map(|entry| entry.backend_session_id.0.clone()),
            router.persistence.clone(),
        )
    };
    let durable_state_changed = match persistence {
        Some(store) => match store.transcript_state_changed(gateway_session_id).await {
            Ok(changed) => changed,
            Err(err) => {
                tracing::warn!(%err, %gateway_session_id, "failed to inspect durable transcript state for stream resume");
                false
            }
        },
        None => false,
    };
    StreamResumeState {
        backend_session_id,
        durable_state_changed,
    }
}

/// Real multi-agent concurrency entry point (added post-Phase-6, replacing
/// the naive "hold the whole-`Router` mutex for an entire `dispatch` call,
/// including the backend's real-LLM-latency I/O" pattern every transport
/// used through Phase 6 -- see `acpx/COVERAGE.md`'s "real multi-agent
/// concurrency" section for the full writeup of why that was a genuine
/// correctness/scalability bug, not just a style preference, for a
/// "virtual gateway daemon" whose entire purpose is fronting *multiple*
/// concurrently-used backend agents).
///
/// Same [`RouterError`] contract as [`Router::dispatch`] (in fact
/// delegates to it for [`MethodClass::GatewayNative`] and
/// [`MethodClass::Unknown`], which never touch a backend process and stay
/// cheap/local). For [`MethodClass::Hybrid`] (`session/new`) and
/// [`MethodClass::Proxied`] (`session/prompt` and friends) -- the only
/// method classes that ever talk to a backend agent process -- this
/// function locks `router` only long enough to resolve gateway state
/// (session registry, profile/provider resolution, `Supervisor::
/// ensure_running`'s bookkeeping) and obtain a
/// `acpx_conductor::SharedBackendProcess` handle, then **drops that lock**
/// before doing the actual backend stdio round trip against just that
/// handle's own per-process mutex. Two concurrent callers resolving to two
/// *different* backend agents now genuinely run their I/O in parallel;
/// two callers resolving to the *same* backend agent still correctly
/// serialize on that one process's own lock (see
/// `acpx_conductor::supervisor`'s module doc comment for why that part is
/// unavoidable, not a remaining bug).
///
/// `acpx-server`'s HTTP/WS/stdio transports all call this instead of
/// `router.lock().await.dispatch(...)`; `Router::dispatch` itself is left
/// untouched and still used directly by every in-process test in this
/// workspace that constructs a bare `Router` (no sharing, no concurrency
/// to speak of), so none of those ~100 existing tests needed to change.
pub async fn dispatch_shared(
    router: &SharedRouterHandle,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    dispatch_shared_for_tenant(router, &TenantId::default_tenant(), request).await
}

/// **Phase B (`acpx-tenant-isolation`).** Tenant-aware entry point for the
/// shared (`Arc<Mutex<Router>>`-based) dispatch path -- mirrors
/// [`Router::dispatch_for_tenant`]'s relationship to [`Router::dispatch`]:
/// [`dispatch_shared`] stays a thin default-tenant wrapper so every
/// pre-existing (tenant-unaware) caller keeps working unchanged; only
/// `acpx-server`'s transports, which extract a real `X-Acpx-Tenant`
/// header, call this directly.
/// Lightweight, redacted operational visibility for every request that
/// reaches [`dispatch_shared_for_tenant`] -- the single dispatch entry
/// point every real transport (HTTP/WS/stdio, native and ACP-bridge)
/// funnels through. `session/prompt` gets its own distinct log line
/// (`session_id`/`prompt_preview`) instead of the generic
/// `method=...`/`tenant=...` shape every other method gets, since a raw
/// prompt body is exactly the kind of thing an operator tailing
/// production logs needs a *preview* of (to correlate a live incident
/// with a specific user-visible turn) without the full-fidelity content
/// (which may be arbitrarily large, and isn't itself an operational
/// signal past its first few tokens).
fn log_request_received(method: &str, tenant_id: &TenantId, request: &serde_json::Value) {
    if method == "session/prompt" {
        let session_id = request
            .pointer("/params/sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let prompt_preview = request
            .pointer("/params/prompt")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        tracing::info!(
            %session_id,
            tenant = %tenant_id.0,
            %prompt_preview,
            "acpx received session/prompt"
        );
    } else {
        tracing::info!(%method, tenant = %tenant_id.0, "acpx received request");
    }
}

pub async fn dispatch_shared_for_tenant(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or(RouterError::MissingMethod)?
        .to_string();
    log_request_received(&method, tenant_id, &request);
    match classify(&method) {
        MethodClass::Hybrid => dispatch_session_new_shared(router, tenant_id, request).await,
        // **Phase 7:** `session/cancel` needs `dispatch_session_cancel_shared`
        // specifically, not the generic `dispatch_proxied_shared` --
        // see `Router::dispatch_session_cancel`'s doc comment for why
        // (id-optional notification shape, no blocking wait for a reply
        // that will never come, and -- the part that actually makes
        // cancellation *work*, not just avoid erroring -- a write path
        // independent of the per-process lock a concurrent
        // `session/prompt` against the same backend may be holding for
        // its entire duration).
        MethodClass::Proxied if method == "session/cancel" => {
            dispatch_session_cancel_shared(router, tenant_id, request).await
        }
        MethodClass::Proxied => dispatch_proxied_shared(router, tenant_id, request).await,
        MethodClass::SessionFork => dispatch_session_fork_shared(router, tenant_id, request).await,
        // **Phase 13.** Mirrors `dispatch_native`'s `"session/list"`
        // branching (see `session_list_selector`'s doc comment) but only
        // when a selector is actually present -- an unqualified
        // `session/list` stays on the generic `GatewayNative` path just
        // below, cheap/local exactly as before. When a selector *is*
        // present this is genuinely a backend round trip (a real
        // `Proxied`-shaped call under the hood), so it gets its own
        // lock-briefly-then-release `_shared` variant like every other
        // backend-talking path in this function -- routing it through
        // the generic `router.lock().await.dispatch(request).await`
        // arm instead would hold the *entire* router lock for the whole
        // backend round trip, blocking every other concurrent client
        // (including ones talking to unrelated backends) for no reason,
        // exactly the regression this function's own doc comment above
        // exists to prevent.
        MethodClass::GatewayNative
            if method == "session/list"
                && request
                    .get("params")
                    .and_then(session_list_selector)
                    .is_some() =>
        {
            dispatch_session_list_real_shared(router, tenant_id, request).await
        }
        // **`client_and_installer_contract` hardening, `acp-gateway-daemon`
        // plan.** `agents/install` is a genuine (potentially many-second,
        // real network/filesystem) download+extract, not the cheap/local
        // registry-cache read every other `GatewayNative` method actually
        // is -- routing it through the generic
        // `router.lock().await.dispatch_for_tenant(...)` arm below would
        // hold the *entire* router mutex for that whole duration,
        // freezing every other concurrent client (every tenant, every
        // session, every unrelated backend) until the install finishes.
        // Found during this hardening pass, not a pre-existing documented
        // risk -- fixed the same way `session/list`'s real-backend arm
        // just above already established: resolve what's needed under a
        // brief lock, release it, then do the slow part unlocked. The
        // wire contract (`{id, outcome}`) is unchanged -- see
        // `dispatch_agents_install_shared`'s doc comment for why a
        // fuller async job/progress model remains a deliberately
        // deferred, separate enhancement.
        MethodClass::GatewayNative if method == "agents/install" => {
            dispatch_agents_install_shared(router, request).await
        }
        MethodClass::GatewayNative | MethodClass::Unknown => {
            router
                .lock()
                .await
                .dispatch_for_tenant(tenant_id, request)
                .await
        }
    }
}

/// [`dispatch_shared_for_tenant`]'s `agents/install` path -- resolves the
/// requested [`acpx_registry::Agent`] under a brief router lock (mirrors
/// `Router::dispatch_native`'s `"agents/install"` arm exactly, including
/// its `{id, outcome}` response shape and every error case), then
/// releases that lock *before* the actual `acpx_registry::install` call,
/// which is the one that can genuinely take seconds (npm/pip resolution,
/// or a full binary download+extract).
///
/// **Not a polling/streamed job**, deliberately: this still blocks the
/// *calling* HTTP/WS/stdio request until the install finishes, same as
/// before this fix -- only the *router-wide* blocking (every other
/// concurrent client on this whole daemon) is what this function
/// resolves. A durable job id + `agents/install/status` progress-polling
/// API remains the documented, still-open, separate enhancement (see
/// `acpx-client::ext::registry::install`'s doc comment) -- deliberately
/// out of scope here to avoid a breaking wire-contract change across
/// `acpx-proto`'s `AgentInstallResult` type and every existing caller/
/// test that depends on today's synchronous shape, for what would be a
/// pure UX improvement (progress feedback) rather than a correctness fix
/// like the mutex-holding bug this function *does* resolve.
async fn dispatch_agents_install_shared(
    router: &SharedRouterHandle,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
    let agent = {
        let mut r = router.lock().await;
        let agent_id = request
            .get("params")
            .and_then(|p| p.get("id"))
            .and_then(|i| i.as_str())
            .ok_or(RouterError::MissingAgentId)?
            .to_string();
        r.ensure_registry_loaded().await;
        r.registry_cache
            .as_ref()
            .expect("just loaded")
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .cloned()
            .ok_or(RouterError::UnknownAgentId(agent_id))?
    };
    let outcome = acpx_registry::install(&agent).await?;
    Ok(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "id": agent.id, "outcome": format!("{outcome:?}") },
    }))
}

/// [`dispatch_shared`]'s `session/cancel` path -- mirrors
/// `Router::dispatch_session_cancel` exactly (see that method's doc
/// comment for the full rationale) but restructured the same way every
/// other `_shared` function in this file is: resolve session/agent state
/// under `router`'s own brief lock, then release it before touching a
/// backend at all. The release matters even more here than for the
/// generic proxied path: `Supervisor::cancel_writer`'s entire purpose is
/// letting this write proceed without contending with a concurrent
/// `session/prompt`'s per-process lock, so holding `router`'s own lock
/// any longer than strictly necessary to look up the writer handle would
/// undermine that (a `session/prompt` against a *different* agent, or a
/// `session/new` for a brand new one, would otherwise queue up behind
/// this cancel call for no reason).
async fn dispatch_session_cancel_shared(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let client_id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let gateway_session_id = request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .ok_or(RouterError::MissingSessionId)?
        .to_string();

    let (backend_session_id, persistence, cancel_writer) = {
        let r = router.lock().await;
        let entry = r
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
            )
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.clone()))?;
        let backend_session_id = entry.backend_session_id.0.clone();
        let cancel_writer = r.supervisor.cancel_writer(&entry.agent_id);
        (backend_session_id, r.persistence.clone(), cancel_writer)
    };

    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": backend_session_id }
    });
    spawn_transcript_fn(
        persistence,
        gateway_session_id,
        Direction::ClientToAgent,
        notification.clone(),
    );

    // Same "nothing running, nothing to cancel" no-op as
    // `Router::dispatch_session_cancel` -- see that method's comment.
    if let Some(writer) = cancel_writer {
        write_cancel_notification_best_effort(&writer, &notification).await;
    }

    Ok(serde_json::json!({ "jsonrpc": "2.0", "id": client_id, "result": {} }))
}

/// [`dispatch_shared`]'s real, per-backend `session/list` path -- see
/// `Router::dispatch_session_list_real`'s doc comment for the full
/// rationale (this mirrors it exactly: `_acpx` selector resolution,
/// forward, backend-id -> gateway-id translation) restructured the same
/// way every other `_shared` function in this file is, to release
/// `router`'s lock before the backend round trip rather than holding it
/// for the call's entire duration.
async fn dispatch_session_list_real_shared(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
    let mut params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let selector = session_list_selector(&params).expect(
        "dispatch_shared only routes to this function when session_list_selector(params) is Some",
    );
    if let Some(obj) = params.as_object_mut() {
        obj.remove("_acpx");
    }

    let (agent_id, profile_name, backend, call_policy, agent_relay, notification_hub, process_reader_demux) = {
        let mut r = router.lock().await;
        let (agent_id, profile) = match selector {
            SessionListSelector::Profile(name) => {
                let (key, profile) = r.resolve_profile(&name, tenant_id).await?;
                (key, Some(profile))
            }
            SessionListSelector::AgentId(explicit_id) => {
                r.ensure_agent_enabled(&explicit_id).await?;
                r.ensure_custom_agent_registered(&explicit_id).await?;
                (explicit_id, None)
            }
        };
        let profile_name = profile.as_ref().map(|p| p.name.clone());
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        // Same demux-vs-idle-scavenger dedup as `dispatch_proxied_shared`'s
        // identical branch -- the demux consumer subsumes this job once
        // active for this process.
        if !r.process_reader_demux {
            r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        }
        let call_policy = r.call_policy(profile.as_ref());
        let process_reader_demux = r.process_reader_demux;
        (
            agent_id,
            profile_name,
            backend,
            call_policy,
            r.agent_request_hub.clone(),
            r.notification_hub.clone(),
            process_reader_demux,
        )
    };

    let outbound = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/list",
        "params": params,
    });

    let response = {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        if process_reader_demux {
            // **`process_reader_demux`.** Must not fall through to
            // `read_matching_response`'s `reader_mut()` below -- once any
            // other call against this same shared backend process has
            // already activated demux (`BackendProcess::start_demux`),
            // `proc.reader` is `None` and that call panics outright. This
            // is the real crash `process_reader_demux`'s field doc
            // comment used to flag as a known, deliberately-deferred gap
            // ("session-fork/session-list paths are unaffected either
            // way") -- closed now that the flag defaults on, so every
            // dispatch path sharing a process must agree on which regime
            // that process is in.
            if proc.pending.is_none() {
                let rx = proc.start_demux();
                spawn_demux_consumer(
                    std::sync::Arc::clone(router),
                    agent_id.clone(),
                    agent_relay.clone(),
                    notification_hub.clone(),
                    std::sync::Arc::clone(&backend),
                    call_policy.clone(),
                    rx,
                );
            }
            let pending = proc
                .pending
                .clone()
                .expect("just activated demux above, or it was already active");
            let writer = proc.writer_handle();
            drop(proc);
            let rx = pending.register(&id).await;
            write_backend_value_via_handle(&backend, &writer, &outbound).await?;
            match tokio::time::timeout(BACKEND_IDLE_READ_TIMEOUT, acpx_conductor::demux::recv(rx))
                .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(acpx_conductor::DemuxRecvError::ReaderClosed)) => {
                    return Err(RouterError::BackendDemuxReaderClosed);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = BACKEND_IDLE_READ_TIMEOUT.as_secs(),
                        "backend produced no output for the entire idle-read timeout window \
                         (process-reader-demux path, session/list); killing the wedged \
                         process so every other session sharing it is unblocked"
                    );
                    let mut proc = backend.lock().await;
                    let _ = proc.kill().await;
                    return Err(RouterError::BackendIdleReadTimeout(BACKEND_IDLE_READ_TIMEOUT));
                }
            }
        } else {
            write_backend_value_locked(&mut proc, &outbound).await?;
            let (response, _notifications, _agent_requests) =
                read_matching_response(&mut proc, &id, call_policy, None).await?;
            response
        }
    };

    if let Some(error) = response.get("error") {
        return Err(RouterError::BackendSessionListError(error.clone()));
    }

    let mut result = response
        .get("result")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "sessions": [] }));
    if let Some(raw_sessions) = result.get("sessions").and_then(|s| s.as_array()) {
        let mut r = router.lock().await;
        let mut filtered = Vec::with_capacity(raw_sessions.len());
        for session in raw_sessions.iter().cloned() {
            let mut session = session;
            let Some(backend_sid) = session
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(str::to_string)
            else {
                filtered.push(session);
                continue;
            };
            let session_cwd = session
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            // **Phase B leak fix**, same rationale as
            // `Router::dispatch_session_list_real`'s equivalent fix.
            let Some(gateway_id) = r.translate_or_register_backend_session(
                tenant_id,
                &agent_id,
                &backend_sid,
                profile_name.clone(),
                session_cwd,
            ) else {
                continue;
            };
            session["sessionId"] = serde_json::Value::String(gateway_id.clone());
            spawn_session_persistence_fn(
                r.persistence.clone(),
                tenant_id.0.clone(),
                gateway_id,
                agent_id.clone(),
                backend_sid,
                profile_name.clone(),
                outbound.clone(),
                response.clone(),
            );
            filtered.push(session);
        }
        if let Some(obj) = result.as_object_mut() {
            obj.insert("sessions".to_string(), serde_json::json!(filtered));
        }
    }

    Ok(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// [`dispatch_shared`]'s `session/prompt`/`session/resume`/`session/load`/
/// `session/close`/`session/set_mode`/`session/cancel` path. Mirrors
/// `Router::dispatch_proxied` exactly (session resolution, sessionId
/// rewrite, transcript persistence, `session/close` bookkeeping) but
/// restructured to release `router`'s lock before the backend round trip.
async fn dispatch_proxied_shared(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    mut request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or(RouterError::MissingMethod)?
        .to_string();
    if method == "session/close" {
        let mut r = router.lock().await;
        if let Some(response) = r.maybe_suppress_close(tenant_id, &mut request).await? {
            return Ok(response);
        }
        drop(r);
    }
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
    let gateway_session_id = request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .ok_or(RouterError::MissingSessionId)?
        .to_string();

    let (backend, persistence, call_policy, agent_id, agent_relay, notification_hub, process_reader_demux) = {
        let mut r = router.lock().await;
        let entry = match r.sessions.resolve(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
        ) {
            Some(entry) => entry.clone(),
            None => {
                r.rehydrate_session(tenant_id, &method, &gateway_session_id)
                    .await?
            }
        };
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        let profile_name = entry.profile_name.clone();
        if let Some(params) = request.get_mut("params") {
            params["sessionId"] = serde_json::Value::String(backend_session_id);
        }
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        // The demux consumer (spawned below, once handshake completes)
        // subsumes the idle scavenger's job for this process -- spawning
        // both would leave two tasks competing for the same lock to do
        // overlapping work.
        if !r.process_reader_demux {
            r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        }
        r.sessions.set_in_flight(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
            1,
        );
        let call_policy = r.call_policy_for(profile_name.as_deref(), &agent_id).await;
        let process_reader_demux = r.process_reader_demux;
        (
            backend,
            r.persistence.clone(),
            call_policy,
            agent_id,
            r.agent_request_hub.clone(),
            r.notification_hub.clone(),
            process_reader_demux,
        )
    };

    spawn_transcript_fn(
        persistence.clone(),
        gateway_session_id.clone(),
        Direction::ClientToAgent,
        request.clone(),
    );

    let response_result = async {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        if process_reader_demux {
            // **Phase 1, `process_reader_demux`.** Register-then-await
            // instead of holding `proc` (the per-process lock) across the
            // write and the entire blocking read loop -- see
            // `memory/acpx/gen/acpx-concurrency-config-execution.meta.json`.
            // This is the change that lets two sessions sharing this
            // backend process actually overlap in wall time.
            if proc.pending.is_none() {
                let rx = proc.start_demux();
                spawn_demux_consumer(
                    std::sync::Arc::clone(router),
                    agent_id.clone(),
                    agent_relay.clone(),
                    notification_hub.clone(),
                    std::sync::Arc::clone(&backend),
                    call_policy.clone(),
                    rx,
                );
            }
            let pending = proc
                .pending
                .clone()
                .expect("just activated demux above, or it was already active");
            let writer = proc.writer_handle();
            drop(proc);
            let idle_timeout = session_establish_or_default_idle_timeout(&method);
            let rx = pending.register(&id).await;
            write_backend_value_via_handle(&backend, &writer, &request).await?;
            let response = match tokio::time::timeout(idle_timeout, acpx_conductor::demux::recv(rx)).await
            {
                Ok(Ok(value)) => value,
                Ok(Err(acpx_conductor::DemuxRecvError::ReaderClosed)) => {
                    return Err(RouterError::BackendDemuxReaderClosed);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = idle_timeout.as_secs(),
                        "backend produced no output for the entire idle-read timeout window \
                         (process-reader-demux path); killing the wedged process so every \
                         other session sharing it is unblocked"
                    );
                    let mut proc = backend.lock().await;
                    let _ = proc.kill().await;
                    return Err(RouterError::BackendIdleReadTimeout(idle_timeout));
                }
            };
            // Unmatched frames (notifications/agent-requests) are handled
            // entirely by the independent demux consumer task, not
            // observed by this call's own read loop -- but any
            // `session/update` the consumer couldn't deliver live gets
            // buffered per gateway session (`Router::pending_updates`),
            // so a `POST /rpc` caller with no live push channel still
            // sees it here instead of it being silently discarded.
            let buffered_updates = {
                let r = router.lock().await;
                r.pending_updates()
                    .drain(tenant_id, &gateway_session_id)
                    .await
            };
            Ok::<_, RouterError>(attach_updates(response, buffered_updates, Vec::new()))
        } else {
            write_backend_value_locked(&mut proc, &request).await?;
            let live = LiveNotifyCtx {
                router: std::sync::Arc::clone(router),
                agent_id,
                tenant_id: Some(tenant_id.clone()),
                agent_relay,
                gateway_session_id: Some(gateway_session_id.clone()),
                notification_hub,
                backend: std::sync::Arc::clone(&backend),
            };
            let (response, notifications, agent_requests) =
                read_matching_response_with_idle_timeout(
                    &mut proc,
                    &id,
                    call_policy,
                    Some(&live),
                    session_establish_or_default_idle_timeout(&method),
                )
                .await?;
            Ok::<_, RouterError>(attach_updates(response, notifications, agent_requests))
        }
    }
    .await;
    {
        let mut r = router.lock().await;
        r.sessions.set_in_flight(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
            0,
        );
    }
    if let Some(store) = persistence.clone() {
        store
            .update_session_activity(gateway_session_id.clone(), now_unix_nanos())
            .await?;
    }
    let response = response_result?;
    mark_successful_recovery_retry(persistence.clone(), &gateway_session_id, &method).await?;

    spawn_transcript_fn(
        persistence.clone(),
        gateway_session_id.clone(),
        Direction::AgentToClient,
        response.clone(),
    );

    if method == "session/close" {
        if let Some(store) = persistence.clone() {
            store
                .close_session(gateway_session_id.clone(), now_rfc3339())
                .await?;
        }
        // Same leak/correctness fix as `Router::dispatch_proxied` above --
        // see that call site's comment. Re-acquire the router lock
        // briefly (bookkeeping only, no backend I/O held) to evict the
        // closed session from the shared `SessionRegistry` too, so the
        // two dispatch paths never drift apart on this behavior.
        let mut r = router.lock().await;
        if let Some(removed) = r.sessions.remove(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
        ) {
            r.release_live_session(tenant_id);
            r.stop_if_session_scoped(&removed.agent_id).await;
            r.mark_unreferenced_if_idle(&removed.agent_id);
        }
    }
    Ok(response)
}

/// [`dispatch_shared`]'s `session/fork` path. Mirrors
/// `Router::dispatch_session_fork` exactly (see that method's doc
/// comment for the full rationale) but restructured -- release
/// `router`'s own lock before the backend round trip -- the same way
/// every other `_shared` function in this file is, per this function's
/// own module-level pattern.
async fn dispatch_session_fork_shared(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    mut request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
    let gateway_session_id = request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .ok_or(RouterError::MissingSessionId)?
        .to_string();
    let cwd = request
        .get("params")
        .and_then(|p| p.get("cwd"))
        .and_then(|c| c.as_str())
        .map(str::to_string);

    let (
        backend,
        persistence,
        call_policy,
        agent_id,
        profile_name,
        admission,
        agent_relay,
        notification_hub,
        process_reader_demux,
    ) = {
        let mut r = router.lock().await;
        // Reserve the fork before potentially rehydrating its persisted
        // source so a rejected fork cannot leave the source registered.
        let admission = r.admit_session(tenant_id)?;
        let entry = match r.sessions.resolve(
            tenant_id,
            &acpx_proto::session::GatewaySessionId(gateway_session_id.clone()),
        ) {
            Some(entry) => entry.clone(),
            None => {
                r.rehydrate_session(tenant_id, "session/fork", &gateway_session_id)
                    .await?
            }
        };
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        let profile_name = entry.profile_name.clone();
        if let Some(params) = request.get_mut("params") {
            params["sessionId"] = serde_json::Value::String(backend_session_id);
        }
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        // Same demux-vs-idle-scavenger dedup as `dispatch_proxied_shared`'s
        // identical branch -- the demux consumer subsumes this job once
        // active for this process.
        if !r.process_reader_demux {
            r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        }
        let call_policy = r.call_policy_for(profile_name.as_deref(), &agent_id).await;
        let process_reader_demux = r.process_reader_demux;
        (
            backend,
            r.persistence.clone(),
            call_policy,
            agent_id,
            profile_name,
            admission,
            r.agent_request_hub.clone(),
            r.notification_hub.clone(),
            process_reader_demux,
        )
    };

    let mut response = async {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        if process_reader_demux {
            // **`process_reader_demux`.** Same crash this closes in
            // `dispatch_session_list_real_shared` -- `proc.reader` is
            // already `None` on any process another call already
            // demuxed, so falling through to `read_matching_response`'s
            // `reader_mut()` below would panic instead of forking. No
            // `LiveNotifyCtx`/inline notifications here, deliberately --
            // same reasoning as `dispatch_session_new_shared`'s own doc
            // comment: this call is what *creates* the new forked
            // gateway session (`sessions.register` below), so no
            // transport connection could possibly have subscribed to it
            // yet; any interleaved notifications the backend emits while
            // forking are handled by the independent demux consumer.
            if proc.pending.is_none() {
                let rx = proc.start_demux();
                spawn_demux_consumer(
                    std::sync::Arc::clone(router),
                    agent_id.clone(),
                    agent_relay.clone(),
                    notification_hub.clone(),
                    std::sync::Arc::clone(&backend),
                    call_policy.clone(),
                    rx,
                );
            }
            let pending = proc
                .pending
                .clone()
                .expect("just activated demux above, or it was already active");
            let writer = proc.writer_handle();
            drop(proc);
            let rx = pending.register(&id).await;
            write_backend_value_via_handle(&backend, &writer, &request).await?;
            let response = match tokio::time::timeout(
                BACKEND_IDLE_READ_TIMEOUT,
                acpx_conductor::demux::recv(rx),
            )
            .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(acpx_conductor::DemuxRecvError::ReaderClosed)) => {
                    return Err(RouterError::BackendDemuxReaderClosed);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = BACKEND_IDLE_READ_TIMEOUT.as_secs(),
                        "backend produced no output for the entire idle-read timeout window \
                         (process-reader-demux path, session/fork); killing the wedged \
                         process so every other session sharing it is unblocked"
                    );
                    let mut proc = backend.lock().await;
                    let _ = proc.kill().await;
                    return Err(RouterError::BackendIdleReadTimeout(BACKEND_IDLE_READ_TIMEOUT));
                }
            };
            Ok::<_, RouterError>(attach_updates(response, Vec::new(), Vec::new()))
        } else {
            write_backend_value_locked(&mut proc, &request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response(&mut proc, &id, call_policy, None).await?;
            Ok::<_, RouterError>(attach_updates(response, notifications, agent_requests))
        }
    }
    .await?;

    let forked_backend_session_id = extract_backend_session_id(&response)?;
    let (forked_gateway_session_id_str, persist_args) = {
        let mut r = router.lock().await;
        let forked_gateway_id = r.sessions.register(
            tenant_id,
            agent_id,
            BackendSessionId(forked_backend_session_id),
            profile_name.clone(),
            cwd,
        );
        admission.commit();
        let forked_gateway_session_id_str = forked_gateway_id.0.clone();
        if let Some(result) = response.get_mut("result") {
            result["sessionId"] = serde_json::Value::String(forked_gateway_id.0);
        }
        let persist_args = r
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(forked_gateway_session_id_str.clone()),
            )
            .map(|entry| (entry.agent_id.clone(), entry.backend_session_id.0.clone()));
        (forked_gateway_session_id_str, persist_args)
    };

    if let Some((persisted_agent_id, persisted_backend_session_id)) = persist_args {
        spawn_session_persistence_fn(
            persistence,
            tenant_id.0.clone(),
            forked_gateway_session_id_str,
            persisted_agent_id,
            persisted_backend_session_id,
            profile_name,
            request,
            response.clone(),
        );
    }

    Ok(response)
}

/// [`dispatch_shared`]'s `session/new` path. Mirrors
/// `Router::dispatch_session_new` exactly (`_acpx.profile` resolution,
/// central MCP server merge, gateway session id issuance, session
/// persistence) but restructured to release `router`'s lock before the
/// backend round trip.
async fn dispatch_session_new_shared(
    router: &SharedRouterHandle,
    tenant_id: &TenantId,
    mut request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;

    let (
        agent_id,
        profile,
        backend,
        persistence,
        cwd,
        admission,
        call_policy,
        pre_minted_gateway_id,
        agent_relay,
        notification_hub,
        process_reader_demux,
    ) = {
        let mut r = router.lock().await;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let cwd = params
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        let profile_name = params
            .get("_acpx")
            .and_then(|ext| ext.get("profile"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        let explicit_agent_id = params
            .get("_acpx")
            .and_then(|ext| ext.get("agentId"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        if profile_name.is_some() && explicit_agent_id.is_some() {
            return Err(RouterError::ConflictingSessionSelection);
        }
        let selected_agent_id = match (&profile_name, explicit_agent_id.as_deref()) {
            (Some(name), None) => {
                r.ensure_default_profiles_seeded().await;
                r.profiles
                    .get(name)
                    .map(|profile| profile.agent_id.clone())
                    .ok_or_else(|| RouterError::UnknownProfile(name.clone()))?
            }
            (None, Some(agent_id)) => agent_id.to_owned(),
            (None, None) => r.default_agent_id.clone(),
            (Some(_), Some(_)) => unreachable!("checked above"),
        };
        r.ensure_agent_enabled(&selected_agent_id).await?;
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }

        let (agent_id, profile) = match (&profile_name, explicit_agent_id) {
            (Some(name), None) => {
                let (supervisor_key, profile) = r.resolve_profile(name, tenant_id).await?;
                (supervisor_key, Some(profile))
            }
            (None, Some(agent_id)) => {
                r.ensure_custom_agent_registered(&agent_id).await?;
                (agent_id, None)
            }
            (None, None) => {
                let agent_id = r.default_agent_id.clone();
                r.ensure_custom_agent_registered(&agent_id).await?;
                (agent_id, None)
            }
            (Some(_), Some(_)) => unreachable!("checked before _acpx stripping"),
        };

        // See `dispatch_session_new`'s identical block for the full
        // rationale -- this is the shared (`Arc<Mutex<Router>>`-based)
        // dispatch path's mirror of that same per-session backend
        // process isolation logic, kept in lockstep since production
        // transports call this function, not `Router::dispatch` directly.
        let mut pre_minted_gateway_id: Option<String> = None;
        let agent_id = if r.session_process_isolation && profile.is_some() {
            let gid = uuid::Uuid::new_v4().to_string();
            let session_scoped_key = format!("{agent_id}:session:{gid}");
            if let Some(spec) = r.supervisor.spec(&agent_id).cloned() {
                r.supervisor.register(session_scoped_key.clone(), spec);
            }
            pre_minted_gateway_id = Some(gid);
            session_scoped_key
        } else {
            agent_id
        };

        if let Some(profile) = &profile {
            if !profile.mcp_servers.is_empty() {
                let central = r.mcp_servers.list_named(&profile.mcp_servers);
                let params = request
                    .get_mut("params")
                    .ok_or(RouterError::MissingParams)?;
                let client_servers = params
                    .get("mcpServers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let merged = crate::mcp_servers::merge_mcp_servers(&client_servers, &central);
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("mcpServers".to_string(), serde_json::json!(merged));
                }
            }
        }

        let admission = r.admit_session(tenant_id)?;
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        // The demux consumer (spawned below, once handshake completes)
        // subsumes the idle scavenger's job for this process -- see
        // `dispatch_proxied_shared`'s identical branch.
        if !r.process_reader_demux {
            r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        }
        // See `dispatch_session_new`'s identical fallback for why: native/
        // unmanaged mode still picks up whatever profile
        // `ensure_default_profiles_seeded` auto-seeded under this
        // `agent_id`, for `call_policy` purposes only. Doesn't trigger the
        // seeding itself -- see `Router::warm_default_profiles`.
        let call_policy_profile = profile
            .clone()
            .or_else(|| r.profiles.get(&agent_id).cloned());
        let call_policy = r.call_policy(call_policy_profile.as_ref());
        let process_reader_demux = r.process_reader_demux;
        (
            agent_id,
            profile,
            backend,
            r.persistence.clone(),
            cwd,
            admission,
            call_policy,
            pre_minted_gateway_id,
            r.agent_request_hub.clone(),
            r.notification_hub.clone(),
            process_reader_demux,
        )
    };

    let mut response = async {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        // No `LiveNotifyCtx` on the non-demux path below, deliberately:
        // this exact call is what *creates* the gateway session
        // (`self.sessions.register` below, after this block returns) --
        // until that registration happens, no gateway session id exists
        // yet for `try_deliver_live`'s `find_by_backend` lookup to ever
        // find, and no transport connection could possibly have
        // subscribed to it yet either (a connection only learns the
        // gateway session id from *this* call's own response). Passing a
        // live ctx here would be dead code that always falls back to
        // buffering -- `session/prompt`/`session/resume`/`session/load`
        // (`dispatch_proxied_shared`, which *does* pass one) are where
        // live delivery actually matters, since those always target an
        // already-registered session. The demux consumer below has the
        // same `tenant_id: None`/`gateway_session_id: None` shape for the
        // same reason -- it's process-scoped, not call-scoped, so it
        // never had a live ctx to pass here in the first place.
        if process_reader_demux {
            if proc.pending.is_none() {
                let rx = proc.start_demux();
                spawn_demux_consumer(
                    std::sync::Arc::clone(router),
                    agent_id.clone(),
                    agent_relay.clone(),
                    notification_hub.clone(),
                    std::sync::Arc::clone(&backend),
                    call_policy.clone(),
                    rx,
                );
            }
            let pending = proc
                .pending
                .clone()
                .expect("just activated demux above, or it was already active");
            let writer = proc.writer_handle();
            drop(proc);
            let rx = pending.register(&id).await;
            write_backend_value_via_handle(&backend, &writer, &request).await?;
            let response = match tokio::time::timeout(
                SESSION_ESTABLISH_IDLE_READ_TIMEOUT,
                acpx_conductor::demux::recv(rx),
            )
            .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(acpx_conductor::DemuxRecvError::ReaderClosed)) => {
                    return Err(RouterError::BackendDemuxReaderClosed);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = SESSION_ESTABLISH_IDLE_READ_TIMEOUT.as_secs(),
                        "backend produced no output for the entire idle-read timeout window \
                         (process-reader-demux path, session/new); killing the wedged \
                         process so every other session sharing it is unblocked"
                    );
                    let mut proc = backend.lock().await;
                    let _ = proc.kill().await;
                    return Err(RouterError::BackendIdleReadTimeout(
                        SESSION_ESTABLISH_IDLE_READ_TIMEOUT,
                    ));
                }
            };
            let agent_capabilities = {
                let proc = backend.lock().await;
                proc.agent_capabilities.clone()
            };
            Ok::<_, RouterError>(attach_session_new_extras(
                response,
                Vec::new(),
                Vec::new(),
                agent_capabilities,
            ))
        } else {
            write_backend_value_locked(&mut proc, &request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response_with_idle_timeout(
                    &mut proc,
                    &id,
                    call_policy,
                    None,
                    SESSION_ESTABLISH_IDLE_READ_TIMEOUT,
                )
                .await?;
            Ok::<_, RouterError>(attach_session_new_extras(
                response,
                notifications,
                agent_requests,
                proc.agent_capabilities.clone(),
            ))
        }
    }
    .await?;

    let backend_session_id = extract_backend_session_id(&response)?;

    let (gateway_session_id_str, entry) = {
        let mut r = router.lock().await;
        // See `dispatch_session_new`'s identical cancellation for why.
        let supervisor_key_for_cancel = agent_id.clone();
        let gateway_id = match pre_minted_gateway_id.clone() {
            Some(gid) => r.sessions.register_with_id(
                tenant_id,
                acpx_proto::session::GatewaySessionId(gid),
                agent_id,
                BackendSessionId(backend_session_id),
                profile.as_ref().map(|p| p.name.clone()),
                cwd,
            ),
            None => r.sessions.register(
                tenant_id,
                agent_id,
                BackendSessionId(backend_session_id),
                profile.as_ref().map(|p| p.name.clone()),
                cwd,
            ),
        };
        r.cancel_unreferenced_shutdown(&supervisor_key_for_cancel);
        let gateway_session_id_str = gateway_id.0.clone();
        if let Some(result) = response.get_mut("result") {
            result["sessionId"] = serde_json::Value::String(gateway_id.0);
        }
        let entry = r
            .sessions
            .resolve(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id_str.clone()),
            )
            .cloned()
            .expect("session was just registered");
        (gateway_session_id_str, entry)
    };

    let effective_params = request
        .get("params")
        .cloned()
        .ok_or(RouterError::MissingParams)?;
    if let Some(store) = persistence.clone() {
        if let Err(error) = store
            .record_session_with_recovery(
                gateway_session_id_str.clone(),
                entry.agent_id.clone(),
                entry.backend_session_id.0.clone(),
                entry.profile_name.clone(),
                now_rfc3339(),
                tenant_id.0.clone(),
                RecoveryMetadata {
                    cwd: entry.cwd.clone(),
                    recovery_params: Some(effective_params),
                    status: RecoveryStatus::Active,
                    recovery_method: RecoveryMethod::Load,
                    last_recovery_error: None,
                    created_at_unix_nanos: Some(now_unix_nanos()),
                    last_activity_at_unix_nanos: Some(now_unix_nanos()),
                    pinned: entry.pinned,
                    bridge_session_id: None,
                    bridge_model_alias: None,
                    bridge_config_options: None,
                },
            )
            .await
        {
            let mut r = router.lock().await;
            if let Some(removed) = r.sessions.remove(
                tenant_id,
                &acpx_proto::session::GatewaySessionId(gateway_session_id_str),
            ) {
                r.stop_if_session_scoped(&removed.agent_id).await;
                r.mark_unreferenced_if_idle(&removed.agent_id);
            }
            return Err(error.into());
        }
    }
    admission.commit();
    spawn_transcript_fn(
        persistence.clone(),
        gateway_session_id_str.clone(),
        Direction::ClientToAgent,
        request,
    );
    spawn_transcript_fn(
        persistence,
        gateway_session_id_str,
        Direction::AgentToClient,
        response.clone(),
    );

    Ok(response)
}

/// Wall-clock timestamp for persistence rows, RFC 3339 via `SystemTime` (no
/// extra date/time crate dependency -- acpx-core doesn't otherwise need
/// one, and this precision is more than enough for session/transcript
/// bookkeeping).
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:09}Z", now.as_secs(), now.subsec_nanos())
}

fn now_unix_nanos() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_nanos()).unwrap_or(i64::MAX)
}

/// **`retention_administration`.** Response shape shared by every
/// `session/retention/*` method -- `session/retention/list` builds an
/// array of these, the single-session arms return one directly. Ages are
/// whole seconds (this is a coarse operator-facing view, not a precision
/// timer).
fn retention_entry_json(
    gateway_session_id: &str,
    entry: &crate::session_registry::SessionEntry,
) -> serde_json::Value {
    let now = std::time::Instant::now();
    serde_json::json!({
        "sessionId": gateway_session_id,
        "pinned": entry.pinned,
        "customIdleTtlSeconds": entry.custom_idle_ttl.map(|ttl| ttl.as_secs()),
        "idleForSeconds": now.saturating_duration_since(entry.last_activity_at).as_secs(),
        "ageSeconds": now.saturating_duration_since(entry.created_at).as_secs(),
        "inFlight": entry.in_flight,
    })
}

/// Rebuild a monotonic lifecycle deadline from durable wall time. A missing
/// timestamp comes from a database predating lifecycle persistence, so it
/// deliberately restarts at `now` instead of expiring the session on boot.
fn restore_lifecycle_instant(stored_unix_nanos: Option<i64>) -> std::time::Instant {
    let Some(stored_unix_nanos) = stored_unix_nanos.filter(|value| *value > 0) else {
        return std::time::Instant::now();
    };
    let elapsed_nanos = now_unix_nanos().saturating_sub(stored_unix_nanos);
    let elapsed_nanos = u64::try_from(elapsed_nanos).unwrap_or(u64::MAX);
    std::time::Instant::now()
        .checked_sub(std::time::Duration::from_nanos(elapsed_nanos))
        .unwrap_or_else(std::time::Instant::now)
}

/// Mask every value in a serialized `Profile`'s `launch_overrides` map
/// before it's ever echoed back to a client (`profiles/create`/`update`'s
/// own response, and every entry in `profiles/list`).
///
/// **Real bug this closes** (found in the same self-review pass as the
/// `session/close` leak and `profiles/delete` process leak above):
/// `launch_overrides` is documented (`profile.rs`, `resolve_profile`'s
/// doc comment) as a raw env-var escape hatch specifically meant to carry
/// things like `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` directly -- the
/// real-adapter e2e test uses exactly that. Unlike the `secret` field
/// (which is deliberately never echoed back, only its opaque `KeyRef`),
/// `launch_overrides` was returned byte-for-byte in every
/// `profiles/create`/`update`/`list` response with no redaction at all.
/// For a gateway explicitly designed to serve *multiple concurrent
/// clients* (this workspace's own stated purpose) sharing one
/// `ACPX_AUTH_TOKEN`, that meant any client able to call `profiles/list`
/// could read every other client's raw API keys in plaintext, not just
/// its own. Keys are left visible (so a client can still see *which*
/// vars a profile overrides, useful for debugging) -- only values are
/// masked, mirroring the existing "secret material is never echoed"
/// precedent for `key_ref`/`Keystore`.
fn redact_launch_overrides(mut profile_json: serde_json::Value) -> serde_json::Value {
    if let Some(overrides) = profile_json
        .get_mut("launch_overrides")
        .and_then(|v| v.as_object_mut())
    {
        for value in overrides.values_mut() {
            *value = serde_json::Value::String("***redacted***".to_string());
        }
    }
    profile_json
}

/// Build a `SpawnSpec` for one of the official registry's `npx`-distributed
/// agents (Claude/Codex/Gemini today) -- `npx -y <package> <dist.args...>`.
/// Falls back to `uvx <package> <dist.args...>` when only a `uvx`
/// distribution is declared. Returns `None` for `binary`-only agents --
/// managed-mode profile launches aren't wired to the `binary` install path
/// (Phase 4 step 19) yet; no registry entry Claude/Codex/Gemini use today
/// needs it, per `01-research.md`.
fn npx_spawn_spec(agent: &acpx_registry::Agent) -> Option<acpx_conductor::SpawnSpec> {
    if let Some(npx) = &agent.distribution.npx {
        let mut args = vec!["-y".to_string(), npx.package.clone()];
        args.extend(npx.args.iter().cloned());
        return Some(acpx_conductor::SpawnSpec::new("npx", args));
    }
    if let Some(uvx) = &agent.distribution.uvx {
        let mut args = vec![uvx.package.clone()];
        args.extend(uvx.args.iter().cloned());
        return Some(acpx_conductor::SpawnSpec::new("uvx", args));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_methods() {
        assert_eq!(classify("session/new"), MethodClass::Hybrid);
        assert_eq!(classify("session/prompt"), MethodClass::Proxied);
        assert_eq!(classify("agents/list"), MethodClass::GatewayNative);
        assert_eq!(classify("bogus/method"), MethodClass::Unknown);
    }

    /// **`acpx-connect-loading-feedback`.** `session/load`/`session/resume`
    /// -- the two `Proxied` methods a real client's connect/loading UI
    /// gates on -- get the short `SESSION_ESTABLISH_IDLE_READ_TIMEOUT`;
    /// every other `Proxied` method (`session/prompt` above all -- a long
    /// wait there is often a legitimate in-progress generation) keeps the
    /// full `BACKEND_IDLE_READ_TIMEOUT` backstop.
    #[test]
    fn session_establish_idle_timeout_only_applies_to_load_and_resume() {
        assert_eq!(
            session_establish_or_default_idle_timeout("session/load"),
            SESSION_ESTABLISH_IDLE_READ_TIMEOUT
        );
        assert_eq!(
            session_establish_or_default_idle_timeout("session/resume"),
            SESSION_ESTABLISH_IDLE_READ_TIMEOUT
        );
        for other in [
            "session/prompt",
            "session/close",
            "session/delete",
            "session/set_config_option",
            "session/set_mode",
        ] {
            assert_eq!(
                session_establish_or_default_idle_timeout(other),
                BACKEND_IDLE_READ_TIMEOUT,
                "method {other} must keep the full idle-read backstop"
            );
        }
    }

    #[tokio::test]
    async fn handle_fs_request_windows_read_by_line_and_limit() {
        // Unit-level coverage for the `line`/`limit` windowing math itself
        // (1-indexed start, max-lines cap) -- `fs_request_test.rs` covers
        // the no-params "whole file" path end to end through `Router`;
        // this covers the windowing arithmetic directly since threading a
        // `line`/`limit`-carrying request through a shell stand-in backend
        // would be awkward to express.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("windowed.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "fs/read_text_file",
            "params": {"path": path.to_str().unwrap(), "line": 2, "limit": 2}
        });
        let reply = handle_fs_request(&request, "fs/read_text_file").await;
        // 1-indexed `line: 2` starts at "two"; `limit: 2` caps it there.
        assert_eq!(reply["result"]["content"], serde_json::json!("two\nthree"));
    }

    #[tokio::test]
    async fn handle_fs_request_read_of_missing_file_is_a_clear_error_not_a_panic() {
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "fs/read_text_file",
            "params": {"path": "/definitely/does/not/exist.txt"}
        });
        let reply = handle_fs_request(&request, "fs/read_text_file").await;
        assert!(reply.get("error").is_some());
        assert_eq!(
            reply["error"]["data"]["path"],
            serde_json::json!("/definitely/does/not/exist.txt")
        );
    }

    #[test]
    fn classifies_mcp_server_methods_as_gateway_native() {
        assert_eq!(classify("mcp_servers/create"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/list"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/update"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/delete"), MethodClass::GatewayNative);
    }

    /// **Phase 9.** `session/delete` and `logout` were entirely
    /// unclassified (fell through to `Unknown`) before this phase, same
    /// category of gap as phase 6's pre-fix `initialize`/`authenticate`.
    #[test]
    fn classifies_phase_9_stable_methods() {
        assert_eq!(classify("session/delete"), MethodClass::Proxied);
        assert_eq!(classify("logout"), MethodClass::GatewayNative);
    }

    /// Regression test for `BACKEND_IDLE_READ_TIMEOUT`'s live incident:
    /// a backend that produces zero bytes of output must not be allowed
    /// to hold `read_matching_response`'s read loop (and the per-process
    /// `BackendProcess` lock a caller holds around it) forever. Exercises
    /// the real `tokio::time::timeout` + kill path via
    /// `read_matching_response_with_idle_timeout`'s shortened-timeout
    /// parameter, in milliseconds rather than the real 20-minute constant.
    #[tokio::test]
    async fn backend_idle_read_timeout_kills_a_wedged_process_and_frees_the_lock() {
        // A silent backend: spawns, writes nothing to stdout, ever.
        let spec = acpx_conductor::SpawnSpec::new("sh", vec!["-c".to_string(), "sleep 30".to_string()]);
        let mut backend = acpx_conductor::BackendProcess::spawn(&spec)
            .await
            .expect("failed to spawn silent test backend");
        assert!(!backend.has_exited(), "backend should still be starting up");

        let result = read_matching_response_with_idle_timeout(
            &mut backend,
            &serde_json::json!(1),
            BackendCallPolicy::default(),
            None,
            Duration::from_millis(100),
        )
        .await;

        assert!(
            matches!(result, Err(RouterError::BackendIdleReadTimeout(_))),
            "expected BackendIdleReadTimeout, got {result:?}"
        );
        // Give the kill a moment to land, then confirm the wedged process
        // was actually terminated -- not merely abandoned with the call
        // failing but the child left running.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            backend.has_exited(),
            "wedged backend process should have been killed on idle-read timeout"
        );
    }

    /// **Fix regression test for the live incident traced from a real
    /// `"bridge session binding is in progress; retry the request"`
    /// report never clearing.** Before `BACKEND_HANDSHAKE_TIMEOUT`
    /// existed, `ensure_backend_initialized`'s `initialize` handshake
    /// read was a bare, unbounded `proc.reader.read_value().await` --
    /// unlike every read that follows it, which
    /// `read_matching_response_with_idle_timeout` already bounds. A
    /// backend that never answers `initialize` left this call (and the
    /// per-process `BackendProcess` lock every caller holds around it)
    /// hanging forever. Mirrors `backend_idle_read_timeout_kills_a_
    /// wedged_process_and_frees_the_lock` immediately above, but for the
    /// handshake path specifically: exercises the real
    /// `tokio::time::timeout` + kill path via `ensure_backend_
    /// initialized_with_handshake_timeout`'s parameter, in milliseconds
    /// rather than the real 30-second constant.
   #[tokio::test]
   async fn backend_handshake_timeout_kills_a_wedged_process_and_frees_the_lock() {
        // A silent backend: spawns, writes nothing to stdout, ever -- so
        // `initialize` is guaranteed to never be answered.
        let spec = acpx_conductor::SpawnSpec::new("sh", vec!["-c".to_string(), "sleep 30".to_string()]);
        let mut backend = acpx_conductor::BackendProcess::spawn(&spec)
            .await
            .expect("failed to spawn silent test backend");
        assert!(!backend.has_exited(), "backend should still be starting up");

        let result = ensure_backend_initialized_with_handshake_timeout(
            &mut backend,
            BackendCallPolicy::default(),
            Duration::from_millis(100),
        )
        .await;

        assert!(
            matches!(
                result,
                Err(RouterError::BackendHandshakeTimeout("initialize", _))
            ),
            "expected BackendHandshakeTimeout(\"initialize\", _), got {result:?}"
        );
       tokio::time::sleep(Duration::from_millis(200)).await;
       assert!(
           backend.has_exited(),
           "wedged backend process should have been killed on handshake timeout"
       );
   }

    /// **Regression test for a real production panic** (`BackendProcess::
    /// reader_mut called after start_demux() took the reader; use
    /// self.pending's registered oneshot instead`), traced from live
    /// `acpx-server` logs: `backend_idle_scavenger` polled `proc.
    /// reader_mut()` unconditionally on every 75ms tick, with no check
    /// for whether some `_shared` dispatch path had meanwhile called
    /// `start_demux()` on the exact same physical process (which is
    /// exactly what happens under normal load now that
    /// `process_reader_demux` defaults on -- see [`ProcessReaderDemux`]'s
    /// doc comment). `spawn_demux_consumer` already took over every
    /// live-notification duty this scavenger existed for once demux
    /// activates, so the fix is for the scavenger to notice `proc.
    /// pending.is_some()` and stop itself instead of ever calling
    /// `reader_mut()` again. Exercises the real (non-test-shortened)
    /// `backend_idle_scavenger` function directly against a real spawned
    /// process with demux already active, and asserts the task finishes
    /// (does not panic) well within its own 75ms poll interval.
    #[tokio::test]
    async fn idle_scavenger_stops_instead_of_panicking_once_demux_has_taken_the_reader() {
        let spec = acpx_conductor::SpawnSpec::new("sh", vec!["-c".to_string(), "cat".to_string()]);
        let mut backend = acpx_conductor::BackendProcess::spawn(&spec)
            .await
            .expect("failed to spawn cat-echo test backend");
        // Activate demux exactly like a real `_shared` dispatch path
        // does (`proc.start_demux()`) -- this is what takes `proc.
        // reader`, the precondition for `reader_mut()` to panic.
        let _unmatched_rx = backend.start_demux();
        let backend = std::sync::Arc::new(tokio::sync::Mutex::new(backend));

        let ctx = LiveNotifyCtx {
            router: std::sync::Arc::new(tokio::sync::Mutex::new(Router::new("idle-scavenger-demux-agent"))),
            agent_id: "idle-scavenger-demux-agent".to_string(),
            tenant_id: None,
            agent_relay: AgentRequestHub::new(),
            gateway_session_id: None,
            notification_hub: NotificationHub::new(),
            backend: std::sync::Arc::clone(&backend),
        };
        let task = tokio::spawn(backend_idle_scavenger(std::sync::Arc::clone(&backend), ctx));

        let joined = tokio::time::timeout(Duration::from_millis(500), task).await;
        let result = joined.expect(
            "backend_idle_scavenger should stop on its very first tick once proc.pending is \
             Some, not run forever -- if this timed out, the pending.is_some() early return \
             regressed",
        );
        assert!(
            result.is_ok(),
            "backend_idle_scavenger panicked instead of stopping cleanly: {result:?}"
        );
    }

   /// Regression test for the startup-recovery agent-registration bug:
    /// confirmed live via `last_recovery_error` across 7 consecutive real
    /// `acpx-server` restarts, `no spawn spec registered for agent
    /// codex-acp`, 0 successful recoveries out of 9 accumulated bridge
    /// sessions, ever. Root cause -- a registry-backed agent id (any
    /// bridge session's concrete backend, as opposed to the one
    /// statically registered `default_agent_id`) only ever got a
    /// `SpawnSpec` via `ensure_registry_agent_registered`, and startup
    /// recovery called `self.supervisor.ensure_running(&agent_id)`
    /// directly, skipping that call entirely -- so recovery ran before
    /// any live session had a chance to lazily register the spec it
    /// needed. Uses the bundled offline `registry.fallback.json` (via
    /// `acpx_registry::fallback_registry()`) instead of
    /// `ensure_registry_loaded`'s live-network attempt, so this stays
    /// hermetic and fast; only registration is asserted here (not a full
    /// `ensure_running` spawn), since the real registry's `codex-acp`
    /// entry launches via real `npx`, which is deliberately out of scope
    /// for a unit test.
    #[tokio::test]
    async fn ensure_registry_agent_registered_populates_a_spec_for_a_registry_only_agent_id() {
        let mut router = Router::new("default");
        router.registry_cache = Some(acpx_registry::fallback_registry());

        assert!(
            router.supervisor.spec("codex-acp").is_none(),
            "codex-acp must start out unregistered, exactly like a freshly booted acpx-server \
             that only ever statically registers `default_agent_id` (see main.rs)"
        );

        router
            .ensure_registry_agent_registered("codex-acp")
            .await
            .expect("codex-acp is a real entry in the bundled fallback registry");

        assert!(
            router.supervisor.spec("codex-acp").is_some(),
            "ensure_registry_agent_registered must have registered a SpawnSpec -- this is the \
             exact call `prepare_open_session_recovery` now makes before \
             `self.supervisor.ensure_running(&agent_id)`, closing the startup-recovery gap"
        );

        // Idempotent: calling it again once already registered must not
        // error or attempt a redundant registry lookup.
        router
            .ensure_registry_agent_registered("codex-acp")
            .await
            .expect("re-registering an already-registered agent id must be a no-op");
    }

    /// Regression test for `call_policy`'s auto-seeded-profile auth
    /// shadowing bug: confirmed live via a real recovery failure
    /// (`backend requires authentication...`) followed minutes later by
    /// a successful live prompt against the exact same fresh backend
    /// process, once an unrelated background capability probe had
    /// authenticated it first through its own workaround. A profile with
    /// no explicit `auth_method_id` (exactly what
    /// `ensure_default_profiles_seeded` always produces for every
    /// registry agent) must still fall back to the router's configured
    /// `native_auth_method_id` -- only an *explicitly* set
    /// `Profile::auth_method_id` should ever override it.
    #[test]
    fn call_policy_falls_back_to_native_auth_method_when_profile_omits_one() {
        let router = Router::new("default").with_native_auth_method_id(Some("api-key".to_string()));

        let auto_seeded_profile = crate::profile::Profile {
            name: "codex-acp".to_string(),
            agent_id: "codex-acp".to_string(),
            provider: None,
            key_ref: None,
            launch_overrides: HashMap::new(),
            mcp_servers: vec![],
            permission_policy: Default::default(),
            allow_fs_access: true,
            allow_terminal_access: true,
            auth_method_id: None,
        };
        let policy = router.call_policy(Some(&auto_seeded_profile));
        assert_eq!(
            policy.auth_method_id.as_deref(),
            Some("api-key"),
            "a profile with no explicit auth_method_id must still fall back to \
             native_auth_method_id, not silently drop it"
        );
        // The rest of the auto-seeded profile's fields must still win --
        // this fix must not regress `call_policy_for`'s own documented
        // reason for consulting the seeded profile at all.
        assert!(policy.allow_fs_access);
        assert!(policy.allow_terminal_access);

        let explicit_profile = crate::profile::Profile {
            auth_method_id: Some("chat-gpt".to_string()),
            ..auto_seeded_profile
        };
        let policy = router.call_policy(Some(&explicit_profile));
        assert_eq!(
            policy.auth_method_id.as_deref(),
            Some("chat-gpt"),
            "an explicitly configured profile auth_method_id must still win over \
             the router's native default"
        );

        let policy = router.call_policy(None);
        assert_eq!(
            policy.auth_method_id.as_deref(),
            Some("api-key"),
            "no profile at all must keep falling back to native_auth_method_id, \
             same as before this fix"
        );
    }
}
