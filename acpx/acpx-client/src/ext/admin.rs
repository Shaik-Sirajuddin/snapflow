//! HTTP-only client for ACPX's loopback administration plane.
//!
//! This deliberately does not reuse [`crate::raw::GatewayClient`]: admin
//! routes are not JSON-RPC, use `ACPX_ADMIN_TOKEN` rather than
//! `ACPX_AUTH_TOKEN`, and must remain isolated from the client-facing ACP
//! transport and `ext::registry`.

use acpx_proto::admin::CustomAgentSpec;

/// Result returned by the enable/disable administration endpoints.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct AgentEnablement {
    pub id: String,
    pub enabled: bool,
}

/// Result returned by `POST /admin/sessions/close-all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct SessionsCloseAllReport {
    pub closed: u64,
    pub failed: u64,
    pub skipped: u64,
}

/// Failures returned by the HTTP-only administration plane.
#[derive(Debug, thiserror::Error)]
pub enum AdminClientError {
    #[error("HTTP request to ACPX admin plane failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ACPX admin plane returned HTTP {status}: {message}")]
    Response { status: u16, message: String },
}

/// HTTP client for `/admin/*`, authenticated only with `ACPX_ADMIN_TOKEN`.
///
/// `base_url` is the admin listener origin, for example
/// `http://127.0.0.1:8791`; it is intentionally independent from a
/// [`crate::raw::GatewayClient`] origin and token.
pub struct AdminClient {
    http: reqwest::Client,
    base_url: String,
    admin_token: String,
}

impl AdminClient {
    pub fn new(base_url: impl Into<String>, admin_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            admin_token: admin_token.into(),
        }
    }

    /// setup-followups plan, agent_settings_ordering_and_install_enable_
    /// flow: the read side of enable/disable -- every registry + custom
    /// agent with its current `enabled` flag (and status/source), so a
    /// client can render an accurate toggle instead of a blind one.
    pub async fn list_agents(
        &self,
    ) -> Result<Vec<acpx_proto::agent::AgentListEntry>, AdminClientError> {
        #[derive(serde::Deserialize)]
        struct ListAgentsResponse {
            agents: Vec<acpx_proto::agent::AgentListEntry>,
        }
        let response = self
            .http
            .get(self.endpoint("admin/agents"))
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        let parsed: ListAgentsResponse = self.json(response).await?;
        Ok(parsed.agents)
    }

    /// Enable one registry or custom agent.
    pub async fn enable_agent(&self, agent_id: &str) -> Result<AgentEnablement, AdminClientError> {
        self.set_enabled(agent_id, true).await
    }

    /// Disable one registry or custom agent.
    pub async fn disable_agent(&self, agent_id: &str) -> Result<AgentEnablement, AdminClientError> {
        self.set_enabled(agent_id, false).await
    }

    /// Create a durable custom-agent definition.
    pub async fn create_custom_agent(
        &self,
        agent: &CustomAgentSpec,
    ) -> Result<CustomAgentSpec, AdminClientError> {
        let response = self
            .http
            .post(self.endpoint("admin/agents/custom"))
            .bearer_auth(&self.admin_token)
            .json(agent)
            .send()
            .await?;
        self.json(response).await
    }

    /// List durable custom-agent definitions.
    pub async fn list_custom_agents(&self) -> Result<Vec<CustomAgentSpec>, AdminClientError> {
        let response = self
            .http
            .get(self.endpoint("admin/agents/custom"))
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        self.json(response).await
    }

    /// Delete a durable custom-agent definition.
    pub async fn delete_custom_agent(&self, agent_id: &str) -> Result<(), AdminClientError> {
        let response = self
            .http
            .delete(self.endpoint(&format!("admin/agents/custom/{agent_id}")))
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        self.empty(response).await
    }

    /// **`e2e_session_teardown_automation`.** Live count of sessions
    /// currently registered for one tenant (default: `"default"`) --
    /// what an e2e teardown check should assert against, not just
    /// trusting each individual `close_all_sessions` response.
    pub async fn session_count(&self, tenant: &str) -> Result<u64, AdminClientError> {
        #[derive(serde::Deserialize)]
        struct SessionsCountResponse {
            count: u64,
        }
        let response = self
            .http
            .get(self.endpoint(&format!("admin/sessions/count?tenant={tenant}")))
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        let parsed: SessionsCountResponse = self.json(response).await?;
        Ok(parsed.count)
    }

    /// **`e2e_session_teardown_automation`.** Closes every unpinned,
    /// not-in-flight session for one tenant right now, regardless of
    /// idle time -- the e2e/dev-test teardown call: after a test run is
    /// done, close everything it opened instead of leaving it for the
    /// (deliberately unchanged, 30-minute) idle-TTL reaper.
    pub async fn close_all_sessions(
        &self,
        tenant: &str,
    ) -> Result<SessionsCloseAllReport, AdminClientError> {
        let response = self
            .http
            .post(self.endpoint(&format!("admin/sessions/close-all?tenant={tenant}")))
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        self.json(response).await
    }

    async fn set_enabled(
        &self,
        agent_id: &str,
        enabled: bool,
    ) -> Result<AgentEnablement, AdminClientError> {
        let action = if enabled { "enable" } else { "disable" };
        let response = self
            .http
            .post(self.agent_endpoint(agent_id, action))
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        self.json(response).await
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    fn agent_endpoint(&self, agent_id: &str, action: &str) -> String {
        // Server-side ids are constrained to URL-path-safe ASCII for custom
        // agents; registry ids use the same gateway identifier contract.
        self.endpoint(&format!("admin/agents/{agent_id}/{action}"))
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, AdminClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(response_error(response).await);
        }
        Ok(response.json().await?)
    }

    async fn empty(&self, response: reqwest::Response) -> Result<(), AdminClientError> {
        if response.status().is_success() {
            return Ok(());
        }
        Err(response_error(response).await)
    }
}

async fn response_error(response: reqwest::Response) -> AdminClientError {
    let status = response.status().as_u16();
    let message = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| {
            body.get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "request rejected".to_owned());
    AdminClientError::Response { status, message }
}
