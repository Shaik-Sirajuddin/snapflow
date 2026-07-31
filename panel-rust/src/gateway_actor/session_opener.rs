//! `acpx_client::pool::SessionOpener` implementation backed by a real
//! `Gateway`.
//!
//! Bridges `acpx_client::pool::ProjectSessionPool`'s transport-agnostic
//! `resume`/`create` calls onto the same `session/resume`/`session/new`
//! wire shapes `thread_actor.rs`'s `Command::OpenSession`/
//! `Command::ReattachSession` handlers already use. Deliberately thin: no
//! retry loop, no replay registration, no live-notification draining --
//! those stay the actor's own responsibility (see this module's doc
//! comment on `GatewaySessionOpener` for why), so a pool-driven open here
//! is exactly one `Gateway::call` attempt, bounded by
//! `acpx_client::pool::ACQUIRE_OPEN_TIMEOUT` at the pool layer.
//!
//! `PoolKey::provider_profile` is `"{profile}"` with an empty string
//! standing for "no profile" (a `PoolKey` field can't itself be
//! `Option<String>` since it must be `Eq + Hash` as a plain compatibility
//! key) -- see [`provider_profile_key`]/[`profile_from_key`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use acpx_client::pool::{OpenError, PoolKey, SessionOpener};
use acpx_client::raw::ClientError;
use acpx_client::Gateway;

/// Classifies a `Gateway::call` failure into the pool's terminal/retryable
/// distinction, reusing `ClientError::is_authentication_or_capacity` --
/// the same classifier `thread_actor.rs`'s own retry loops already trust
/// (via `AcpxThreadError::is_authentication_or_capacity`).
fn classify(err: ClientError) -> OpenError {
    if err.is_authentication_or_capacity() {
        OpenError::terminal(err.to_string())
    } else {
        OpenError::retryable(err.to_string())
    }
}

/// `PoolKey::provider_profile`'s sentinel for "no profile selected" --
/// `PoolKey` fields must be plain `String` (the key needs `Eq + Hash` as a
/// whole), so `Option<String>` can't be threaded through directly.
pub const NO_PROFILE_SENTINEL: &str = "__default__";

pub fn provider_profile_key(profile: Option<&str>) -> String {
    profile.unwrap_or(NO_PROFILE_SENTINEL).to_string()
}

fn profile_from_key(key: &PoolKey) -> Option<&str> {
    if key.provider_profile == NO_PROFILE_SENTINEL {
        None
    } else {
        Some(key.provider_profile.as_str())
    }
}

/// finding-b-real-attach-agent-id-plumbing: `key.agent_id` already carries
/// the thread's own explicitly-selected provider (e.g. `"claude-acp"`) --
/// but until now nothing ever forwarded it to `session/new`/`session/
/// resume` when no real profile also names that agent, so a real attach
/// silently fell through to the gateway's single `ACPX_DEFAULT_AGENT_ID`
/// regardless of what the compose bar's Provider picker showed (live-
/// confirmed: a `claude-acp`-selected thread's `session/new` kept failing
/// with the *codex* backend's own auth error). `session/new` supports a
/// real, direct `_acpx.agentId` selector for exactly this (see acpx-core's
/// `router.rs`, `dispatch_session_new`'s `_acpx.agentId`/`_acpx.profile`
/// precedence) -- distinct from `_acpx.profile` and mutually exclusive
/// with it (acpx-core rejects a request naming both). Returns `None` for
/// `NO_PROVIDER_REQUESTED_FALLBACK` ("codex"), which must keep meaning
/// "no explicit selection, use the server's own default" exactly as
/// before -- only a real, specific agent id should ever override that.
fn explicit_agent_id_from_key(key: &PoolKey) -> Option<&str> {
    if key.agent_id.is_empty()
        || key.agent_id == crate::agent_bridge::NO_PROVIDER_REQUESTED_FALLBACK
    {
        None
    } else {
        Some(key.agent_id.as_str())
    }
}

/// Embeds `_acpx.agentId` into `params` when the caller resolved an
/// explicit agent selection for this key AND no real profile already
/// claims that role (acpx-core rejects a request naming both `_acpx.
/// profile` and `_acpx.agentId`; a real profile is always the more
/// specific selection when both could apply). Returns the profile to
/// pass through `Gateway::call`'s own `profile` parameter unchanged --
/// `None` whenever this embedded an explicit agentId instead, since
/// `Gateway::call`'s `with_profile` must not also inject `_acpx.profile`
/// into the same params and either violate that exclusivity or silently
/// overwrite the `_acpx` object this just built.
fn apply_explicit_agent_selection<'a>(
    key: &'a PoolKey,
    mut params: serde_json::Value,
) -> (serde_json::Value, Option<&'a str>) {
    let profile = profile_from_key(key);
    if profile.is_some() {
        return (params, profile);
    }
    let Some(agent_id) = explicit_agent_id_from_key(key) else {
        return (params, None);
    };
    if let Some(object) = params.as_object_mut() {
        object.insert(
            "_acpx".to_string(),
            serde_json::json!({ "agentId": agent_id }),
        );
    }
    (params, None)
}

/// Opens sessions for one project against a shared `Gateway`. `key.
/// project_dir` is used verbatim as the ACP `cwd`; `mcp_servers` is project/
/// agent policy, not a compatibility axis for session reuse, so it lives on
/// the opener (one opener per pool, one pool per project+gateway) rather
/// than threaded through `PoolKey`.
///
/// Mutable via [`Self::set_mcp_servers`] rather than fixed at construction:
/// a warm-pooled session's `mcpServers` is fixed forever at whatever
/// `session/new` call created it (ACP has no "update a session's MCP
/// servers" operation), so when the caller's own MCP config source changes
/// (skills directory moved, snapshotd's listener address changed, ...) the
/// only way to make *future* sessions reflect it is to update this opener
/// and drop the pool's now-stale idle entries -- see
/// `ProjectSessionPool::refresh_key`/`refresh_all`, meant to be called
/// immediately after `set_mcp_servers`.
pub struct GatewaySessionOpener {
    gateway: Arc<Gateway>,
    mcp_servers: RwLock<serde_json::Value>,
}

impl GatewaySessionOpener {
    pub fn new(gateway: Arc<Gateway>, mcp_servers: serde_json::Value) -> Self {
        Self {
            gateway,
            mcp_servers: RwLock::new(mcp_servers),
        }
    }

    /// Updates the `mcpServers` array future `create()` calls will send.
    /// Does not touch any already-open session (impossible -- ACP has no
    /// such operation) or the pool's existing idle/leased entries; pair
    /// this with `ProjectSessionPool::refresh_key`/`refresh_all` so stale
    /// idle entries are dropped and a leased one is stamped for drop-on-
    /// release rather than reuse.
    pub fn set_mcp_servers(&self, mcp_servers: serde_json::Value) {
        *self
            .mcp_servers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mcp_servers;
    }

    fn cwd_for(key: &PoolKey) -> PathBuf {
        PathBuf::from(&key.project_dir)
    }
}

impl SessionOpener for GatewaySessionOpener {
    /// One bounded `session/resume` attempt against `saved_session_id`.
    /// `Err` (stale/missing/auth-failed) tells the pool to fall back to
    /// `create` -- this function never retries or falls back on its own,
    /// matching the plan's "authentication and capacity errors are
    /// terminal for that attempt" rule.
    fn resume<'a>(
        &'a self,
        key: &'a PoolKey,
        saved_session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, OpenError>> + Send + 'a>> {
        Box::pin(async move {
            let params = serde_json::json!({
                "sessionId": saved_session_id,
                "cwd": Self::cwd_for(key).to_string_lossy(),
            });
            let (params, profile) = apply_explicit_agent_selection(key, params);
            self.gateway
                .call("session/resume", params, profile)
                .await
                .map_err(classify)?;
            Ok(saved_session_id.to_string())
        })
    }

    /// One bounded `session/new` attempt. Mirrors `Command::OpenSession`'s
    /// params (`cwd`, `mcpServers`) but not its five-attempt retry loop or
    /// `register_session_replay` call -- those belong to the actor that
    /// owns the resulting lease's live notification stream, not to session
    /// *creation* itself.
    fn create<'a>(
        &'a self,
        key: &'a PoolKey,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, Option<serde_json::Value>), OpenError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            // Cloned and the guard dropped before the call below -- a std
            // RwLock guard must never cross an `.await`.
            let mcp_servers = self
                .mcp_servers
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let params = serde_json::json!({
                "cwd": Self::cwd_for(key).to_string_lossy(),
                "mcpServers": mcp_servers,
            });
            let (params, profile) = apply_explicit_agent_selection(key, params);
            let value = self
                .gateway
                .call("session/new", params, profile)
                .await
                .map_err(classify)?;
            let session_id = value
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    OpenError::retryable("session/new response had no sessionId field")
                })?;
            // The full response (including `configOptions`/`sessionModes`)
            // is kept, not discarded -- a freshly pool-created session is
            // never independently `session/resume`d by any other code
            // path, so this is the only place its capabilities can ever be
            // captured. See `SessionLease::capabilities`'s doc comment.
            Ok((session_id, Some(value)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_profile_key_round_trips_through_the_sentinel() {
        assert_eq!(provider_profile_key(Some("codex")), "codex");
        assert_eq!(provider_profile_key(None), NO_PROFILE_SENTINEL);

        let key_with_profile =
            PoolKey::new("/proj", "agent-1", provider_profile_key(Some("codex")));
        assert_eq!(profile_from_key(&key_with_profile), Some("codex"));

        let key_without_profile = PoolKey::new("/proj", "agent-1", provider_profile_key(None));
        assert_eq!(profile_from_key(&key_without_profile), None);
    }
}
