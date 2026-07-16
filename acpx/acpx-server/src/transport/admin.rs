//! Loopback-only HTTP administration surface for agent enablement and
//! custom ACP-agent definitions. It never shares the client-plane token.

use std::sync::Arc;

use acpx_core::{
    detect, AdminError, AdminOps, AgentEnablement, CustomAgent, CustomAgentStore, PersistenceStore,
};
use acpx_proto::admin::CustomAgentSpec;
use acpx_proto::agent::{AgentListEntry, AgentSource, AgentStatus};
use acpx_registry::Registry;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Json;

const CUSTOM_AGENT_VERSION: &str = "custom";

#[derive(Clone)]
pub struct AdminAuthConfig {
    token: Arc<str>,
}

impl AdminAuthConfig {
    pub fn new(token: String) -> Self {
        Self {
            token: token.into(),
        }
    }

    fn authorize(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
            return false;
        };
        let Ok(value) = value.to_str() else {
            return false;
        };
        let Some(presented) = value.strip_prefix("Bearer ") else {
            return false;
        };
        tokens_match(presented, &self.token)
    }
}

fn tokens_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .iter()
        .zip(expected)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[derive(Clone)]
struct AdminState {
    auth: AdminAuthConfig,
    ops: AdminOps,
    registry: Registry,
    enablement: AgentEnablement,
}

pub fn build_ops(store: PersistenceStore, registry: &Registry) -> AdminOps {
    AdminOps::new(
        AgentEnablement::new(store.clone()),
        CustomAgentStore::new(store),
        registry.agents.iter().map(|agent| agent.id.clone()),
    )
}

pub async fn serve_on(
    listener: tokio::net::TcpListener,
    admin_token: String,
    store: PersistenceStore,
    registry: Registry,
) -> anyhow::Result<()> {
    let bind_addr = listener.local_addr()?;
    if !bind_addr.ip().is_loopback() {
        anyhow::bail!("admin transport must only bind a loopback address, got {bind_addr}");
    }
    let enablement = AgentEnablement::new(store.clone());
    let state = AdminState {
        auth: AdminAuthConfig::new(admin_token),
        ops: build_ops(store, &registry),
        registry,
        enablement,
    };
    let app = axum::Router::new()
        .route("/admin/agents", get(list_agents))
        .route("/admin/agents/:id/enable", post(enable_agent))
        .route("/admin/agents/:id/disable", post(disable_agent))
        .route(
            "/admin/agents/custom",
            get(list_custom_agents).post(create_custom_agent),
        )
        .route("/admin/agents/custom/:id", delete(delete_custom_agent))
        .with_state(state);
    tracing::info!(
        bind_addr = %bind_addr,
        "acpx-server admin transport listening on loopback"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn list_agents(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut agents = Vec::with_capacity(state.registry.agents.len());
    for agent in &state.registry.agents {
        let enabled = match state.enablement.is_enabled(&agent.id).await {
            Ok(enabled) => enabled,
            Err(error) => return persistence_error(error),
        };
        agents.push(AgentListEntry {
            id: agent.id.clone(),
            name: agent.name.clone(),
            version: agent.version.clone(),
            status: detect::detect(&agent.id, &agent.distribution),
            enabled,
            source: AgentSource::Registry,
        });
    }
    match state.ops.list_custom_agents().await {
        Ok(custom_agents) => {
            for agent in custom_agents {
                let enabled = match state.enablement.is_enabled(&agent.id).await {
                    Ok(enabled) => enabled,
                    Err(error) => return persistence_error(error),
                };
                agents.push(AgentListEntry {
                    id: agent.id,
                    name: agent.name,
                    version: CUSTOM_AGENT_VERSION.to_owned(),
                    status: AgentStatus::NotInstalled,
                    enabled,
                    source: AgentSource::Custom,
                });
            }
            Json(serde_json::json!({ "agents": agents })).into_response()
        }
        Err(error) => admin_error(error),
    }
}

async fn enable_agent(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    set_enabled(state, headers, id, true).await
}

async fn disable_agent(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    set_enabled(state, headers, id, false).await
}

async fn set_enabled(state: AdminState, headers: HeaderMap, id: String, enabled: bool) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.ops.set_enabled(id.clone(), enabled).await {
        Ok(()) => Json(serde_json::json!({ "id": id, "enabled": enabled })).into_response(),
        Err(error) => admin_error(error),
    }
}

async fn create_custom_agent(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(spec): Json<CustomAgentSpec>,
) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let agent = CustomAgent {
        id: spec.id,
        name: spec.name,
        command: spec.command,
        args: spec.args,
        env: spec.env,
        cwd: spec.cwd,
    };
    match state.ops.create_custom_agent(agent.clone()).await {
        Ok(()) => (StatusCode::CREATED, Json(custom_agent_spec(agent))).into_response(),
        Err(error) => admin_error(error),
    }
}

async fn list_custom_agents(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.ops.list_custom_agents().await {
        Ok(agents) => Json(
            agents
                .into_iter()
                .map(custom_agent_spec)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => admin_error(error),
    }
}

async fn delete_custom_agent(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.ops.delete_custom_agent(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => admin_error(error),
    }
}

fn custom_agent_spec(agent: CustomAgent) -> CustomAgentSpec {
    CustomAgentSpec {
        id: agent.id,
        name: agent.name,
        command: agent.command,
        args: agent.args,
        env: agent.env,
        cwd: agent.cwd,
    }
}

fn persistence_error(error: acpx_core::PersistenceError) -> Response {
    tracing::error!(%error, "admin persistence read failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn admin_error(error: AdminError) -> Response {
    let status = match &error {
        AdminError::UnknownAgent(_)
        | AdminError::CustomAgent(acpx_core::CustomAgentStoreError::NotFound(_)) => {
            StatusCode::NOT_FOUND
        }
        AdminError::RegistryIdCollision(_)
        | AdminError::CustomAgent(acpx_core::CustomAgentStoreError::AlreadyExists(_)) => {
            StatusCode::CONFLICT
        }
        AdminError::InvalidCustomAgent { .. } => StatusCode::BAD_REQUEST,
        AdminError::CustomAgent(acpx_core::CustomAgentStoreError::Persistence(_))
        | AdminError::Persistence(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}
