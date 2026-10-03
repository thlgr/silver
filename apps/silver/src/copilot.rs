//! GitHub Copilot transport. Copilot rejects a GitHub token: `GET /copilot_internal/v2/token` with
//! `Authorization: token <gh_token>` mints a ~30 minute bearer and, for Enterprise or proxied
//! accounts, its API host. The integrator allowlist keys off the editor identifiers and headers.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{Model, ModelRequest, ModelStream};
use tokio_util::sync::CancellationToken;

use crate::provider::OpenAiCompatibleProvider;
use silver_core::redact::redact;

/// Endpoint that exchanges a GitHub token for a Copilot API token.
const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Editor identifier the exchange and the API expect (matches VS Code / Copilot CLI).
const EDITOR_VERSION: &str = "vscode/1.104.1";

/// User agent used for the exchange request.
const EXCHANGE_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";

/// Integration id that puts the request on the chat allowlist.
const INTEGRATION_ID: &str = "vscode-chat";

/// Refresh the minted token this long before it expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// Lifetime assumed when the exchange response omits `expires_at`.
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(1800);

/// Timeout for the exchange request itself.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);

/// A minted Copilot token and what it is valid for.
#[derive(Clone)]
struct MintedToken {
    token: String,
    /// Unix seconds at which the token stops being accepted.
    expires_at: u64,
    /// Account-specific API host, when the exchange named one.
    base_url: Option<String>,
}

impl MintedToken {
    /// Whether the token is still usable [REFRESH_MARGIN] from now.
    fn fresh(&self, now: u64) -> bool {
        self.expires_at > now.saturating_add(REFRESH_MARGIN.as_secs())
    }
}

/// Streaming client for GitHub Copilot's OpenAI-compatible chat endpoint.
pub struct CopilotProvider {
    github_token: String,
    default_base_url: String,
    model: String,
    http: reqwest::Client,
    minted: Arc<Mutex<Option<MintedToken>>>,
}

impl CopilotProvider {
    /// Create a transport for a GitHub token (a PAT, `gh auth token`, or `GITHUB_TOKEN`).
    pub fn new(
        base_url: impl Into<String>,
        github_token: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let base_url = if base_url.trim().is_empty() {
            "https://api.githubcopilot.com".to_string()
        } else {
            base_url.trim().trim_end_matches('/').to_string()
        };
        Self {
            github_token: github_token.into(),
            default_base_url: base_url,
            model: model.into(),
            http: reqwest::Client::new(),
            minted: Arc::new(Mutex::new(None)),
        }
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// The headers every Copilot request carries.
    pub fn request_headers() -> Vec<(String, String)> {
        vec![
            ("Editor-Version".to_string(), EDITOR_VERSION.to_string()),
            (
                "Copilot-Integration-Id".to_string(),
                INTEGRATION_ID.to_string(),
            ),
            (
                "Openai-Intent".to_string(),
                "conversation-edits".to_string(),
            ),
            // Every silver turn is an agent turn; Copilot meters user and agent traffic apart.
            ("x-initiator".to_string(), "agent".to_string()),
        ]
    }

    /// A usable bearer and its host: the cached token while fresh, else a new exchange. A failed
    /// exchange falls back to the raw GitHub token against the default host, which is what accounts
    /// that need no exchange use.
    pub async fn credentials(&self) -> (String, String) {
        if let Some(minted) = self.cached() {
            return (
                minted.token,
                minted
                    .base_url
                    .unwrap_or_else(|| String::clone(&self.default_base_url)),
            );
        }
        match self.exchange().await {
            Ok(minted) => {
                let base_url = Option::clone(&minted.base_url)
                    .unwrap_or_else(|| String::clone(&self.default_base_url));
                let token = String::clone(&minted.token);
                if let Ok(mut slot) = self.minted.lock() {
                    *slot = Some(minted);
                }
                (token, base_url)
            }
            Err(error) => {
                tracing::warn!(
                    error = %redact(&error),
                    "Copilot token exchange failed; using the GitHub token directly"
                );
                (
                    String::clone(&self.github_token),
                    String::clone(&self.default_base_url),
                )
            }
        }
    }

    /// The cached token, when one is still fresh.
    fn cached(&self) -> Option<MintedToken> {
        let slot = self.minted.lock().ok()?;
        slot.as_ref()
            .filter(|minted| minted.fresh(now_unix()))
            .cloned()
    }

    /// Run the exchange once.
    async fn exchange(&self) -> Result<MintedToken, String> {
        if self.github_token.trim().is_empty() {
            return Err("no GitHub token configured".to_string());
        }
        let response = self
            .http
            .get(TOKEN_EXCHANGE_URL)
            .timeout(EXCHANGE_TIMEOUT)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", self.github_token),
            )
            .header(reqwest::header::USER_AGENT, EXCHANGE_USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .header("Editor-Version", EDITOR_VERSION)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!("exchange returned HTTP {}", status.as_u16()));
        }
        parse_exchange(&body)
    }
}

/// Parse an exchange response into a minted token.
fn parse_exchange(body: &str) -> Result<MintedToken, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "exchange returned no token".to_string())?;
    let expires_at = value
        .get("expires_at")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| now_unix() + DEFAULT_TOKEN_TTL.as_secs());
    // `endpoints.api` is authoritative for Enterprise and proxied accounts; otherwise the
    // token's own `proxy-ep` field names the proxy host, whose API twin serves the API.
    let base_url = value
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(Value::as_str)
        .map(|api| api.trim().trim_end_matches('/').to_string())
        .filter(|api| !api.is_empty())
        .or_else(|| base_url_from_proxy_ep(token));
    Ok(MintedToken {
        token: token.to_string(),
        expires_at,
        base_url,
    })
}

/// The API host encoded in a Copilot token's `proxy-ep=proxy.<host>` field.
fn base_url_from_proxy_ep(token: &str) -> Option<String> {
    let field = token
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("proxy-ep="))?;
    let host = field
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    let host = match host.strip_prefix("proxy.") {
        Some(rest) => format!("api.{rest}"),
        None => host.to_string(),
    };
    Some(format!("https://{host}"))
}

/// Unix seconds now, saturating at the epoch if the clock is before it.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl Model for CopilotProvider {
    fn name(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let (token, base_url) = self.credentials().await;
        if token.trim().is_empty() {
            return Err(CoreError::ProviderUnavailable(
                "GitHub Copilot needs a GitHub token; sign in with /login copilot".to_string(),
            ));
        }
        OpenAiCompatibleProvider::new(base_url, token, String::clone(&self.model))
            .with_http_client(reqwest::Client::clone(&self.http))
            .with_extra_headers(Self::request_headers())
            .stream(request, cancel)
            .await
    }
}
