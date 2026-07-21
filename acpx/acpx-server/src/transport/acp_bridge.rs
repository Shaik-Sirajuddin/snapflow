//! Strict ACP request dispatcher for the optional `/acp` surface.
//!
//! The public protocol has no ACPX profile or adapter selector. A bridge
//! session starts as an in-memory virtual session, selects one policy-owned
//! model alias, and binds to a regular ACPX gateway session only when a turn
//! needs a backend. This module deliberately delegates all backend work to
//! `acpx_core::Router`; it does not own a second process manager.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Instant;

use acpx_bridge::{BridgeConfig, BridgeModel};
use acpx_core::persistence::PersistenceError;
use acpx_core::persistence::{sessions::RecoveryStatus, PersistenceStore};
use acpx_core::router::{dispatch_shared_for_tenant, RouterError};
use acpx_core::{
    BindingClaim, BridgeSession, BridgeSessionError, BridgeSessionId, BridgeSessionState,
    BridgeSessionStore, InteractionBinding, InteractionHub, TenantId,
};
use acpx_proto::session::NewSessionParams;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex as AsyncMutex, RwLock};
use tokio::time::Duration;

use super::SharedRouter;

/// Live-connection hook so a backend-initiated request mid-`session/prompt`
/// (`session/request_permission`, `fs/*`, `terminal/*`) reaches the actual
/// `/acp` client instead of always falling through to the static
/// `Profile`/`CallPolicy` auto-answer. Mirrors `transport::ws::handle_socket`'s
/// native-transport binding, translated one level down: the bridge's virtual
/// session id is not the key `InteractionHub` uses (it only ever knows the
/// native/gateway session id), so binding must happen the moment a virtual
/// session's native id becomes known -- i.e. inside [`bind`] itself, right
/// before the very first `session/prompt` round trip that lazy-bind kicks
/// off, not after the fact. A `POST /acp/rpc` one-shot call passes `None`,
/// same "no persistent connection to hand an interactive request to"
/// reasoning as every other one-shot path in this codebase.
#[derive(Clone)]
pub(crate) struct BridgeInteractionCtx {
    pub hub: InteractionHub,
    pub sender: mpsc::Sender<Value>,
    /// Keyed by native/gateway session id (not the bridge's virtual id) so
    /// a reconnect or a second bridge session sharing an agent process
    /// never collide; owned by the WS connection so it can unbind every
    /// entry on disconnect.
    pub bindings: Arc<AsyncMutex<HashMap<String, InteractionBinding>>>,
}

impl BridgeInteractionCtx {
    /// (Re-)assert this connection as the answerer for `native_id`. Safe to
    /// call on every prompt-shaped request, not just the first: `bind`'s
    /// "newer bind replaces older owner" semantics make this idempotent for
    /// the common case (this same connection re-asserting itself) and
    /// correct for the reconnect case (a new connection taking over from a
    /// stale one).
    async fn claim(&self, tenant_id: &TenantId, native_id: &str) {
        let binding = self
            .hub
            .bind(
                tenant_id.clone(),
                native_id.to_string(),
                self.sender.clone(),
            )
            .await;
        let previous = self
            .bindings
            .lock()
            .await
            .insert(native_id.to_string(), binding);
        if let Some(previous) = previous {
            self.hub.unbind(&previous).await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeDispatchError {
    #[error("strict ACP bridge requests cannot include _acpx extensions")]
    AcpxExtensionNotAllowed,
    #[error("bridge request requires params.sessionId")]
    MissingSessionId,
    #[error("bridge session binding is in progress; retry the request")]
    BindingInProgress,
    #[error("bridge session binding previously failed; create a new session")]
    BindingFailed,
    #[error("bridge model selector must use configId \"model\" and a string value")]
    InvalidModelSelection,
    #[error("model alias {0:?} is not configured")]
    UnknownModel(String),
    #[error("cannot switch a bound bridge session between adapters")]
    CrossAdapterModelSwitch,
    #[error(transparent)]
    Session(#[from] BridgeSessionError),
    #[error(transparent)]
    Router(#[from] RouterError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// Shared state mounted only when the strict bridge feature is enabled.
/// Always held behind `Arc<BridgeRuntime>` (see `AppState::bridge_runtime`)
/// rather than cloned by value -- `config`'s `ArcSwap` isn't `Clone` (and
/// deliberately so: cloning it would let a caller hold a stale snapshot
/// past a [`BridgeRuntime::reload_config`] swap without realizing it).
pub struct BridgeRuntime {
    /// **`config_hot_reload` (phase 2).** Lock-free swappable so
    /// [`Self::reload_config`] (driven by the background file-watcher
    /// spawned in `serve_on_with_bridge_and_tenant_tokens`) can publish a
    /// freshly-validated `BridgeConfig` without a restart and without any
    /// concurrent reader ever blocking on a lock. Use [`Self::config`] to
    /// read it -- never add a second public field pointing at the same
    /// data, or a stale clone becomes possible to hold past a reload.
    config: arc_swap::ArcSwap<BridgeConfig>,
    pub sessions: BridgeSessionStore,
    models: Arc<RwLock<Vec<BridgeModel>>>,
    config_options: Arc<RwLock<Vec<Value>>>,
    /// Guards [`Self::refresh_models`] from running its (bounded, but
    /// still real) per-agent capability probe on every single dispatch.
    /// See [`MODEL_REFRESH_COOLDOWN`]'s doc comment for the live incident
    /// this closes.
    last_refresh_attempt: Arc<StdMutex<Option<Instant>>>,
}

/// Hard ceiling on one adapter's capability probe inside [`BridgeRuntime::refresh_models`].
///
/// **Real incident this guards against, not a hypothetical:** `probe_adapter_capabilities`
/// runs while holding the single global [`SharedRouter`] mutex (see the lock scope below),
/// and its `initialize`/`authenticate`/`session/new` reads against the backend's stdio have
/// no timeout of their own (`BackendCallPolicy::timeout` is a real field but is never wired
/// into `ensure_backend_initialized`/`read_matching_response` -- a separate, deeper gap left
/// for a future fix). Before this constant existed, one backend that stopped answering --
/// observed live against `codex-acp` -- wedged the router mutex forever, which froze *every*
/// tenant/session on the server, not just `/acp/models`: this matched a real "Zed stuck at
/// loading" report end to end, with `session/new` calls from Zed's own client queued behind
/// the same lock and never returning either (confirmed live via CLOSE-WAIT sockets piling up
/// on the HTTP listener while every authenticated endpoint, not only `/acp/models`, hung).
/// Bounding this one call lets a stuck backend fail *this* refresh cleanly -- existing
/// static/previously-discovered models stay available -- instead of taking the whole server
/// down with it.
const MODEL_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Minimum time between [`BridgeRuntime::refresh_models`] actually probing
/// backends, regardless of how many dispatches arrive in between.
///
/// **Real incident this guards against, not a hypothetical.** Before this
/// existed, `dispatch_with_interaction` called `refresh_models` --
/// unconditionally, on *every* bridge request, including session-less ones
/// like `agents/list` and every `session/prompt` for an unrelated session
/// -- with zero debounce. Each call re-acquires the global router mutex
/// and re-probes every configured agent id from scratch, bounded to
/// `MODEL_PROBE_TIMEOUT` (20s) per agent by `tokio::time::timeout`, but
/// that per-call bound did not prevent pile-up: while one backend was
/// merely busy (a real in-flight `session/prompt`, not even wedged), its
/// `probe_adapter_capabilities` call queued behind the *same*
/// per-backend-process lock the live prompt held, so it burned close to
/// the full 20s holding the router mutex before giving up. Every other
/// concurrent bridge request -- including ones with nothing to do with
/// that backend, like a plain `agents/list` -- queued behind the *router*
/// mutex during that window, and if several concurrent requests each
/// tried their own redundant refresh in turn, the stalls serialized and
/// compounded instead of overlapping. Reproduced live: a single in-flight
/// prompt against `codex-acp` caused a completely unrelated `agents/list`
/// call to hang for minutes, well past any single `MODEL_PROBE_TIMEOUT`
/// window, with `/health` (which never touches the router lock) still
/// answering instantly the whole time -- proof the stall was this
/// per-request refresh, not a genuine full-router deadlock. Once any
/// refresh attempt has been made, every dispatch within this cooldown
/// window returns immediately using the already-cached model list instead
/// of re-probing; the first-ever call (start of process lifetime, `None`)
/// always proceeds so model discovery still happens at least once.
const MODEL_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

impl BridgeRuntime {
    pub fn new(config: Arc<BridgeConfig>) -> Self {
        Self {
            models: Arc::new(RwLock::new(config.models.clone())),
            config_options: Arc::new(RwLock::new(Vec::new())),
            config: arc_swap::ArcSwap::new(config),
            sessions: BridgeSessionStore::new(),
            last_refresh_attempt: Arc::new(StdMutex::new(None)),
        }
    }

    /// Current live `BridgeConfig` -- a cheap `Arc` clone, never blocks on
    /// a lock. Call fresh each time you need it rather than caching the
    /// returned `Arc` across an `.await` point that could span a reload,
    /// unless holding a momentarily-stale snapshot is genuinely fine for
    /// that call site (it is for every existing read site this phase
    /// touched: each reads one field once, synchronously).
    pub fn config(&self) -> Arc<BridgeConfig> {
        self.config.load_full()
    }

    /// **`config_hot_reload` (phase 2).** Publish a freshly-validated
    /// `BridgeConfig`, replacing the live one for every subsequent
    /// [`Self::config`] read -- no restart, no dropped sessions (nothing
    /// session-scoped lives on `BridgeConfig` itself; `BridgeSessionStore`
    /// is untouched by a reload). Callers must have already validated
    /// `new_config` (e.g. via [`BridgeConfig::from_file`], which validates
    /// internally) -- this method does not re-validate, it only swaps.
    ///
    /// Also resets the static baseline of [`Self::models`] to
    /// `new_config.models` (mirroring what [`Self::new`] does at
    /// construction), so a model list edit is reflected immediately
    /// without waiting for the next cooldown-gated [`Self::refresh_models`]
    /// probe to layer live-discovered entries back on top.
    pub async fn reload_config(&self, new_config: BridgeConfig) {
        let mut models = self.models.write().await;
        *models = new_config.models.clone();
        drop(models);
        self.config.store(Arc::new(new_config));
    }

    /// Refreshes public models from each configured adapter's cached
    /// capability probe. Static entries remain as an operator fallback, but
    /// every discovered model is exposed without hand-maintaining aliases.
    pub async fn refresh_models(&self, router: &SharedRouter) {
        self.refresh_models_with_config(router, MODEL_PROBE_TIMEOUT, MODEL_REFRESH_COOLDOWN)
            .await
    }

    /// Real logic behind [`Self::refresh_models`], parameterized so a unit
    /// test can use millisecond-scale timeouts instead of waiting out
    /// `MODEL_PROBE_TIMEOUT`/`MODEL_REFRESH_COOLDOWN`'s real production
    /// durations. `refresh_models` is a thin wrapper always passing the
    /// prod constants -- this seam exists purely for testability, mirroring
    /// `acpx_core::router`'s identical `read_matching_response`/
    /// `read_matching_response_with_idle_timeout` split.
    async fn refresh_models_with_config(
        &self,
        router: &SharedRouter,
        probe_timeout: Duration,
        cooldown: Duration,
    ) {
        {
            // Claim the right to refresh *before* doing any router-lock
            // work, and claim it eagerly (write the new timestamp even
            // though the probes below haven't run yet) so a burst of
            // concurrent callers within the same instant all see the
            // claim and skip, rather than all passing the check and all
            // piling onto the router lock together. A `std::sync::Mutex`
            // is safe here (never held across an `.await`): this block
            // ends before any async work begins.
            let mut last_attempt = self
                .last_refresh_attempt
                .lock()
                // Self-heals on poison rather than permanently wedging
                // every future model refresh attempt this process ever
                // makes -- see `bridge_sessions::lock_sessions`'s doc
                // comment for the identical reasoning against a plain
                // `Option<Instant>` guard.
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            if let Some(previous) = *last_attempt {
                if now.duration_since(previous) < cooldown {
                    return;
                }
            }
            *last_attempt = Some(now);
        }
        // Seed set = operator-pinned static `models` entries (back-compat
        // override/pin, never pruned below) UNION every explicitly
        // *provisioned* profile's `agent_id` (`ACPX_CONFIG_FILE`'s
        // `profiles` array or a runtime `profiles/create` call).
        // Deliberately excludes `ensure_default_profiles_seeded`'s
        // auto-seeded (one-per-installed-CLI) profiles -- probing, and
        // implicitly spawning, every ACP agent binary this host happens
        // to have installed would silently turn a curated model picker
        // into "every CLI on this dev machine," defeating the reason the
        // bridge has its own model catalog at all (see this crate's own
        // module doc: "deliberately exposes models, never ACPX managed
        // profiles" -- that boundary applies to auto-seeded profiles
        // too, not just the hand-authored ones).
        let static_config = self.config();
        let mut agent_ids: HashSet<String> = static_config
            .agent_ids()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        // A quick lock + synchronous in-memory read -- no registry
        // fetch, no subprocess spawns (see
        // `Router::provisioned_profile_agent_ids`'s own doc comment for
        // why it must stay this cheap on a cooldown-gated hot path).
        agent_ids.extend(router.lock().await.provisioned_profile_agent_ids());
        let agent_ids: Vec<String> = agent_ids.into_iter().collect();
        let mut discovered = Vec::new();
        let mut discovered_options = Vec::new();
        let mut succeeded_agent_ids: HashSet<String> = HashSet::new();
        for agent_id in agent_ids {
            let probe = async {
                let mut router = router.lock().await;
                router.probe_adapter_capabilities(&agent_id, "/tmp").await
            };
            let capabilities = match tokio::time::timeout(probe_timeout, probe).await {
                Ok(result) => result,
                Err(_) => {
                    // Timing out here drops `probe` -- and with it the router
                    // `MutexGuard` acquired inside -- so a wedged backend
                    // releases the global lock for every other tenant/session
                    // instead of holding it forever. See `MODEL_PROBE_TIMEOUT`'s
                    // doc comment for the live incident this fixes.
                    tracing::warn!(
                        agent_id = %agent_id,
                        timeout_secs = probe_timeout.as_secs(),
                        "capability probe timed out; skipping model discovery for \
                         this adapter this refresh (static/previously-discovered \
                         models remain available)"
                    );
                    continue;
                }
            };
            let Ok(capabilities) = capabilities else {
                continue;
            };
            succeeded_agent_ids.insert(agent_id.clone());
            let namespace = agent_id.strip_suffix("-acp").unwrap_or(&agent_id);
            discovered.extend(capabilities.models.into_iter().map(|model| BridgeModel {
                id: format!("{namespace}/{}", model.value),
                name: Some(model.name),
                agent_id: agent_id.clone(),
                model_id: model.value,
            }));
            discovered_options.extend(
                capabilities
                    .config_options
                    .into_iter()
                    .filter(|option| option.id != "model")
                    .map(|option| {
                        json!({
                            "id": option.id,
                            "name": option.name,
                            "category": option.category,
                            "type": "select",
                            "currentValue": option.current_value,
                            "options": option.options.into_iter().map(|choice| json!({
                                "value": choice.value,
                                "name": choice.name,
                            })).collect::<Vec<_>>(),
                        })
                    }),
            );
        }
        if discovered.is_empty() && succeeded_agent_ids.is_empty() {
            // No agent answered at all this cycle (every probe failed or
            // timed out) -- keep serving whatever was previously known
            // rather than pruning it away on a transient outage.
            return;
        }
        let mut models = self.models.write().await;
        let static_ids: HashSet<&str> =
            static_config.models.iter().map(|model| model.id.as_str()).collect();
        Self::merge_discovered_models(&mut models, &static_ids, &succeeded_agent_ids, discovered);
        drop(models);
        let mut options = self.config_options.write().await;
        for option in discovered_options {
            let id = option.get("id").and_then(Value::as_str);
            if id.is_some_and(|id| options.iter().any(|existing| existing["id"] == id)) {
                continue;
            }
            options.push(option);
        }
    }

    /// Pure merge step behind [`Self::refresh_models_with_config`]'s
    /// tail, split out so a test can exercise it directly without a
    /// real/fake subprocess probe. Drops every previously-discovered
    /// (non-static) entry belonging to an agent in `succeeded_agent_ids`,
    /// then adds back exactly `discovered` -- a model its agent no
    /// longer reports stops being served instead of staying available
    /// forever (the live incident: a Bifrost catalog entry like
    /// `claude/claude-fable-5[1m]` outliving its own upstream removal,
    /// because the previous merge was an unconditional `Vec::extend`
    /// that never dropped anything). Entries whose `id` is in
    /// `static_ids` (operator-pinned overrides) are never touched here
    /// regardless of `succeeded_agent_ids`, and an agent absent from
    /// `succeeded_agent_ids` (its probe failed/timed out this cycle)
    /// keeps its previous entries untouched -- better briefly-stale than
    /// briefly-empty.
    fn merge_discovered_models(
        models: &mut Vec<BridgeModel>,
        static_ids: &HashSet<&str>,
        succeeded_agent_ids: &HashSet<String>,
        discovered: Vec<BridgeModel>,
    ) {
        models.retain(|model| {
            static_ids.contains(model.id.as_str())
                || !succeeded_agent_ids.contains(&model.agent_id)
        });
        let mut seen: HashSet<String> = models.iter().map(|model| model.id.clone()).collect();
        models.extend(
            discovered
                .into_iter()
                .filter(|model| seen.insert(model.id.clone())),
        );
    }

    pub async fn resolve_model(&self, alias: &str) -> Option<BridgeModel> {
        self.models
            .read()
            .await
            .iter()
            .find(|model| model.id == alias)
            .cloned()
    }

    /// The model alias a session with no explicit selection resolves
    /// against. Prefers `BridgeConfig::default_model` when the operator
    /// pinned one; otherwise falls back to the first entry in the live
    /// model list (whatever `refresh_models_with_config` has discovered
    /// so far via provisioned profiles) so a bridge config with no
    /// static `models`/`default_model` at all -- the normal shape now --
    /// still has a usable default the moment discovery has found
    /// anything. Returns an empty string only in the genuine edge case
    /// of zero models discovered yet (e.g. process just started, no
    /// provisioned profile has successfully probed) -- every caller of
    /// this already handles an unresolvable alias as
    /// `BridgeDispatchError::UnknownModel`, so this deliberately doesn't
    /// invent a placeholder.
    pub async fn effective_default_model(&self) -> String {
        let configured = self.config().default_model.clone();
        if !configured.is_empty() {
            return configured;
        }
        self.models
            .read()
            .await
            .first()
            .map(|model| model.id.clone())
            .unwrap_or_default()
    }

    /// `current_model_alias` is the bridge session's own
    /// `selected_public_model_alias` (or `None` for a brand new,
    /// never-configured session). Callers MUST pass the session's actual
    /// current selection here -- this used to always stamp
    /// `self.config().default_model`, so every `session/set_config_option`
    /// response (bound or unbound) reported the global default as
    /// `currentValue` regardless of what was just selected, and Zed's
    /// `AcpSessionConfigOptions::set_config_option` applies that response
    /// verbatim to its cached UI state -- so picking e.g. Haiku would
    /// immediately snap the picker back to showing the default model.
    pub async fn model_config_options(&self, current_model_alias: Option<&str>) -> Value {
        let models = self.models.read().await;
        // Inlined rather than calling `Self::effective_default_model` --
        // that method also takes `self.models.read()`, and
        // `tokio::sync::RwLock` is explicitly documented as not
        // reentrant: a second read acquisition from the same task while
        // this `models` guard is still held can deadlock against a
        // writer that queued in between (write-preferring fairness).
        let configured_default = self.config().default_model.clone();
        let current_value = match current_model_alias {
            Some(alias) => alias.to_string(),
            None if !configured_default.is_empty() => configured_default,
            None => models
                .first()
                .map(|model| model.id.clone())
                .unwrap_or_default(),
        };
        let mut options = vec![json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current_value,
            "options": models.iter().map(|model| json!({
                "value": model.id,
                "name": model.name.as_deref().unwrap_or(&model.id),
            })).collect::<Vec<_>>(),
        })];
        options.extend(self.config_options.read().await.iter().cloned());
        Value::Array(options)
    }

    pub async fn adapter_config_option(
        &self,
        config_id: &str,
        value: &str,
    ) -> Option<(String, String)> {
        self.config_options
            .read()
            .await
            .iter()
            .find(|option| {
                option["id"] == config_id
                    && option["options"].as_array().is_some_and(|choices| {
                        choices.iter().any(|choice| choice["value"] == value)
                    })
            })
            .map(|_| (config_id.to_string(), value.to_string()))
    }

    pub async fn public_models(&self, agents_result: &Value) -> Vec<acpx_bridge::PublicModel> {
        let models = self.models.read().await;
        BridgeConfig::public_models_for(&models, agents_result)
    }

    pub fn bound_gateway_session_id(
        &self,
        tenant_id: &TenantId,
        virtual_session_id: &str,
    ) -> Option<String> {
        self.sessions
            .bound_gateway_session_id(tenant_id, &BridgeSessionId(virtual_session_id.to_string()))
    }

    pub fn virtual_session_id(
        &self,
        tenant_id: &TenantId,
        bound_gateway_session_id: &str,
    ) -> Option<String> {
        self.sessions
            .find_by_bound_gateway_session_id(tenant_id, bound_gateway_session_id)
            .map(|id| id.0)
    }

    /// Recreate bridge-visible mappings only after native startup recovery
    /// has restored their gateway sessions. Malformed or stale bridge data is
    /// skipped so it cannot make the whole daemon unavailable.
    pub async fn restore_recovered_sessions(
        &self,
        store: &PersistenceStore,
    ) -> Result<usize, BridgeDispatchError> {
        let mut restored = 0;
        for record in store.list_recoverable_sessions().await? {
            if record.status != RecoveryStatus::Restored {
                continue;
            }
            let (Some(virtual_id), Some(model_alias), Some(params)) = (
                record.bridge_session_id,
                record.bridge_model_alias,
                record.recovery_params,
            ) else {
                continue;
            };
            let Ok(params) = serde_json::from_value::<NewSessionParams>(params) else {
                tracing::warn!(
                    gateway_session_id = %record.gateway_session_id,
                    "skipping malformed persisted bridge session parameters"
                );
                continue;
            };
            let options = record
                .bridge_config_options
                .and_then(|value| serde_json::from_value::<HashMap<String, String>>(value).ok())
                .unwrap_or_default();
            let tenant = TenantId(record.tenant_id);
            let id = BridgeSessionId(virtual_id);
            if self.sessions.get(&tenant, &id).is_some() {
                continue;
            }
            self.sessions.restore_bound(
                &tenant,
                id,
                params,
                Some(model_alias),
                options,
                record.gateway_session_id,
            )?;
            restored += 1;
        }
        Ok(restored)
    }
}

/// **`config_hot_reload` (phase 2).** Watches `config_path`
/// (`ACPX_ACP_BRIDGE_CONFIG_FILE`) for filesystem changes and publishes
/// each valid edit to `runtime` via [`BridgeRuntime::reload_config`], with
/// no restart. An invalid candidate (fails [`BridgeConfig::validate`], or
/// isn't even parseable JSON) is logged and discarded -- the previously
/// live config keeps serving every request, exactly the "reject and log,
/// keep old config live" contract
/// `memory/acpx/gen/acpx-concurrency-config-execution.meta.json` phase 2
/// specifies.
///
/// Runs on its own dedicated OS thread, not a tokio task: `notify`'s
/// watcher callback fires from a platform-native (inotify on Linux) event
/// thread outside tokio's runtime, and every step this loop takes in
/// response (`BridgeConfig::from_file` -- a small synchronous disk read
/// + JSON parse + validate -- and `ArcSwap::store`) is itself synchronous
/// and non-blocking-in-the-async-sense, so there is nothing here that
/// benefits from running inside tokio; keeping it on a plain thread
/// avoids ever needing `spawn_blocking` or bridging the callback into an
/// async channel. The one truly async step, [`BridgeRuntime::reload_
/// config`] (it briefly awaits `self.models`'s `RwLock`), is driven via a
/// short-lived single-threaded `tokio::runtime::Handle::block_on` call
/// scoped to just that one await -- cheap and rare (config edits are not
/// a hot path) enough that spinning up a tiny runtime per reload is the
/// simplest correct option, not a performance concern.
///
/// A failure to construct or start the watcher itself (rare -- e.g. the
/// config file's parent directory disappearing, or the platform's
/// notification backend being unavailable) is logged and this task simply
/// exits: config changes then require a restart, exactly today's
/// pre-phase-2 behavior, never a startup failure.
pub fn spawn_config_watcher(runtime: Arc<BridgeRuntime>, config_path: std::path::PathBuf) {
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};

        let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!(
                    %err,
                    path = %config_path.display(),
                    "acpx bridge config hot-reload: failed to create a file watcher; \
                     config changes will require a restart"
                );
                return;
            }
        };
        if let Err(err) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
            tracing::warn!(
                %err,
                path = %config_path.display(),
                "acpx bridge config hot-reload: failed to watch the bridge config file; \
                 config changes will require a restart"
            );
            return;
        }
        tracing::info!(
            path = %config_path.display(),
            "acpx bridge config hot-reload: watching for changes"
        );

        for event in rx {
            let event = match event {
                Ok(event) => event,
                Err(err) => {
                    tracing::warn!(%err, "acpx bridge config hot-reload: watch error");
                    continue;
                }
            };
            if !(event.kind.is_modify() || event.kind.is_create()) {
                continue;
            }
            match BridgeConfig::from_file(&config_path) {
                Ok(candidate) => {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("build a tiny current-thread runtime for one reload swap");
                    rt.block_on(runtime.reload_config(candidate));
                    tracing::info!(
                        path = %config_path.display(),
                        "acpx bridge config hot-reloaded"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        %err,
                        path = %config_path.display(),
                        "acpx bridge config hot-reload: candidate failed validation, \
                         keeping the previous config live"
                    );
                }
            }
        }
    });
}

pub async fn dispatch(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
) -> Result<Value, BridgeDispatchError> {
    dispatch_with_interaction(router, runtime, tenant_id, request, None).await
}

/// Same as [`dispatch`], but callers on a persistent transport (currently
/// only `transport::ws::handle_acp_socket`) pass `interaction` so a
/// backend-initiated request mid-turn reaches this exact connection instead
/// of the static policy fallback. See [`BridgeInteractionCtx`]'s doc comment
/// for why this can't simply be bolted on after the fact.
pub(crate) async fn dispatch_with_interaction(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
    interaction: Option<&BridgeInteractionCtx>,
) -> Result<Value, BridgeDispatchError> {
    runtime.refresh_models(router).await;
    match request.get("method").and_then(Value::as_str) {
        Some("session/new") => new_session(runtime, tenant_id, request).await,
        Some("session/set_config_option") => {
            set_config_option(router, runtime, tenant_id, request).await
        }
        Some("session/close") | Some("session/delete") => {
            close_or_delete(router, runtime, tenant_id, request).await
        }
        Some("session/fork") => {
            fork_session(router, runtime, tenant_id, request, interaction).await
        }
        Some(
            "session/prompt" | "session/cancel" | "session/load" | "session/resume"
            | "session/set_mode",
        ) => forward_bound(router, runtime, tenant_id, request, interaction).await,
        Some(_) => {
            reject_acpx_extension(&request)?;
            Ok(dispatch_shared_for_tenant(router, tenant_id, request).await?)
        }
        None => Err(BridgeDispatchError::Router(RouterError::MissingMethod)),
    }
}

async fn new_session(
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
) -> Result<Value, BridgeDispatchError> {
    reject_acpx_extension(&request)?;
    let params = request
        .get("params")
        .cloned()
        .ok_or(RouterError::MissingParams)?;
    let parsed = serde_json::from_value(params).map_err(|_| RouterError::MissingParams)?;
    let session_id = runtime.sessions.try_register(
        tenant_id,
        parsed,
        runtime.config().max_virtual_sessions_per_tenant,
    )?;
    Ok(json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": {
            "sessionId": session_id.0,
            "configOptions": runtime.model_config_options(None).await,
        }
    }))
}

async fn set_config_option(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
) -> Result<Value, BridgeDispatchError> {
    reject_acpx_extension(&request)?;
    let session_id = request_session_id(&request)?;
    let config_id = request
        .pointer("/params/configId")
        .and_then(Value::as_str)
        .ok_or(BridgeDispatchError::InvalidModelSelection)?;
    let selected_value = request
        .pointer("/params/value")
        .and_then(Value::as_str)
        .ok_or(BridgeDispatchError::InvalidModelSelection)?;
    let session = runtime
        .sessions
        .get(tenant_id, &session_id)
        .ok_or_else(|| BridgeSessionError::NotFound {
            tenant_id: tenant_id.0.clone(),
            session_id: session_id.0.clone(),
        })?;

    if config_id != "model" {
        let (native_id, native_value) = runtime
            .adapter_config_option(config_id, selected_value)
            .await
            .ok_or(BridgeDispatchError::InvalidModelSelection)?;
        return match session.state {
            BridgeSessionState::Unbound => {
                runtime.sessions.select_adapter_config_option(
                    tenant_id,
                    &session_id,
                    native_id,
                    native_value,
                )?;
                Ok(success(
                    &request,
                    json!({"configOptions": runtime
                        .model_config_options(session.selected_public_model_alias.as_deref())
                        .await}),
                ))
            }
            BridgeSessionState::Binding => Err(BridgeDispatchError::BindingInProgress),
            BridgeSessionState::Failed => Err(BridgeDispatchError::BindingFailed),
            BridgeSessionState::Bound => {
                let mut native = request.clone();
                native["params"]["sessionId"] =
                    Value::String(session.bound_gateway_session_id.clone().expect("bound id"));
                native["params"]["configId"] = Value::String(native_id.to_string());
                native["params"]["value"] = Value::String(native_value.to_string());
                let response = forward_session_request(router, tenant_id, &session, native).await?;
                if response.get("error").is_none() {
                    let updated = runtime.sessions.update_bound_adapter_config_option(
                        tenant_id,
                        &session_id,
                        native_id,
                        native_value,
                    )?;
                    persist_bridge_binding(router, runtime, &updated).await?;
                }
                Ok(response)
            }
        };
    }
    let selected = runtime
        .resolve_model(selected_value)
        .await
        .ok_or_else(|| BridgeDispatchError::UnknownModel(selected_value.to_string()))?;

    match session.state {
        BridgeSessionState::Unbound => {
            runtime
                .sessions
                .select_model(tenant_id, &session_id, selected.id.clone())?;
            Ok(success(
                &request,
                json!({"configOptions": runtime.model_config_options(Some(&selected.id)).await}),
            ))
        }
        BridgeSessionState::Binding => Err(BridgeDispatchError::BindingInProgress),
        BridgeSessionState::Failed => Err(BridgeDispatchError::BindingFailed),
        BridgeSessionState::Bound => {
            let current_alias_owned;
            let current_alias = match session
                .selected_public_model_alias
                .as_deref()
            {
                Some(alias) => alias,
                None => {
                    current_alias_owned = runtime.effective_default_model().await;
                    current_alias_owned.as_str()
                }
            };
            let current = runtime
                .resolve_model(current_alias)
                .await
                .ok_or_else(|| BridgeDispatchError::UnknownModel(current_alias.to_string()))?;
            if current.agent_id != selected.agent_id {
                return Err(BridgeDispatchError::CrossAdapterModelSwitch);
            }
            let mut native = request.clone();
            native["params"]["sessionId"] =
                Value::String(session.bound_gateway_session_id.expect("bound id"));
            native["params"]["value"] = Value::String(selected.model_id.clone());
            let mut response = dispatch_shared_for_tenant(router, tenant_id, native).await?;
            if response.get("error").is_none() {
                let updated = runtime.sessions.update_bound_model(
                    tenant_id,
                    &session_id,
                    selected.id.clone(),
                )?;
                persist_bridge_binding(router, runtime, &updated).await?;
                if let Some(result) = response.get_mut("result").and_then(Value::as_object_mut) {
                    // Always overwrite rather than `.entry().or_insert()`: the
                    // native backend's own response may carry its own
                    // "configOptions" in terms of native model ids, which
                    // would silently stick if we only filled in a missing
                    // key -- Zed (talking to the bridge) needs the bridge's
                    // public-alias view, stamped with the selection that
                    // was just applied, not whatever the native adapter
                    // happened to return.
                    result.insert(
                        "configOptions".to_string(),
                        runtime.model_config_options(Some(&selected.id)).await,
                    );
                }
            }
            Ok(response)
        }
    }
}

/// On-demand counterpart to [`BridgeRuntime::restore_recovered_sessions`]
/// (which only ever runs once, in bulk, at daemon startup, and only
/// restores rows the startup batch itself already marked `Restored`
/// within a fixed timeout window -- see that method's doc comment).
/// Every live request path that resolves an *existing* bridge session id
/// (`close_or_delete`, `forward_bound`) calls this first instead of
/// reading `BridgeSessionStore` directly: an in-memory hit returns
/// immediately, unchanged from before; a miss falls back to
/// `PersistenceStore::find_session_by_bridge_session_id` and, if a
/// still-open row for this exact bridge session id is found, restores it
/// into the in-memory store on the spot before continuing.
///
/// **Real live incident this closes.** A restart whose startup recovery
/// batch times out or never runs for a given session (a slow/cold
/// backend spawn, or the daemon simply not having gotten to it within
/// `ACPX_STARTUP_SESSION_RECOVERY_TIMEOUT_SECONDS`) used to permanently
/// orphan that session's bridge-visible virtual id: `restore_recovered_
/// sessions`'s bulk pass would never revisit it (it only runs once, at
/// startup), so every later request against that same id -- exactly
/// what a real client (Zed) reconnecting with its own locally-persisted
/// session id does -- failed with "bridge session ... was not found",
/// even though the underlying native session was still genuinely
/// recoverable via `Router::rehydrate_session` (which has no such
/// startup-only restriction). This also covers a same-process, non-
/// restart eviction from `BridgeSessionStore` for any reason not
/// specifically preserved by `Router::maybe_suppress_close`'s `_acpx.
/// backgroundClose` handling.
///
/// Deliberately not filtered to `RecoveryStatus::Restored` the way the
/// startup batch is (see `find_session_by_bridge_session_id`'s own
/// doc comment): the downstream native dispatch path independently
/// re-validates and re-establishes the underlying gateway session
/// regardless of what status a prior startup attempt left it in, exactly
/// as it already does for a client's own explicit `session/load`/
/// `session/resume` retry.
///
/// Two callers racing the same miss is handled without surfacing an
/// error to either: `BridgeSessionStore::restore_bound` rejects a second
/// insert over an already-present entry, which this treats as "someone
/// else already recovered it" and simply re-reads via `.get()`.
async fn resolve_or_recover_bridge_session(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    session_id: &BridgeSessionId,
) -> Result<BridgeSession, BridgeDispatchError> {
    if let Some(session) = runtime.sessions.get(tenant_id, session_id) {
        return Ok(session);
    }
    let not_found = || {
        BridgeDispatchError::from(BridgeSessionError::NotFound {
            tenant_id: tenant_id.0.clone(),
            session_id: session_id.0.clone(),
        })
    };
    let Some(store) = router.lock().await.persistence_store() else {
        return Err(not_found());
    };
    let Some(record) = store
        .find_session_by_bridge_session_id(tenant_id.0.clone(), session_id.0.clone())
        .await?
    else {
        return Err(not_found());
    };
    let (Some(virtual_id), Some(model_alias), Some(params)) = (
        record.bridge_session_id.clone(),
        record.bridge_model_alias.clone(),
        record.recovery_params.clone(),
    ) else {
        return Err(not_found());
    };
    let Ok(params) = serde_json::from_value::<NewSessionParams>(params) else {
        tracing::warn!(
            gateway_session_id = %record.gateway_session_id,
            "on-demand bridge session recovery: skipping malformed persisted parameters"
        );
        return Err(not_found());
    };
    let options = record
        .bridge_config_options
        .and_then(|value| serde_json::from_value::<HashMap<String, String>>(value).ok())
        .unwrap_or_default();
    // Errors here mean a racing caller already restored this exact
    // entry -- fall through to the `.get()` below rather than
    // propagating, per this function's own doc comment.
    let _ = runtime.sessions.restore_bound(
        tenant_id,
        BridgeSessionId(virtual_id),
        params,
        Some(model_alias),
        options,
        record.gateway_session_id,
    );
    runtime
        .sessions
        .get(tenant_id, session_id)
        .ok_or_else(not_found)
}

async fn close_or_delete(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
) -> Result<Value, BridgeDispatchError> {
    reject_acpx_extension_except_bg(&request)?;
    let is_delete = request.get("method").and_then(Value::as_str) == Some("session/delete");
    let session_id = request_session_id(&request)?;
    let session = resolve_or_recover_bridge_session(router, runtime, tenant_id, &session_id).await?;
    if session.state == BridgeSessionState::Unbound {
        runtime.sessions.remove(tenant_id, &session_id);
        return Ok(success(&request, json!({})));
    }
    let response = forward_session_request(router, tenant_id, &session, request).await?;
    // **Real ACP semantics, `close` vs `delete`.** `session/close` means
    // "free this session's resources", not "this session id is
    // permanently gone" -- the entire reason `sessionCapabilities.close`
    // and `.resume` both exist is so a client can free a session now and
    // legitimately `session/resume` the *same* id later (a real Zed
    // conversation-tab close/reopen does exactly this, reconnecting with
    // a brand new `AgentConnection`/bridge subprocess and calling
    // `resume_session` with the id it remembers -- see `agent_ui::
    // conversation_view`'s `resume_session` call site). Only `session/
    // delete` is the real "this id is gone for good" signal. So a
    // successful `session/close` must keep this bridge's own virtual
    // session-id mapping (`BridgeRuntime::sessions`, separate from the
    // native gateway `SessionRegistry` `Router::maybe_suppress_close`
    // manages) -- `forward_bound`'s `session/resume`/`session/load`
    // handling needs it to still find `bound_gateway_session_id` to
    // rewrite onto. `session/prompt` and friends against this same id
    // in the meantime still correctly fail: the *native* gateway session
    // is genuinely gone (unless background mode kept it alive), and
    // `rehydrate_session`'s restart-survival fallback only accepts
    // `session/load`/`session/resume`, not `session/prompt`. Found live,
    // running a real Zed-shaped close-then-resume round trip against
    // this feature: `session/resume` on a session this same bridge had
    // genuinely closed came back "bridge session not found", despite
    // ACPX's own `initialize` response advertising `sessionCapabilities.
    // resume` as fully supported.
    //
    // `_acpx.backgroundClose` (see `Router::maybe_suppress_close`'s doc
    // comment) is a stronger case of the same thing -- the underlying
    // gateway session was never even touched -- but is otherwise now
    // redundant with the `!is_delete` rule below; kept as an explicit,
    // documented marker (not just relied on implicitly) since a real
    // ACP client's response parser must never depend on it existing.
    if response.get("error").is_none() && is_delete {
        runtime.sessions.remove(tenant_id, &session_id);
    }
    Ok(response)
}

async fn forward_bound(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
    interaction: Option<&BridgeInteractionCtx>,
) -> Result<Value, BridgeDispatchError> {
    reject_acpx_extension(&request)?;
    let session_id = request_session_id(&request)?;
    let session = resolve_or_recover_bridge_session(router, runtime, tenant_id, &session_id).await?;
    let session = match session.state {
        BridgeSessionState::Unbound => {
            bind(router, runtime, tenant_id, &session_id, session).await?
        }
        BridgeSessionState::Binding => return Err(BridgeDispatchError::BindingInProgress),
        BridgeSessionState::Failed => return Err(BridgeDispatchError::BindingFailed),
        BridgeSessionState::Bound => session,
    };
    // Must happen before the round trip below, not after: this exact call
    // (the lazy-bind case above included) is where a backend-initiated
    // `session/request_permission` can be raised, and `try_forward_interaction`
    // only ever gets one shot per request -- no live binding yet means it
    // silently falls back to the static policy answer for this entire turn.
    if let (Some(ctx), Some(native_id)) = (interaction, session.bound_gateway_session_id.as_deref())
    {
        ctx.claim(tenant_id, native_id).await;
    }
    forward_session_request(router, tenant_id, &session, request).await
}

async fn fork_session(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
    interaction: Option<&BridgeInteractionCtx>,
) -> Result<Value, BridgeDispatchError> {
    reject_acpx_extension(&request)?;
    let source_id = request_session_id(&request)?;
    let source = runtime.sessions.get(tenant_id, &source_id).ok_or_else(|| {
        BridgeSessionError::NotFound {
            tenant_id: tenant_id.0.clone(),
            session_id: source_id.0.clone(),
        }
    })?;
    let source = match source.state {
        BridgeSessionState::Unbound => bind(router, runtime, tenant_id, &source_id, source).await?,
        BridgeSessionState::Binding => return Err(BridgeDispatchError::BindingInProgress),
        BridgeSessionState::Failed => return Err(BridgeDispatchError::BindingFailed),
        BridgeSessionState::Bound => source,
    };
    if let (Some(ctx), Some(native_id)) = (interaction, source.bound_gateway_session_id.as_deref())
    {
        ctx.claim(tenant_id, native_id).await;
    }
    let mut native_request = request.clone();
    native_request["params"]["sessionId"] = Value::String(
        source
            .bound_gateway_session_id
            .clone()
            .expect("bound bridge session has native gateway id"),
    );
    let mut response = dispatch_shared_for_tenant(router, tenant_id, native_request).await?;
    let Some(native_fork_id) = response
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(response);
    };
    let public_fork_id = runtime.sessions.register_bound(
        tenant_id,
        source.original_new_session_params,
        source.selected_public_model_alias,
        source.selected_adapter_config_options,
        native_fork_id.clone(),
    );
    if let Some(ctx) = interaction {
        // The fork's own native id is a distinct backend session; claim it
        // too so a prompt against the fork on this same connection also
        // gets live interactive forwarding without waiting on a second
        // round trip to discover it.
        ctx.claim(tenant_id, &native_fork_id).await;
    }
    let fork = runtime
        .sessions
        .get(tenant_id, &public_fork_id)
        .expect("fresh bridge fork exists");
    if let Err(error) = persist_bridge_binding(router, runtime, &fork).await {
        let _ = close_native_session(router, tenant_id, &native_fork_id).await;
        let _ = runtime.sessions.remove(tenant_id, &public_fork_id);
        return Err(error);
    }
    response["result"]["sessionId"] = Value::String(public_fork_id.0);
    Ok(response)
}

async fn bind(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    virtual_id: &BridgeSessionId,
    session: BridgeSession,
) -> Result<BridgeSession, BridgeDispatchError> {
    match runtime.sessions.begin_binding(tenant_id, virtual_id)? {
        BindingClaim::Owner => {}
        BindingClaim::Binding => return Err(BridgeDispatchError::BindingInProgress),
        BindingClaim::Failed => return Err(BridgeDispatchError::BindingFailed),
        BindingClaim::Bound => {
            return Ok(runtime
                .sessions
                .get(tenant_id, virtual_id)
                .expect("bound bridge session exists"))
        }
    }

    let model_alias_owned;
    let model_alias = match session
        .selected_public_model_alias
        .as_deref()
    {
        Some(alias) => alias,
        None => {
            model_alias_owned = runtime.effective_default_model().await;
            model_alias_owned.as_str()
        }
    };
    let model = match runtime.resolve_model(model_alias).await {
        Some(model) => model,
        None => {
            let _ = runtime.sessions.fail_binding(tenant_id, virtual_id);
            return Err(BridgeDispatchError::UnknownModel(model_alias.to_string()));
        }
    };

    let result = async {
        router
            .lock()
            .await
            .ensure_registry_agent_registered(&model.agent_id)
            .await?;

        let mut params = serde_json::to_value(&session.original_new_session_params)
            .expect("bridge session params serialize");
        // Some published ACP adapters (notably the current Claude ACP
        // adapter) validate `mcpServers` as required even when an ACP client
        // omitted it. The bridge owns this backend-facing normalization; the
        // public `/acp` request remains standard and profile-free.
        if params.get("mcpServers").is_none() {
            params["mcpServers"] = json!([]);
        }
        params["_acpx"] = json!({"agentId": model.agent_id});
        let native_new = json!({
            "jsonrpc": "2.0",
            "id": bridge_internal_id(virtual_id, "new"),
            "method": "session/new",
            "params": params,
        });
        let response = dispatch_shared_for_tenant(router, tenant_id, native_new).await?;
        if let Some(error) = response.get("error") {
            return Err(RouterError::BackendSessionNewError(error.clone()));
        }
        let native_id = response
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .ok_or(RouterError::MissingBackendSessionId)?
            .to_string();
        let native_select = json!({
            "jsonrpc": "2.0",
            "id": bridge_internal_id(virtual_id, "model"),
            "method": "session/set_config_option",
            "params": {
                "sessionId": native_id,
                "configId": "model",
                "value": model.model_id,
            }
        });
        let selection = dispatch_shared_for_tenant(router, tenant_id, native_select).await?;
        if let Some(error) = selection.get("error") {
            return Err(RouterError::BackendSessionNewError(error.clone()));
        }
        for (config_id, value) in &session.selected_adapter_config_options {
            let configured = dispatch_shared_for_tenant(
                router,
                tenant_id,
                json!({
                    "jsonrpc": "2.0",
                    "id": bridge_internal_id(virtual_id, config_id),
                    "method": "session/set_config_option",
                    "params": {
                        "sessionId": native_id,
                        "configId": config_id,
                        "value": value,
                    }
                }),
            )
            .await?;
            if let Some(error) = configured.get("error") {
                return Err(RouterError::BackendSessionNewError(error.clone()));
            }
        }
        Ok::<_, RouterError>(native_id)
    }
    .await;

    match result {
        Ok(native_id) => {
            let bound = runtime
                .sessions
                .finish_binding(tenant_id, virtual_id, native_id)?;
            if let Err(error) = persist_bridge_binding(router, runtime, &bound).await {
                let native_id = bound
                    .bound_gateway_session_id
                    .as_deref()
                    .expect("bound bridge session has native id");
                let _ = close_native_session(router, tenant_id, native_id).await;
                let _ = runtime.sessions.remove(tenant_id, virtual_id);
                return Err(error);
            }
            Ok(bound)
        }
        Err(error) => {
            let _ = runtime.sessions.fail_binding(tenant_id, virtual_id);
            Err(error.into())
        }
    }
}

async fn persist_bridge_binding(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    session: &BridgeSession,
) -> Result<(), BridgeDispatchError> {
    let Some(store) = router.lock().await.persistence_store() else {
        return Ok(());
    };
    let fallback_default_model;
    store
        .update_bridge_binding(
            session
                .bound_gateway_session_id
                .clone()
                .expect("bound bridge session has native id"),
            session.id.0.clone(),
            match session.selected_public_model_alias.clone() {
                Some(alias) => alias,
                None => {
                    fallback_default_model = runtime.effective_default_model().await;
                    fallback_default_model.clone()
                }
            },
            serde_json::to_value(&session.selected_adapter_config_options)
                .expect("bridge adapter options serialize"),
        )
        .await?;
    Ok(())
}

async fn close_native_session(
    router: &SharedRouter,
    tenant_id: &TenantId,
    gateway_session_id: &str,
) -> Result<(), BridgeDispatchError> {
    dispatch_shared_for_tenant(
        router,
        tenant_id,
        json!({
            "jsonrpc": "2.0",
            "id": bridge_internal_id(&BridgeSessionId(gateway_session_id.to_string()), "close"),
            "method": "session/close",
            "params": {"sessionId": gateway_session_id},
        }),
    )
    .await?;
    Ok(())
}

async fn forward_session_request(
    router: &SharedRouter,
    tenant_id: &TenantId,
    session: &BridgeSession,
    mut request: Value,
) -> Result<Value, BridgeDispatchError> {
    request["params"]["sessionId"] = Value::String(
        session
            .bound_gateway_session_id
            .clone()
            .expect("bound bridge session has native gateway id"),
    );
    let mut response = dispatch_shared_for_tenant(router, tenant_id, request).await?;
    if let Some(result) = response.get_mut("result") {
        if result.get("sessionId").is_some() {
            result["sessionId"] = Value::String(session.id.0.clone());
        }
    }
    // Buffered updates are read directly from the backend stream, so their
    // params still contain the backend-native session id. Normal live hub
    // delivery translates them earlier; this path covers the first lazy-bind
    // turn before a WebSocket subscription exists.
    if let Some(updates) = response
        .pointer_mut("/_acpx/updates")
        .and_then(Value::as_array_mut)
    {
        for update in updates {
            if update.pointer("/params/sessionId").is_some() {
                update["params"]["sessionId"] = Value::String(session.id.0.clone());
            }
        }
    }
    Ok(response)
}

fn reject_acpx_extension(request: &Value) -> Result<(), BridgeDispatchError> {
    if request.pointer("/params/_acpx").is_some() {
        Err(BridgeDispatchError::AcpxExtensionNotAllowed)
    } else {
        Ok(())
    }
}

/// `close_or_delete`'s narrow relaxation of [`reject_acpx_extension`]:
/// `_acpx.bg` (see `acpx_core::router`'s `take_background_override` and
/// `LifecycleConfig::background_mode`'s doc comment) is the one
/// acpx-specific extension field this otherwise byte-for-byte
/// spec-conformant bridge accepts, because it's purely additive and
/// silently ignorable by any real ACP client that never sends it --
/// exactly the "ignore unrecognized fields" contract every conformant
/// JSON-RPC implementation already has to honor on both sides, so
/// accepting it here breaks nothing for a client that doesn't opt in.
/// Router-side dispatch strips it before anything is ever forwarded to
/// a real backend. Any other `_acpx.*` key alongside `bg` is still
/// rejected, same as `reject_acpx_extension`.
fn reject_acpx_extension_except_bg(request: &Value) -> Result<(), BridgeDispatchError> {
    let Some(extension) = request.pointer("/params/_acpx").and_then(Value::as_object) else {
        return Ok(());
    };
    if extension.keys().any(|key| key != "bg") {
        return Err(BridgeDispatchError::AcpxExtensionNotAllowed);
    }
    Ok(())
}

fn request_session_id(request: &Value) -> Result<BridgeSessionId, BridgeDispatchError> {
    request
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .map(|id| BridgeSessionId(id.to_string()))
        .ok_or(BridgeDispatchError::MissingSessionId)
}

fn success(request: &Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": result,
    })
}

fn bridge_internal_id(session_id: &BridgeSessionId, operation: &str) -> Value {
    // ACP permits numeric ids, and fixed values are safe here because a
    // connector serializes requests on its stdio stream. Numeric ids also
    // keep older ACP adapter test doubles that only parse numeric ids
    // interoperable with the bridge's internal setup calls.
    let _ = (session_id, operation);
    json!(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpx_core::persistence::{
        sessions::{RecoveryMetadata, RecoveryMethod},
        PersistenceStore,
    };

    /// **`_acpx.bg` bridge carve-out.** The strict `/acp` bridge rejects
    /// any `_acpx` extension on every method except `session/close`/
    /// `session/delete`'s narrow `bg`-only allowance -- see
    /// `LifecycleConfig::background_mode`'s doc comment for the feature.
    /// A real ACP client that never sends `_acpx` at all must remain
    /// entirely unaffected (empty extension object and no extension
    /// object are both accepted); a client that sends `bg` alone is
    /// accepted; any other key, alone or alongside `bg`, is still
    /// rejected exactly like `reject_acpx_extension` would reject it.
    #[test]
    fn reject_acpx_extension_except_bg_only_allows_the_bg_key() {
        let no_extension = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/close",
            "params": {"sessionId": "s1"}
        });
        assert!(reject_acpx_extension_except_bg(&no_extension).is_ok());

        let bg_only = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/close",
            "params": {"sessionId": "s1", "_acpx": {"bg": "off"}}
        });
        assert!(reject_acpx_extension_except_bg(&bg_only).is_ok());

        let bg_boolean = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/close",
            "params": {"sessionId": "s1", "_acpx": {"bg": true}}
        });
        assert!(reject_acpx_extension_except_bg(&bg_boolean).is_ok());

        let unrelated_extension = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/close",
            "params": {"sessionId": "s1", "_acpx": {"profile": "work"}}
        });
        assert!(matches!(
            reject_acpx_extension_except_bg(&unrelated_extension),
            Err(BridgeDispatchError::AcpxExtensionNotAllowed)
        ));

        let bg_plus_other = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/close",
            "params": {"sessionId": "s1", "_acpx": {"bg": "off", "profile": "work"}}
        });
        assert!(matches!(
            reject_acpx_extension_except_bg(&bg_plus_other),
            Err(BridgeDispatchError::AcpxExtensionNotAllowed)
        ));
    }

    /// **Live-incident regression test.** `background_mode`'s `session/
    /// close` suppression (`Router::maybe_suppress_close`) keeps the
    /// underlying *gateway* session alive, but this bridge separately
    /// tracks its own virtual-session-id -> gateway-session-id mapping
    /// (`BridgeRuntime::sessions`) -- found live, running a real
    /// Zed-shaped WS round trip against this feature, that
    /// `close_or_delete` unconditionally dropped *that* mapping on any
    /// successful `session/close` response, so a client (like Zed) that
    /// keeps using the same session id after a suppressed close got
    /// "bridge session not found" on its very next call despite the
    /// backend session being fully alive. Fixed via the `_acpx.
    /// backgroundClose` response marker `close_or_delete` now checks
    /// before deciding whether to evict its own mapping. Exercises a
    /// real lazy-bind-on-first-prompt sequence (closing an `Unbound`
    /// session -- before any prompt -- is a different, already-correct
    /// code path with nothing to preserve, so this deliberately prompts
    /// once first).
    #[tokio::test]
    async fn background_mode_close_keeps_the_bridges_own_virtual_session_mapping_too() {
        use acpx_core::router::Router;
        use acpx_core::LifecycleConfig;
        use tokio::sync::Mutex as AsyncMutex;

        const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

        let mut router = Router::new("stand-in-agent".to_string()).with_lifecycle_config(
            LifecycleConfig {
                background_mode: true,
                ..LifecycleConfig::default()
            },
        );
        router.register_agent(
            "stand-in-agent",
            acpx_conductor::SpawnSpec::new(
                "sh",
                vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
            ),
        );
        let router: SharedRouter = Arc::new(AsyncMutex::new(router));

        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "stand-in/default".to_string(),
            models: vec![BridgeModel {
                id: "stand-in/default".to_string(),
                name: None,
                agent_id: "stand-in-agent".to_string(),
                model_id: "default".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));
        let tenant_id = TenantId::from("tenant-a");

        let new_response = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new");
        let sid = new_response["result"]["sessionId"].as_str().unwrap().to_string();

        // Binds for real (lazy binding happens on the first prompt).
        dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": sid, "prompt": []}}),
        )
        .await
        .expect("first session/prompt binds the session");

        let close_response = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/close", "params": {"sessionId": sid}}),
        )
        .await
        .expect("background-mode session/close");
        assert_eq!(close_response["result"], json!({}));

        let prompt_after_close = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid, "prompt": []}}),
        )
        .await;
        assert!(
            prompt_after_close.is_ok(),
            "the bridge's own virtual session mapping must survive a \
             background-suppressed close, not just the gateway session: \
             {prompt_after_close:?}"
        );
    }

    /// **Live-incident regression test.** Seeds a persisted session row
    /// exactly the way a real gateway session ends up on disk (via
    /// `persist_bridge_binding`'s `update_bridge_binding` call after a
    /// live bind), but marks its `RecoveryStatus` `RecoveryFailed` --
    /// simulating a startup recovery batch that timed out or never ran
    /// for this session (the real failure mode this round's live
    /// incident traced back to) -- and, crucially, never calls
    /// `restore_recovered_sessions` at all, so `BridgeRuntime::sessions`
    /// starts out with no in-memory entry for it whatsoever, unlike
    /// `restored_native_session_rebuilds_its_virtual_bridge_mapping`
    /// above (which exercises the *bulk* startup path this test
    /// deliberately bypasses). A live `session/resume` call against this
    /// exact bridge session id -- the same call Zed's own
    /// `AcpConnection::resume_session` makes when reconnecting with a
    /// locally-remembered session id -- must still succeed via
    /// `resolve_or_recover_bridge_session`'s on-demand persistence
    /// fallback, not fail with "bridge session ... not found".
    /// **Real live-production regression test.** Reproduces the exact
    /// incident this round chased: a bridge session that was genuinely,
    /// durably closed (`status = 'closed'`, `closed_at` set -- what a
    /// real `session/close` round trip persists) must still be
    /// resumable via `session/resume` on the same bridge session id, at
    /// any point afterward, matching `Router::rehydrate_session`'s own
    /// "no status/closed_at restriction for `session/load`/`session/
    /// resume`" behavior -- this is not a bug in the fix, it's the
    /// entire reason `sessionCapabilities.resume` exists as a *distinct*
    /// capability from `.close`. Confirmed against a real production
    /// row: `sqlite3 ... "SELECT status, closed_at FROM sessions WHERE
    /// bridge_session_id = '...'"` returned `closed | 1784439119...`
    /// for a session Zed's own reconnect reported as "bridge session ...
    /// not found" against the *pre-fix* deployed binary.
    #[tokio::test]
    async fn bridge_session_resumable_after_a_genuine_durable_close() {
        use acpx_core::router::Router;
        use tokio::sync::Mutex as AsyncMutex;

        const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

        let store = PersistenceStore::open_in_memory().expect("persistence store");
        store
            .record_session_with_recovery(
                "gateway-closed",
                "stand-in-agent",
                "backend-closed",
                None,
                "2026-07-19T00:00:00Z",
                "tenant-a",
                RecoveryMetadata {
                    cwd: Some("/tmp".to_string()),
                    recovery_params: Some(json!({"cwd": "/tmp", "mcpServers": []})),
                    status: RecoveryStatus::Restored,
                    recovery_method: RecoveryMethod::Load,
                    bridge_session_id: Some("virtual-closed".to_string()),
                    bridge_model_alias: Some("stand-in/default".to_string()),
                    bridge_config_options: Some(json!({})),
                    ..RecoveryMetadata::default()
                },
            )
            .await
            .expect("seed row");
        // Mirrors exactly what `Router::dispatch_proxied_shared`'s
        // `session/close` handling persists on a real close.
        store
            .close_session("gateway-closed", "2026-07-19T00:05:00Z")
            .await
            .expect("mark the row durably closed");

        let mut router =
            Router::new("stand-in-agent".to_string()).with_persistence(store.clone());
        router.register_agent(
            "stand-in-agent",
            acpx_conductor::SpawnSpec::new(
                "sh",
                vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
            ),
        );
        let router: SharedRouter = Arc::new(AsyncMutex::new(router));

        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "stand-in/default".to_string(),
            models: vec![BridgeModel {
                id: "stand-in/default".to_string(),
                name: None,
                agent_id: "stand-in-agent".to_string(),
                model_id: "default".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));
        let tenant_id = TenantId::from("tenant-a");

        let resume_response = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/resume",
                "params": {"sessionId": "virtual-closed", "cwd": "/tmp"}
            }),
        )
        .await
        .expect("session/resume must succeed against a genuinely closed bridge session");
        assert!(
            resume_response.get("error").is_none(),
            "resuming a closed session must produce a real success response, not \
             a lingering 'bridge session not found': {resume_response:?}"
        );
    }

    #[tokio::test]
    async fn bridge_session_recovers_on_demand_when_startup_recovery_never_restored_it() {
        use acpx_core::router::Router;
        use tokio::sync::Mutex as AsyncMutex;

        const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

        let store = PersistenceStore::open_in_memory().expect("persistence store");
        store
            .record_session_with_recovery(
                "gateway-on-demand",
                "stand-in-agent",
                "backend-on-demand",
                None,
                "2026-07-19T00:00:00Z",
                "tenant-a",
                RecoveryMetadata {
                    cwd: Some("/tmp".to_string()),
                    recovery_params: Some(json!({"cwd": "/tmp", "mcpServers": []})),
                    status: RecoveryStatus::RecoveryFailed,
                    recovery_method: RecoveryMethod::Resume,
                    last_recovery_error: Some("startup recovery timed out".to_string()),
                    bridge_session_id: Some("virtual-on-demand".to_string()),
                    bridge_model_alias: Some("stand-in/default".to_string()),
                    bridge_config_options: Some(json!({})),
                    ..RecoveryMetadata::default()
                },
            )
            .await
            .expect("seed a startup-recovery-failed but still-open session row");

        let mut router =
            Router::new("stand-in-agent".to_string()).with_persistence(store.clone());
        router.register_agent(
            "stand-in-agent",
            acpx_conductor::SpawnSpec::new(
                "sh",
                vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
            ),
        );
        let router: SharedRouter = Arc::new(AsyncMutex::new(router));

        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "stand-in/default".to_string(),
            models: vec![BridgeModel {
                id: "stand-in/default".to_string(),
                name: None,
                agent_id: "stand-in-agent".to_string(),
                model_id: "default".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));
        let tenant_id = TenantId::from("tenant-a");

        // Deliberately never call `runtime.restore_recovered_sessions`
        // here -- `runtime.sessions` starts genuinely empty, exactly
        // like a fresh process whose startup batch skipped/timed out on
        // this session.
        let resume_response = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/resume",
                "params": {"sessionId": "virtual-on-demand", "cwd": "/tmp"}
            }),
        )
        .await
        .expect("session/resume must recover the bridge mapping on demand, not error");
        assert!(
            resume_response.get("error").is_none(),
            "on-demand recovery must produce a real success response: {resume_response:?}"
        );

        // The mapping is now genuinely present in memory -- a follow-up
        // call no longer needs the persistence fallback at all.
        assert!(
            runtime
                .sessions
                .get(&tenant_id, &BridgeSessionId("virtual-on-demand".to_string()))
                .is_some(),
            "on-demand recovery must actually populate BridgeSessionStore, not just answer once"
        );
    }

    #[tokio::test]
    async fn restored_native_session_rebuilds_its_virtual_bridge_mapping() {
        let store = PersistenceStore::open_in_memory().expect("persistence store");
        store
            .record_session_with_recovery(
                "gateway-restored",
                "codex-acp",
                "backend-restored",
                None,
                "2026-07-16T00:00:00Z",
                "tenant-a",
                RecoveryMetadata {
                    cwd: Some("/workspace".to_string()),
                    recovery_params: Some(json!({"cwd": "/workspace", "mcpServers": []})),
                    status: RecoveryStatus::Restored,
                    recovery_method: RecoveryMethod::Load,
                    last_recovery_error: None,
                    bridge_session_id: Some("virtual-restored".to_string()),
                    bridge_model_alias: Some("codex/gpt-5".to_string()),
                    bridge_config_options: Some(json!({"permissionMode": "acceptEdits"})),
                    ..RecoveryMetadata::default()
                },
            )
            .await
            .expect("seed restored native session");

        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "codex/gpt-5".to_string(),
            models: vec![BridgeModel {
                id: "codex/gpt-5".to_string(),
                name: None,
                agent_id: "codex-acp".to_string(),
                model_id: "gpt-5".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));
        assert_eq!(
            runtime
                .restore_recovered_sessions(&store)
                .await
                .expect("restore bridge mapping"),
            1
        );
        let tenant = TenantId::from("tenant-a");
        assert_eq!(
            runtime.bound_gateway_session_id(&tenant, "virtual-restored"),
            Some("gateway-restored".to_string())
        );
        let session = runtime
            .sessions
            .get(&tenant, &BridgeSessionId("virtual-restored".to_string()))
            .expect("restored virtual session");
        assert_eq!(
            session
                .selected_adapter_config_options
                .get("permissionMode"),
            Some(&"acceptEdits".to_string())
        );
    }

    /// **Live incident regression test for `MODEL_REFRESH_COOLDOWN`.**
    /// Registers one agent backed by a real, deliberately silent
    /// subprocess (`sh -c 'cat > /dev/null'` -- never answers on stdout,
    /// mirrors `router.rs`'s own wedged-backend test), so every capability
    /// probe against it is guaranteed to actually block until
    /// `probe_timeout` elapses rather than resolving instantly by luck.
    /// A call that genuinely re-probes takes at least `probe_timeout`;
    /// a call the cooldown correctly skips returns near-instantly. This
    /// distinguishes "cooldown skipped the probe" from "probe happened to
    /// be fast" in a way pure timing assertions against a real network
    /// probe never could.
    #[tokio::test]
    async fn refresh_models_cooldown_skips_redundant_probes_of_a_wedged_backend() {
        use acpx_core::router::Router;
        use tokio::sync::Mutex as AsyncMutex;

        let mut router = Router::new("codex-acp".to_string());
        router.register_agent(
            "codex-acp",
            acpx_conductor::SpawnSpec::new(
                "sh",
                vec!["-c".to_string(), "cat > /dev/null".to_string()],
            ),
        );
        let router: SharedRouter = Arc::new(AsyncMutex::new(router));

        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "codex/default".to_string(),
            models: vec![BridgeModel {
                id: "codex/default".to_string(),
                name: None,
                agent_id: "codex-acp".to_string(),
                model_id: "default".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));

        let probe_timeout = Duration::from_millis(150);
        let cooldown = Duration::from_millis(600);

        let first_start = std::time::Instant::now();
        runtime
            .refresh_models_with_config(&router, probe_timeout, cooldown)
            .await;
        let first_elapsed = first_start.elapsed();
        assert!(
            first_elapsed >= probe_timeout,
            "first call must actually probe the wedged backend and block \
             for the full probe_timeout, took {first_elapsed:?}"
        );

        let second_start = std::time::Instant::now();
        runtime
            .refresh_models_with_config(&router, probe_timeout, cooldown)
            .await;
        let second_elapsed = second_start.elapsed();
        assert!(
            second_elapsed < probe_timeout,
            "second call within the cooldown window must skip the probe \
             entirely and return near-instantly, took {second_elapsed:?}"
        );

        tokio::time::sleep(cooldown).await;

        let third_start = std::time::Instant::now();
        runtime
            .refresh_models_with_config(&router, probe_timeout, cooldown)
            .await;
        let third_elapsed = third_start.elapsed();
        assert!(
            third_elapsed >= probe_timeout,
            "third call after the cooldown expired must probe again, \
             took {third_elapsed:?}"
        );
    }

    /// **Fix regression test for the reported live incident.**
    /// Reproduced `"bridge session binding is in progress; retry the
    /// request"` (`BridgeDispatchError::BindingInProgress`) never
    /// clearing: [`bind`]'s lazy-binding work is the `session/new` +
    /// `session/set_config_option` round trip against the real backend
    /// process, run entirely inside `dispatch_shared_for_tenant` ->
    /// `dispatch_session_new_shared` -> `ensure_backend_initialized`.
    /// Before `router.rs`'s `BACKEND_HANDSHAKE_TIMEOUT` fix,
    /// `ensure_backend_initialized`'s `initialize` handshake read had no
    /// timeout of its own, so a backend that never answered `initialize`
    /// left [`bind`]'s `result = async { ... }.await` block permanently
    /// pending: neither `finish_binding` nor `fail_binding` was ever
    /// called, so [`BridgeSessionState`] stayed `Binding` forever, and
    /// every retry -- including the exact one the error message itself
    /// instructs -- kept returning `BindingInProgress` indefinitely.
    ///
    /// Now that the handshake read is bounded, this proves the livelock
    /// is actually broken: the original `session/prompt` eventually
    /// resolves with a clear backend error (not the client's own
    /// timeout), `bind()`'s `Err` branch calls `fail_binding`, moving
    /// [`BridgeSessionState`] to `Failed` -- and a subsequent retry
    /// immediately observes the distinct, terminal `BindingFailed`
    /// (`"bridge session binding previously failed; create a new
    /// session"`), never `BindingInProgress` again. That is a real,
    /// qualitative behavior change a client can act on (create a new
    /// session) instead of a livelock it can only spin against forever.
    /// See `acpx-core/tests/session_load_backend_handshake_timeout_test.rs`
    /// for the lower-level `Router::dispatch` proof, and
    /// `acp_bridge_binary_test.rs` for the real-process, real-HTTP
    /// version of this same proof.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_binding_eventually_fails_cleanly_and_stops_livelocking_when_the_backend_never_answers_initialize(
    ) {
        use acpx_core::router::Router;
        use tokio::sync::Mutex as AsyncMutex;

        // Same silent-backend idiom as `refresh_models_cooldown_skips_
        // redundant_probes_of_a_wedged_backend` just above: spawns, never
        // writes a single byte to stdout, so `initialize` is guaranteed
        // to never be answered.
        let mut router = Router::new("stand-in-agent".to_string());
        router.register_agent(
            "stand-in-agent",
            acpx_conductor::SpawnSpec::new(
                "sh",
                vec!["-c".to_string(), "cat > /dev/null".to_string()],
            ),
        );
        let router: SharedRouter = Arc::new(AsyncMutex::new(router));

        let runtime = Arc::new(BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "stand-in/default".to_string(),
            models: vec![BridgeModel {
                id: "stand-in/default".to_string(),
                name: None,
                agent_id: "stand-in-agent".to_string(),
                model_id: "default".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        })));
        let tenant_id = TenantId::from("tenant-a");

        // Pre-warm `refresh_models`'s cooldown gate with a short probe
        // timeout so the real `dispatch()` calls below (which always use
        // the production `MODEL_PROBE_TIMEOUT`/`MODEL_REFRESH_COOLDOWN`
        // constants) skip re-probing this wedged backend and this test
        // isn't stuck paying `MODEL_PROBE_TIMEOUT` (20s) on top of the
        // gap this test actually exercises -- an unrelated, already-
        // fixed cost (see `refresh_models_cooldown_skips_redundant_
        // probes_of_a_wedged_backend` above), not part of what this
        // test is proving.
        runtime
            .refresh_models_with_config(&router, Duration::from_millis(50), Duration::from_secs(60))
            .await;

        // `session/new` never touches the backend at all (lazy binding),
        // so this returns immediately with a virtual session id.
        let new_response = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new");
        let sid = new_response["result"]["sessionId"].as_str().unwrap().to_string();

        // The first prompt triggers `bind()`, which claims `Binding`
        // ownership and then blocks inside `ensure_backend_initialized`
        // until `BACKEND_HANDSHAKE_TIMEOUT` (30s) elapses. Spawned so
        // this test can observe the `BindingInProgress` window in
        // between, then await the same handle to observe the eventual
        // resolution.
        let first_router = Arc::clone(&router);
        let first_runtime = Arc::clone(&runtime);
        let first_tenant = tenant_id.clone();
        let first_sid = sid.clone();
        let first_prompt = tokio::spawn(async move {
            dispatch(
                &first_router,
                &first_runtime,
                &first_tenant,
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                    "params": {"sessionId": first_sid, "prompt": []}
                }),
            )
            .await
        });

        // Give the spawned prompt time to actually claim `Binding`
        // ownership and reach the wedged `initialize` read.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !first_prompt.is_finished(),
            "the first prompt should still be blocked inside bind() at this point"
        );

        // The exact retry the error message instructs a client to make,
        // made while binding is still genuinely in flight.
        let retry = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": sid, "prompt": []}
            }),
        )
        .await;
        assert!(
            matches!(retry, Err(BridgeDispatchError::BindingInProgress)),
            "expected the documented 'retry the request' error, got {retry:?}"
        );

        // The livelock is broken: awaiting the original call (bounded
        // generously above the real 30s `BACKEND_HANDSHAKE_TIMEOUT`, so
        // a regression back to an unbounded hang still fails this test
        // deterministically instead of wedging the suite) shows it
        // eventually resolves with the backend's own handshake-timeout
        // error, not the client's timeout.
        let first_outcome = tokio::time::timeout(Duration::from_secs(40), first_prompt)
            .await
            .expect("the original session/prompt must eventually resolve, not hang forever")
            .expect("spawned task must not panic");
        assert!(
            matches!(
                first_outcome,
                Err(BridgeDispatchError::Router(RouterError::BackendHandshakeTimeout(
                    "initialize",
                    _
                )))
            ),
            "expected the original call to fail with the backend's own handshake timeout, \
             got {first_outcome:?}"
        );

        // A retry after that point must observe the distinct, terminal
        // `BindingFailed` -- never `BindingInProgress` again. This is
        // the qualitative proof the livelock is gone: a real client now
        // gets an actionable "create a new session" error instead of an
        // instruction to retry something that can never succeed.
        let post_failure_retry = dispatch(
            &router,
            &runtime,
            &tenant_id,
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                "params": {"sessionId": sid, "prompt": []}
            }),
        )
        .await;
        assert!(
            matches!(post_failure_retry, Err(BridgeDispatchError::BindingFailed)),
            "expected BindingFailed once the handshake timeout has moved the session out of \
             Binding, got {post_failure_retry:?}"
        );
    }

    /// **`bridge_no_static_models_required`.** With `models: vec![]` and
    /// `default_model: String::new()` -- the normal shape now that the
    /// bridge's model list comes from provisioned-profile-driven live
    /// discovery rather than a static override -- a brand-new session
    /// still needs *some* default to resolve against the moment
    /// discovery has found anything. `effective_default_model` must
    /// fall back to the first live-discovered entry rather than
    /// resolving an empty alias (which every caller would otherwise
    /// treat as `UnknownModel`).
    #[tokio::test]
    async fn effective_default_model_falls_back_to_first_discovered_model_when_unset() {
        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: String::new(),
            models: vec![],
            max_virtual_sessions_per_tenant: None,
        }));
        assert_eq!(runtime.effective_default_model().await, "");

        runtime.models.write().await.push(BridgeModel {
            id: "claude/sonnet".to_string(),
            name: Some("Claude Sonnet".to_string()),
            agent_id: "claude-acp".to_string(),
            model_id: "sonnet".to_string(),
        });
        assert_eq!(runtime.effective_default_model().await, "claude/sonnet");
    }

    /// An operator-pinned `default_model` must still win over whatever
    /// discovery has found, even after live models exist -- this is the
    /// "explicit override always wins" half of the same fallback.
    #[tokio::test]
    async fn effective_default_model_prefers_the_configured_pin_over_discovered_models() {
        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: "claude/haiku".to_string(),
            models: vec![BridgeModel {
                id: "claude/haiku".to_string(),
                name: None,
                agent_id: "claude-acp".to_string(),
                model_id: "haiku".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));
        runtime.models.write().await.push(BridgeModel {
            id: "claude/opus".to_string(),
            name: None,
            agent_id: "claude-acp".to_string(),
            model_id: "opus".to_string(),
        });
        assert_eq!(runtime.effective_default_model().await, "claude/haiku");
    }

    /// **`bridge_model_seed_pruning`.** A previously-discovered model must
    /// disappear once a *successful* re-probe of its owning agent no
    /// longer reports it -- the bug flagged live: Bifrost catalog
    /// entries like `claude/claude-fable-5[1m]` staying `available: true`
    /// forever even after the upstream catalog dropped them, because the
    /// old merge logic only ever called `Vec::extend`. Exercised directly
    /// against the private `models`/`config_options` state (this is an
    /// invariant of `refresh_models_with_config`'s merge step, not of any
    /// specific probe transport) rather than a real subprocess probe, to
    /// stay fast and hermetic.
    #[tokio::test]
    async fn stale_discovered_models_are_pruned_when_their_agent_no_longer_reports_them() {
        let runtime = BridgeRuntime::new(Arc::new(BridgeConfig {
            default_model: String::new(),
            models: vec![BridgeModel {
                id: "pinned/model".to_string(),
                name: None,
                agent_id: "pinned-agent".to_string(),
                model_id: "pinned".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        }));
        {
            let mut models = runtime.models.write().await;
            models.push(BridgeModel {
                id: "claude/claude-fable-5[1m]".to_string(),
                name: Some("Fable".to_string()),
                agent_id: "claude-acp".to_string(),
                model_id: "claude-fable-5[1m]".to_string(),
            });
            models.push(BridgeModel {
                id: "claude/sonnet".to_string(),
                name: Some("Sonnet".to_string()),
                agent_id: "claude-acp".to_string(),
                model_id: "sonnet".to_string(),
            });
        }

        // `claude-acp` answered again this cycle but its catalog shrank
        // to just `sonnet` -- `claude-fable-5[1m]` is gone upstream.
        let discovered = vec![BridgeModel {
            id: "claude/sonnet".to_string(),
            name: Some("Sonnet".to_string()),
            agent_id: "claude-acp".to_string(),
            model_id: "sonnet".to_string(),
        }];
        let succeeded_agent_ids: HashSet<String> = ["claude-acp".to_string()].into_iter().collect();
        {
            let mut models = runtime.models.write().await;
            let static_config = runtime.config();
            let static_ids: HashSet<&str> =
                static_config.models.iter().map(|m| m.id.as_str()).collect();
            BridgeRuntime::merge_discovered_models(
                &mut models,
                &static_ids,
                &succeeded_agent_ids,
                discovered,
            );
        }

        let ids: Vec<String> = runtime
            .models
            .read()
            .await
            .iter()
            .map(|m| m.id.clone())
            .collect();
        assert!(
            !ids.contains(&"claude/claude-fable-5[1m]".to_string()),
            "a model its agent no longer reports must be pruned, got {ids:?}"
        );
        assert!(ids.contains(&"claude/sonnet".to_string()), "still-reported model must remain");
        assert!(
            ids.contains(&"pinned/model".to_string()),
            "a static/pinned model from a different, un-probed agent must never be pruned"
        );
    }
}
