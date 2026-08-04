//! MCP OAuth 2.1 client flow, per the Model Context Protocol authorization
//! spec: RFC 9728 protected-resource discovery, RFC 8414 authorization-
//! server metadata, RFC 7591 dynamic client registration, PKCE (RFC 7636)
//! authorization-code exchange, and refresh. Scoped to exactly what
//! `Router::authenticate_mcp_server` needs -- not a general-purpose OAuth
//! client library, and not a new crate dependency beyond `sha2` (PKCE's
//! S256 challenge needs a hash function no other dependency here already
//! exposes; everything else -- HTTP, random bytes, JSON, URL parsing --
//! reuses `reqwest`/`rand`/`serde_json` already in this crate).
//!
//! The redirect-capture side (`start_loopback_listener`) is a minimal
//! hand-rolled single-request HTTP listener rather than pulling in `axum`
//! (which `acpx-core` deliberately does not depend on -- only
//! `acpx-server` does, for its own request-serving transport) or a
//! separate HTTP server crate for one GET request.

use rand::RngCore;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("url parse failed: {0}")]
    UrlParse(String),
    #[error("no OAuth authorization server metadata found for {0}")]
    NoAuthServerMetadata(String),
    #[error("dynamic client registration failed: {0}")]
    RegistrationFailed(String),
    #[error("token exchange failed: {0}")]
    TokenExchangeFailed(String),
    #[error("oauth loopback callback listener failed: {0}")]
    CallbackListener(String),
    #[error("authorization was not completed before the timeout")]
    Timeout,
    #[error("oauth callback state parameter did not match (possible CSRF)")]
    StateMismatch,
}

/// RFC 8414 authorization server metadata (subset actually used).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
}

fn origin_of(url: &str) -> Result<String, OAuthError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| OAuthError::UrlParse(e.to_string()))?;
    Ok(format!("{}://{}", parsed.scheme(), parsed.authority()))
}

/// Discover the authorization server metadata for an MCP HTTP server at
/// `server_url`. Per the MCP spec: first try RFC 9728 protected-resource
/// metadata (`<origin>/.well-known/oauth-protected-resource`), which -- if
/// present -- names the authorization server(s) that protect this
/// resource; fall back to treating the resource's own origin as the
/// authorization server directly. Either way, the final step is always an
/// RFC 8414 fetch (`<auth-server-origin>/.well-known/oauth-authorization-
/// server`).
pub async fn discover(
    client: &reqwest::Client,
    server_url: &str,
) -> Result<AuthServerMetadata, OAuthError> {
    let origin = origin_of(server_url)?;

    let prm_url = format!("{origin}/.well-known/oauth-protected-resource");
    let auth_server_origin = match client.get(&prm_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let prm = resp.json::<ProtectedResourceMetadata>().await.ok();
            prm.and_then(|prm| prm.authorization_servers.into_iter().next())
                .unwrap_or_else(|| origin.clone())
        }
        _ => origin.clone(),
    };

    let asm_url = format!("{auth_server_origin}/.well-known/oauth-authorization-server");
    let resp = client
        .get(&asm_url)
        .send()
        .await
        .map_err(OAuthError::Http)?;
    if !resp.status().is_success() {
        return Err(OAuthError::NoAuthServerMetadata(server_url.to_string()));
    }
    resp.json::<AuthServerMetadata>()
        .await
        .map_err(|e| OAuthError::NoAuthServerMetadata(e.to_string()))
}

#[derive(Debug, Clone)]
pub struct ClientRegistration {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// RFC 7591 dynamic client registration. Requests `token_endpoint_auth_
/// method: "none"` (public client + PKCE, no client secret) since acpx
/// has no durable place to keep a confidential client secret scoped to
/// one MCP server beyond the same encrypted `oauth_tokens` row the access/
/// refresh tokens already use -- DCR-issued public clients avoid needing
/// one at all, matching the MCP spec's own recommendation for native/CLI
/// clients.
pub async fn register_client(
    client: &reqwest::Client,
    metadata: &AuthServerMetadata,
    redirect_uri: &str,
) -> Result<ClientRegistration, OAuthError> {
    let Some(registration_endpoint) = &metadata.registration_endpoint else {
        return Err(OAuthError::RegistrationFailed(
            "authorization server has no registration_endpoint (dynamic client registration \
             unsupported here; configure oauth.client_id on this MCP server manually instead)"
                .to_string(),
        ));
    };
    let body = serde_json::json!({
        "client_name": "acpx",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(OAuthError::Http)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(OAuthError::RegistrationFailed(format!("{status}: {text}")));
    }
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| OAuthError::RegistrationFailed(e.to_string()))?;
    let client_id = value
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            OAuthError::RegistrationFailed("registration response missing client_id".to_string())
        })?
        .to_string();
    let client_secret = value
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(ClientRegistration {
        client_id,
        client_secret,
    })
}

/// A PKCE (RFC 7636) verifier/challenge pair. `verifier` must be kept by
/// the caller (never sent in the authorization request) and presented
/// again at token-exchange time; `challenge` is the S256 digest of it,
/// sent up front in the authorization URL.
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Base64url (no padding) encoding -- RFC 4648 section 5. Hand-rolled
/// rather than a new `base64` dependency, matching `keystore.rs`'s own
/// `hex_encode`/`hex_decode` precedent for "one small internal use isn't
/// worth a new crate."
fn base64_url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

pub fn generate_pkce_pair() -> PkcePair {
    let mut verifier_bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut verifier_bytes);
    let verifier = base64_url_encode(&verifier_bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64_url_encode(&hasher.finalize());
    PkcePair {
        verifier,
        challenge,
    }
}

/// Builds the `GET <authorization_endpoint>?...` URL the user's browser
/// is sent to, with the S256 PKCE challenge and CSRF `state` attached.
pub fn build_authorization_url(
    metadata: &AuthServerMetadata,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    pkce: &PkcePair,
) -> Result<String, OAuthError> {
    let mut url = reqwest::Url::parse(&metadata.authorization_endpoint)
        .map_err(|e| OAuthError::UrlParse(e.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
    }
    Ok(url.to_string())
}

/// Access/refresh token pair returned by the token endpoint. `obtained_at_
/// unix` is stamped locally (not part of the wire response) so
/// `is_expired`/refresh scheduling has a fixed reference point independent
/// of clock skew between this process and the authorization server.
/// `client_id`/`client_secret` are *not* part of the wire response either
/// -- stamped on by `exchange_code`/`refresh` after the fact -- but must
/// travel with the tokens (serialized into the same encrypted
/// `oauth_tokens` row) so a later refresh can present the *same* client
/// identity that originally obtained them, which most authorization
/// servers require for a `refresh_token` grant.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub obtained_at_unix: u64,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl OAuthTokens {
    /// True once within 30 seconds of `expires_in` -- the same refresh-
    /// ahead buffer convention used by comparable clients (e.g. Zed's own
    /// MCP OAuth token provider) so a token doesn't expire mid-request.
    pub fn needs_refresh(&self) -> bool {
        match self.expires_in {
            Some(expires_in) => {
                let now = now_unix();
                let expiry = self.obtained_at_unix.saturating_add(expires_in);
                now.saturating_add(30) >= expiry
            }
            None => false,
        }
    }
}

/// In-memory cache of live OAuth tokens, keyed by MCP server name.
/// Cheaply `Clone` (`Arc<Mutex<..>>` internally) so `Router` and a
/// detached OAuth-completion background task can share one handle -- see
/// `Router::oauth_tokens`'s doc comment for why that matters.
#[derive(Debug, Default, Clone)]
pub struct OAuthTokenCache {
    tokens: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, OAuthTokens>>>,
}

impl OAuthTokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, server_name: &str) -> Option<OAuthTokens> {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(server_name)
            .cloned()
    }

    pub fn insert(&self, server_name: impl Into<String>, tokens: OAuthTokens) {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(server_name.into(), tokens);
    }

    pub fn remove(&self, server_name: &str) {
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(server_name);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn post_token_request(
    client: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<OAuthTokens, OAuthError> {
    let resp = client
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .map_err(OAuthError::Http)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchangeFailed(format!("{status}: {text}")));
    }
    let mut tokens: OAuthTokens = resp
        .json()
        .await
        .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
    tokens.obtained_at_unix = now_unix();
    Ok(tokens)
}

#[allow(clippy::too_many_arguments)]
pub async fn exchange_code(
    client: &reqwest::Client,
    metadata: &AuthServerMetadata,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    pkce_verifier: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", pkce_verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let mut tokens = post_token_request(client, &metadata.token_endpoint, &form).await?;
    tokens.client_id = client_id.to_string();
    tokens.client_secret = client_secret.map(str::to_string);
    Ok(tokens)
}

/// Exchanges a refresh token for a fresh access token, presenting the
/// same `client_id`/`client_secret` the original `exchange_code` used
/// (see `OAuthTokens`'s doc comment for why that identity travels with
/// the tokens). Per RFC 6749 §6, a server MAY omit `refresh_token` from
/// the response to mean "the existing one is still valid" rather than
/// issuing a new one -- if that happens, the caller's `refresh_token` is
/// carried forward onto the result rather than being silently dropped
/// (which would otherwise strand the caller with no refresh token at all
/// on the very next refresh attempt).
pub async fn refresh(
    client: &reqwest::Client,
    metadata: &AuthServerMetadata,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let mut tokens = post_token_request(client, &metadata.token_endpoint, &form).await?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    tokens.client_id = client_id.to_string();
    tokens.client_secret = client_secret.map(str::to_string);
    Ok(tokens)
}

/// The `code`/`state` query parameters captured off the OAuth redirect.
pub struct LoopbackCallback {
    pub code: String,
    pub state: String,
}

/// Binds an ephemeral `127.0.0.1:0` TCP listener and returns its
/// `redirect_uri` (`http://127.0.0.1:<port>/callback`) immediately, plus a
/// `JoinHandle` that resolves once exactly one HTTP request has been
/// accepted and parsed (or the 5-minute timeout elapses). Callers pass the
/// returned `redirect_uri` into `register_client`/`build_authorization_
/// url` so the authorization server's redirect actually lands here, then
/// `.await` the handle separately (typically from a detached task) once
/// they've already returned the authorization URL to the caller that
/// needs to open a browser.
pub async fn start_loopback_listener() -> Result<
    (
        String,
        tokio::task::JoinHandle<Result<LoopbackCallback, OAuthError>>,
    ),
    OAuthError,
> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OAuthError::CallbackListener(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| OAuthError::CallbackListener(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let handle = tokio::spawn(async move {
        let accepted = tokio::time::timeout(Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| OAuthError::Timeout)?
            .map_err(|e| OAuthError::CallbackListener(e.to_string()))?;
        let (mut stream, _) = accepted;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 8192];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| OAuthError::CallbackListener(e.to_string()))?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let request_line = request.lines().next().unwrap_or("");
        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

        let mut code = None;
        let mut state = None;
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                let value = percent_decode(value);
                match key {
                    "code" => code = Some(value),
                    "state" => state = Some(value),
                    _ => {}
                }
            }
        }

        let body = "<html><body>Authentication complete. You may close this window.</body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        match (code, state) {
            (Some(code), Some(state)) => Ok(LoopbackCallback { code, state }),
            _ => Err(OAuthError::CallbackListener(
                "redirect had no code/state query params".to_string(),
            )),
        }
    });

    Ok((redirect_uri, handle))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compares a callback's `state` against the one the flow began with,
/// returning [`OAuthError::StateMismatch`] on any difference -- a
/// dedicated helper so callers can't accidentally skip this check (it is
/// the entire CSRF defense for this flow).
pub fn verify_state(expected: &str, actual: &str) -> Result<(), OAuthError> {
    if expected == actual {
        Ok(())
    } else {
        Err(OAuthError::StateMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier_base64url() {
        let pair = generate_pkce_pair();
        let mut hasher = Sha256::new();
        hasher.update(pair.verifier.as_bytes());
        let expected = base64_url_encode(&hasher.finalize());
        assert_eq!(pair.challenge, expected);
        assert!(!pair.verifier.contains('+'));
        assert!(!pair.verifier.contains('/'));
        assert!(!pair.verifier.contains('='));
    }

    #[test]
    fn base64_url_encode_matches_known_vector() {
        // RFC 4648 test vector ("f", "fo", "foo") adapted to base64url.
        assert_eq!(base64_url_encode(b"f"), "Zg");
        assert_eq!(base64_url_encode(b"fo"), "Zm8");
        assert_eq!(base64_url_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn build_authorization_url_includes_pkce_and_state() {
        let metadata = AuthServerMetadata {
            authorization_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            registration_endpoint: None,
        };
        let pkce = generate_pkce_pair();
        let url = build_authorization_url(
            &metadata,
            "client-123",
            "http://127.0.0.1:9999/callback",
            "state-abc",
            &pkce,
        )
        .unwrap();
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("state=state-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
    }

    #[test]
    fn verify_state_rejects_mismatch() {
        assert!(verify_state("a", "a").is_ok());
        assert!(matches!(
            verify_state("a", "b"),
            Err(OAuthError::StateMismatch)
        ));
    }

    #[test]
    fn origin_of_strips_path_and_query() {
        assert_eq!(
            origin_of("https://example.com/mcp?x=1").unwrap(),
            "https://example.com"
        );
    }

    #[tokio::test]
    async fn loopback_listener_captures_code_and_state() {
        let (redirect_uri, handle) = start_loopback_listener().await.unwrap();
        let url = reqwest::Url::parse(&redirect_uri).unwrap();
        let addr = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());

        let client_task = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            stream
                .write_all(
                    b"GET /callback?code=abc123&state=xyz789 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let callback = handle.await.unwrap().unwrap();
        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "xyz789");
        client_task.await.unwrap();
    }
}
