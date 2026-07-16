//! Strict ACP request dispatcher for the optional `/acp` surface.
//!
//! The public protocol has no ACPX profile or adapter selector. A bridge
//! session starts as an in-memory virtual session, selects one policy-owned
//! model alias, and binds to a regular ACPX gateway session only when a turn
//! needs a backend. This module deliberately delegates all backend work to
//! `acpx_core::Router`; it does not own a second process manager.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    pub sender: mpsc::UnboundedSender<Value>,
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
#[derive(Clone)]
pub struct BridgeRuntime {
    pub config: Arc<BridgeConfig>,
    pub sessions: BridgeSessionStore,
    models: Arc<RwLock<Vec<BridgeModel>>>,
    config_options: Arc<RwLock<Vec<Value>>>,
}

impl BridgeRuntime {
    pub fn new(config: Arc<BridgeConfig>) -> Self {
        Self {
            models: Arc::new(RwLock::new(config.models.clone())),
            config_options: Arc::new(RwLock::new(Vec::new())),
            config,
            sessions: BridgeSessionStore::new(),
        }
    }

    /// Refreshes public models from each configured adapter's cached
    /// capability probe. Static entries remain as an operator fallback, but
    /// every discovered model is exposed without hand-maintaining aliases.
    pub async fn refresh_models(&self, router: &SharedRouter) {
        let agent_ids: Vec<String> = self
            .config
            .agent_ids()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        let mut discovered = Vec::new();
        let mut discovered_options = Vec::new();
        for agent_id in agent_ids {
            let capabilities = {
                let mut router = router.lock().await;
                router.probe_adapter_capabilities(&agent_id, "/tmp").await
            };
            let Ok(capabilities) = capabilities else {
                continue;
            };
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
        if discovered.is_empty() {
            return;
        }
        let mut models = self.models.write().await;
        let mut seen: HashSet<String> = models.iter().map(|model| model.id.clone()).collect();
        models.extend(
            discovered
                .into_iter()
                .filter(|model| seen.insert(model.id.clone())),
        );
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

    pub async fn resolve_model(&self, alias: &str) -> Option<BridgeModel> {
        self.models
            .read()
            .await
            .iter()
            .find(|model| model.id == alias)
            .cloned()
    }

    pub async fn model_config_options(&self) -> Value {
        let models = self.models.read().await;
        let mut options = vec![json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": self.config.default_model,
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
        runtime.config.max_virtual_sessions_per_tenant,
    )?;
    Ok(json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": {
            "sessionId": session_id.0,
            "configOptions": runtime.model_config_options().await,
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
                    json!({"configOptions": runtime.model_config_options().await}),
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
                json!({"configOptions": runtime.model_config_options().await}),
            ))
        }
        BridgeSessionState::Binding => Err(BridgeDispatchError::BindingInProgress),
        BridgeSessionState::Failed => Err(BridgeDispatchError::BindingFailed),
        BridgeSessionState::Bound => {
            let current_alias = session
                .selected_public_model_alias
                .as_deref()
                .unwrap_or(&runtime.config.default_model);
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
                    result
                        .entry("configOptions".to_string())
                        .or_insert(runtime.model_config_options().await);
                }
            }
            Ok(response)
        }
    }
}

async fn close_or_delete(
    router: &SharedRouter,
    runtime: &BridgeRuntime,
    tenant_id: &TenantId,
    request: Value,
) -> Result<Value, BridgeDispatchError> {
    reject_acpx_extension(&request)?;
    let session_id = request_session_id(&request)?;
    let Some(session) = runtime.sessions.get(tenant_id, &session_id) else {
        return Err(BridgeSessionError::NotFound {
            tenant_id: tenant_id.0.clone(),
            session_id: session_id.0.clone(),
        }
        .into());
    };
    if session.state == BridgeSessionState::Unbound {
        runtime.sessions.remove(tenant_id, &session_id);
        return Ok(success(&request, json!({})));
    }
    let response = forward_session_request(router, tenant_id, &session, request).await?;
    if response.get("error").is_none() {
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
    let session = runtime
        .sessions
        .get(tenant_id, &session_id)
        .ok_or_else(|| BridgeSessionError::NotFound {
            tenant_id: tenant_id.0.clone(),
            session_id: session_id.0.clone(),
        })?;
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

    let model_alias = session
        .selected_public_model_alias
        .as_deref()
        .unwrap_or(&runtime.config.default_model);
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
    store
        .update_bridge_binding(
            session
                .bound_gateway_session_id
                .clone()
                .expect("bound bridge session has native id"),
            session.id.0.clone(),
            session
                .selected_public_model_alias
                .clone()
                .unwrap_or_else(|| runtime.config.default_model.clone()),
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
}
