//! OAuth sign-in for provider credentials: RFC 8628 device flow, and PKCE S256 with an RFC 8252
//! loopback callback. Tokens live in the configured oauth_token_store (default
//! `<data_dir>/oauth.json`, 0600, atomic) and only ever render redacted.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::Config;
use silver_core::redact::{redact, REDACTED};

// -- Constants (auth_constants.py, 1:1 unless noted) --------------------------------------------

/// Refresh an access token this many seconds before it actually expires.
pub const ACCESS_TOKEN_REFRESH_SKEW_SECONDS: i64 = 120;

/// RFC 8628 poll interval cap: never poll the device endpoint more often than this.
pub const DEVICE_AUTH_POLL_INTERVAL_CAP_SECONDS: u64 = 1;

/// slow_down grows the poll interval by one second per response, capped here.
pub const DEVICE_AUTH_SLOW_DOWN_CAP_SECONDS: u64 = 30;

/// expires_in fallback when a token response omits it (anthropic_credentials.py).
pub const DEFAULT_TOKEN_TTL_SECONDS: u64 = 3_600;

/// RFC 8628 device-code grant type.
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Prefix shared by every OAuth environment override.
pub const OAUTH_ENV_PREFIX: &str = "SILVER_OAUTH_";

/// Public client id of the Codex CLI, which is what a ChatGPT subscription signs in to.
pub const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Authorization endpoint for the Codex sign-in.
pub const CODEX_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";

/// Token endpoint for the Codex sign-in.
pub const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Scopes the Codex client asks for; offline_access is what yields a refresh token.
pub const CODEX_OAUTH_SCOPE: &str = "openid profile email offline_access";

/// The exact loopback port OpenAI registered for this client.
pub const CODEX_CALLBACK_PORT: u16 = 1455;

/// The exact loopback path OpenAI registered for this client.
pub const CODEX_CALLBACK_PATH: &str = "/auth/callback";

/// Wait this long for the PKCE loopback callback before giving up.
pub const DEFAULT_PKCE_TIMEOUT_SECONDS: u64 = 120;

/// Response bodies in errors are clipped to this many characters (auth_openrouter.py).
const ERROR_BODY_LIMIT: usize = 2_048;

// Nous Portal (auth_constants.py).
pub const DEFAULT_NOUS_PORTAL_URL: &str = "https://portal.nousresearch.com";
const DEFAULT_NOUS_CLIENT_ID: &str = "hermes-cli";
const DEFAULT_NOUS_SCOPE: &str = "inference:invoke";

// OpenRouter PKCE (auth_constants.py).
const OPENROUTER_AUTH_URL: &str = "https://openrouter.ai/auth";
const OPENROUTER_AUTH_KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const DEFAULT_OPENROUTER_KEY_LABEL: &str = "silver";

// -- Public types -------------------------------------------------------------------------------

/// One persisted grant. `expires_at` is Unix seconds; 0 means it never expires (an OpenRouter key).
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    #[serde(default)]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl OAuthToken {
    /// True when the token is absent, already expired, or inside the refresh skew.
    ///
    /// A non-expiring token (expires_at == 0) never needs a refresh.
    pub fn needs_refresh(&self, now_unix: i64) -> bool {
        self.expires_at != 0
            && now_unix
                >= self
                    .expires_at
                    .saturating_sub(ACCESS_TOKEN_REFRESH_SKEW_SECONDS)
    }
}

/// Redacted so a token can never reach a log line through {:?}.
impl fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthToken")
            .field("access_token", &REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("expires_at", &self.expires_at)
            .field("token_type", &self.token_type)
            .finish()
    }
}

/// The on-disk token store, exactly { provider: { access_token, refresh_token, expires_at,
/// token_type } }.
pub struct OAuthStore<'a> {
    path: Cow<'a, Path>,
    tokens: BTreeMap<String, OAuthToken>,
}

impl<'a> OAuthStore<'a> {
    /// Open the store; a missing, unreadable, non-object or corrupt file yields an empty one,
    /// logged without its contents.
    pub fn open(path: impl Into<Cow<'a, Path>>) -> Self {
        let path = path.into();
        let tokens = read_tokens(&path).unwrap_or_default();
        Self { path, tokens }
    }

    /// The resolved backing file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored grant for provider, if any.
    pub fn get(&self, provider: &str) -> Option<OAuthToken> {
        self.tokens.get(provider).cloned()
    }

    /// Insert or replace the grant for provider (not persisted until [OAuthStore::save]).
    pub fn set(&mut self, provider: &str, token: OAuthToken) {
        self.tokens.insert(provider.to_string(), token);
    }

    /// Remove the grant for provider; true when one was present.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.tokens.remove(provider).is_some()
    }

    /// Every provider with a stored grant, in lexical order.
    pub fn list(&self) -> Vec<String> {
        self.tokens.keys().cloned().collect()
    }

    /// Atomically persist the store, 0600 on Unix.
    pub fn save(&self) -> Result<(), OAuthError> {
        let json = serde_json::to_vec_pretty(&self.tokens)?;
        crate::atomic_file::write_private(&self.path, &json).map_err(|source| OAuthError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }
}

impl fmt::Debug for OAuthStore<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthStore")
            .field("path", &self.path)
            .field("providers", &self.list())
            .finish()
    }
}

/// Device-flow payload handed to the caller by [OAuthManager::begin_login].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceLogin {
    /// Code the user enters on the verification page.
    pub user_code: String,
    /// Page the user opens (the verification_uri_complete when the server sends one).
    pub verification_uri: String,
    /// Opaque code [OAuthManager::poll_login] redeems; it is never an access token.
    pub device_code: String,
    /// Server-requested poll interval in seconds.
    pub interval: u64,
    /// Lifetime of the device code in seconds.
    pub expires_in: u64,
}

/// PKCE payload handed to the caller by [OAuthManager::begin_login].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkceLogin {
    /// Browser authorization URL (OpenRouter /auth?...).
    pub authorize_url: String,
    /// The exact loopback redirect URI sent to the authorization server.
    pub redirect_uri: String,
    /// CSRF nonce; also embedded in the callback path.
    pub state: String,
}

/// What [OAuthManager::begin_login] started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginStart {
    /// RFC 8628 device authorization.
    Device(DeviceLogin),
    /// Authorization-code + PKCE (RFC 8252 loopback).
    Pkce(PkceLogin),
}

/// The outcome of one [OAuthManager::poll_login] attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenStatus {
    /// Device flow: the user has not approved yet; poll again after interval seconds.
    Pending { interval: u64 },
    /// Device flow: the server asked us to slow down; the grown interval to honor.
    SlowDown { interval: u64 },
    /// PKCE: the loopback callback has not landed yet.
    Waiting,
    /// Tokens were obtained and persisted.
    Authorized,
    /// The server denied the authorization (or the code was rejected).
    Denied { code: String, description: String },
    /// The device-code / callback window elapsed.
    Expired,
}

/// OAuth flow and token store errors. None renders a token; bodies are clipped and redacted.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// oauth.enabled is false.
    #[error("OAuth is disabled in config")]
    Disabled,
    /// The provider is not in the built-in table.
    #[error("unknown OAuth provider '{0}'")]
    UnknownProvider(String),
    /// A required endpoint is missing and no env override supplies it.
    #[error("provider '{provider}' has no {field} configured; set {env}")]
    MissingEndpoint {
        provider: String,
        field: &'static str,
        env: String,
    },
    /// poll_login was called before begin_login.
    #[error("no OAuth login is in progress for provider '{0}'")]
    NoLoginInProgress(String),
    /// The token file could not be read or written.
    #[error("OAuth token store I/O error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The token file could not be serialized (should not happen).
    #[error("OAuth token store is not valid JSON: {0}")]
    StoreJson(#[from] serde_json::Error),
    /// A transport failure talking to the provider.
    #[error("OAuth HTTP error during {context}: {detail}")]
    Http { context: String, detail: String },
    /// The provider returned an OAuth error object.
    #[error("{provider} returned OAuth error {code}: {description}")]
    Provider {
        provider: String,
        code: String,
        description: String,
        relogin_required: bool,
    },
    /// The device authorization window elapsed before approval.
    #[error("device authorization for provider '{0}' timed out")]
    Timeout(String),
    /// No refresh token is stored, so the access token cannot be renewed.
    #[error("no refresh token stored for provider '{0}'")]
    NoRefreshToken(String),
    /// No grant is stored for the provider.
    #[error("no token stored for provider '{0}'")]
    NoToken(String),
    /// A required JSON field was absent.
    #[error("OAuth response from {provider} is missing '{field}'")]
    MissingField {
        provider: String,
        field: &'static str,
    },
    /// A response body was not JSON.
    #[error("OAuth response from {provider} is not valid JSON")]
    InvalidJson { provider: String },
    /// The callback's state did not match the nonce we generated.
    #[error("OAuth state mismatch; possible CSRF")]
    StateMismatch,
    /// The loopback listener could not bind.
    #[error("could not bind loopback callback listener on 127.0.0.1: {0}")]
    CallbackBind(String),
    /// The loopback callback reported a provider error.
    #[error("OAuth callback error: {0}")]
    Callback(String),
    /// The loopback callback window elapsed.
    #[error("OAuth callback for provider '{0}' timed out")]
    CallbackTimeout(String),
}

// -- Provider table -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Device,
    Pkce,
}

/// Which PKCE dialect a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PkceStyle {
    /// OpenRouter: `callback_url` on the authorize URL, JSON exchange, returns a plain key.
    OpenRouter,
    /// RFC 6749 authorization code with PKCE: `redirect_uri`, form exchange, bearer tokens.
    Standard,
}

struct ProviderSpec {
    id: &'static str,
    flow: Flow,
    client_id: &'static str,
    scopes: &'static str,
    device_url: &'static str,
    token_url: &'static str,
    authorize_url: &'static str,
    /// Nous sends the refresh token in x-nous-refresh-token instead of the form body.
    refresh_uses_header: bool,
    /// Dialect used by the PKCE flow; ignored for device-code providers.
    pkce_style: PkceStyle,
    /// Loopback port the callback must arrive on. Zero picks an ephemeral port; a provider
    /// that registered one exact redirect URI (Codex) needs its port.
    callback_port: u16,
    /// Callback path the provider redirects to. Empty uses a nonce-carrying path.
    callback_path: &'static str,
}

const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "nous",
        flow: Flow::Device,
        client_id: DEFAULT_NOUS_CLIENT_ID,
        scopes: DEFAULT_NOUS_SCOPE,
        device_url: "https://portal.nousresearch.com/api/oauth/device/code",
        token_url: "https://portal.nousresearch.com/api/oauth/token",
        authorize_url: "",
        refresh_uses_header: true,
        pkce_style: PkceStyle::Standard,
        callback_port: 0,
        callback_path: "",
    },
    ProviderSpec {
        id: "generic",
        flow: Flow::Device,
        client_id: "",
        scopes: "",
        device_url: "",
        token_url: "",
        authorize_url: "",
        refresh_uses_header: false,
        pkce_style: PkceStyle::Standard,
        callback_port: 0,
        callback_path: "",
    },
    ProviderSpec {
        id: "openrouter",
        flow: Flow::Pkce,
        client_id: "",
        scopes: "",
        device_url: "",
        token_url: OPENROUTER_AUTH_KEYS_URL,
        authorize_url: OPENROUTER_AUTH_URL,
        refresh_uses_header: false,
        pkce_style: PkceStyle::OpenRouter,
        callback_port: 0,
        callback_path: "",
    },
    ProviderSpec {
        // ChatGPT / Codex subscription sign-in. OpenAI registered one exact redirect URI for
        // this public client, so the loopback listener must own that port and path.
        id: "openai-codex",
        flow: Flow::Pkce,
        client_id: CODEX_OAUTH_CLIENT_ID,
        scopes: CODEX_OAUTH_SCOPE,
        device_url: "",
        token_url: CODEX_OAUTH_TOKEN_URL,
        authorize_url: CODEX_OAUTH_AUTHORIZE_URL,
        refresh_uses_header: false,
        pkce_style: PkceStyle::Standard,
        callback_port: CODEX_CALLBACK_PORT,
        callback_path: CODEX_CALLBACK_PATH,
    },
];

fn provider_spec(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|spec| spec.id == id)
}

/// The SILVER_OAUTH_<PROVIDER> prefix, with every non-alphanumeric byte mapped to _.
pub fn provider_env_prefix(provider: &str) -> String {
    let mapped: String = provider
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{OAUTH_ENV_PREFIX}{mapped}")
}

fn env_override(provider: &str, suffix: &str) -> Option<String> {
    let key = format!("{}_", provider_env_prefix(provider));
    std::env::var(format!("{key}{suffix}"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Clone)]
struct ResolvedProvider {
    id: &'static str,
    flow: Flow,
    client_id: String,
    client_secret: Option<String>,
    scopes: String,
    device_url: Option<String>,
    token_url: Option<String>,
    authorize_url: Option<String>,
    #[expect(dead_code, reason = "reserved for future OAuth flows")]
    redirect_uri: Option<String>,
    refresh_uses_header: bool,
    pkce_style: PkceStyle,
    callback_port: u16,
    callback_path: &'static str,
}

fn nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn resolve_provider(id: &str) -> Option<ResolvedProvider> {
    let spec = provider_spec(id)?;
    Some(ResolvedProvider {
        id: spec.id,
        flow: spec.flow,
        client_id: env_override(id, "CLIENT_ID").unwrap_or_else(|| spec.client_id.to_string()),
        client_secret: env_override(id, "CLIENT_SECRET"),
        scopes: env_override(id, "SCOPE").unwrap_or_else(|| spec.scopes.to_string()),
        device_url: env_override(id, "DEVICE_URL").or_else(|| nonempty(spec.device_url)),
        token_url: env_override(id, "TOKEN_URL").or_else(|| nonempty(spec.token_url)),
        authorize_url: env_override(id, "AUTHORIZE_URL").or_else(|| nonempty(spec.authorize_url)),
        redirect_uri: env_override(id, "REDIRECT_URI"),
        refresh_uses_header: spec.refresh_uses_header,
        pkce_style: spec.pkce_style,
        callback_port: spec.callback_port,
        callback_path: spec.callback_path,
    })
}

fn missing_endpoint(spec: &ResolvedProvider, field: &'static str, suffix: &str) -> OAuthError {
    OAuthError::MissingEndpoint {
        provider: spec.id.to_string(),
        field,
        env: format!("{}_", provider_env_prefix(spec.id)) + suffix,
    }
}

// -- Manager ------------------------------------------------------------------------------------

struct OAuthInner {
    store_path: PathBuf,
    enabled: bool,
    http: reqwest::Client,
    pending: Mutex<HashMap<String, PendingLogin>>,
    store_lock: Mutex<()>,
    refresh_gate: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
enum PendingKind {
    Device {
        device_code: String,
        interval: Duration,
    },
    Pkce {
        code_verifier: String,
        /// Redirect URI the code was issued against; the standard exchange re-sends it.
        redirect_uri: String,
        code: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
        state_ok: bool,
        redirected: bool,
    },
}

#[derive(Clone)]
struct PendingLogin {
    kind: PendingKind,
    deadline: Instant,
}

/// Owns the token store, HTTP client and in-flight logins. The token file is re-read on every call,
/// so a refresh token another process rotated is picked up instead of replayed.
#[derive(Clone)]
pub struct OAuthManager {
    inner: Arc<OAuthInner>,
}

impl fmt::Debug for OAuthManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthManager")
            .field("store_path", &self.inner.store_path)
            .field("enabled", &self.inner.enabled)
            .field("pending", &self.pending_providers())
            .finish()
    }
}

impl OAuthManager {
    /// Build a manager from config. Uses the shared [crate::config::http_client_builder].
    pub fn new(config: &Config) -> Self {
        let http = match crate::config::http_client_builder(&config.security) {
            Ok(builder) => builder
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            Err(_) => reqwest::Client::new(),
        };
        Self {
            inner: Arc::new(OAuthInner {
                store_path: config.oauth_token_store(),
                enabled: config.oauth.enabled,
                http,
                pending: Mutex::new(HashMap::new()),
                store_lock: Mutex::new(()),
                refresh_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The resolved token-store path.
    pub fn store_path(&self) -> &Path {
        &self.inner.store_path
    }

    /// Start a sign-in. A device flow returns [LoginStart::Device] to drive with
    /// [OAuthManager::poll_login]; PKCE binds its loopback listener first, so the redirect URI
    /// names a port already owned.
    pub async fn begin_login(&self, provider: &str) -> Result<LoginStart, OAuthError> {
        if !self.inner.enabled {
            return Err(OAuthError::Disabled);
        }
        let spec = resolve_provider(provider)
            .ok_or_else(|| OAuthError::UnknownProvider(provider.to_string()))?;
        match spec.flow {
            Flow::Device => self.begin_device_login(spec).await,
            Flow::Pkce => self.begin_pkce_login(spec).await,
        }
    }

    /// Advance a login by one step: one token poll for a device flow (the caller waits `interval`
    /// between calls), or a look at the loopback callback for PKCE.
    pub async fn poll_login(&self, provider: &str) -> Result<TokenStatus, OAuthError> {
        if !self.inner.enabled {
            return Err(OAuthError::Disabled);
        }
        let spec = resolve_provider(provider)
            .ok_or_else(|| OAuthError::UnknownProvider(provider.to_string()))?;
        let pending = self
            .pending_snapshot(provider)
            .ok_or_else(|| OAuthError::NoLoginInProgress(provider.to_string()))?;
        match pending.kind {
            PendingKind::Device {
                device_code,
                interval,
            } => {
                if Instant::now() >= pending.deadline {
                    self.clear_pending(provider);
                    return Ok(TokenStatus::Expired);
                }
                self.poll_device_token(&spec, provider, &device_code, interval)
                    .await
            }
            PendingKind::Pkce {
                code_verifier,
                redirect_uri,
                code,
                error,
                error_description,
                state_ok,
                redirected,
                ..
            } => {
                if let Some(code) = error {
                    self.clear_pending(provider);
                    return Ok(TokenStatus::Denied {
                        code,
                        description: error_description.unwrap_or_default(),
                    });
                }
                if redirected {
                    if !state_ok {
                        self.clear_pending(provider);
                        return Err(OAuthError::StateMismatch);
                    }
                    if let Some(code) = code {
                        self.clear_pending(provider);
                        let token = self
                            .exchange_pkce_code(&spec, &code, &code_verifier, &redirect_uri)
                            .await?;
                        self.persist_token(provider, token)?;
                        return Ok(TokenStatus::Authorized);
                    }
                }
                if Instant::now() >= pending.deadline {
                    self.clear_pending(provider);
                    return Ok(TokenStatus::Expired);
                }
                Ok(TokenStatus::Waiting)
            }
        }
    }

    /// A valid access token, or None. Inside the refresh skew it starts a background refresh and
    /// returns None for this call; await [OAuthManager::access_token_async] where possible.
    pub fn access_token(&self, provider: &str) -> Option<String> {
        if !self.inner.enabled {
            return None;
        }
        let token = self.load_store().get(provider)?;
        if !token.needs_refresh(now_unix()) {
            return Some(token.access_token);
        }
        if token.refresh_token.is_some() {
            self.spawn_background_refresh(provider);
        }
        None
    }

    /// Refresh-if-needed and return the token, awaiting the network round-trip.
    pub async fn access_token_async(&self, provider: &str) -> Result<Option<String>, OAuthError> {
        if !self.inner.enabled {
            return Ok(None);
        }
        let token = match self.load_store().get(provider) {
            Some(token) => token,
            None => return Ok(None),
        };
        if !token.needs_refresh(now_unix()) {
            return Ok(Some(token.access_token));
        }
        if token.refresh_token.is_none() {
            return Ok(None);
        }
        self.refresh(provider).await?;
        Ok(self
            .load_store()
            .get(provider)
            .map(|token| token.access_token))
    }

    /// Redeem the refresh token and persist the rotated pair. A terminal provider error
    /// (invalid_grant, invalid_token, refresh_token_reused) sets relogin_required and keeps the
    /// grant.
    pub async fn refresh(&self, provider: &str) -> Result<(), OAuthError> {
        if !self.inner.enabled {
            return Err(OAuthError::Disabled);
        }
        let spec = resolve_provider(provider)
            .ok_or_else(|| OAuthError::UnknownProvider(provider.to_string()))?;
        let _gate = self.inner.refresh_gate.lock().await;

        let existing = self
            .load_store()
            .get(provider)
            .ok_or_else(|| OAuthError::NoToken(provider.to_string()))?;
        let refresh_token = existing
            .refresh_token
            .as_deref()
            .ok_or_else(|| OAuthError::NoRefreshToken(provider.to_string()))?;

        let token_url = spec
            .token_url
            .as_deref()
            .ok_or_else(|| missing_endpoint(&spec, "token endpoint", "TOKEN_URL"))?;
        let form = refresh_form(&spec.client_id, refresh_token, spec.refresh_uses_header);
        let mut request = self
            .inner
            .http
            .post(token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form);
        if spec.refresh_uses_header {
            request = request.header("x-nous-refresh-token", refresh_token);
        }
        if let Some(secret) = &spec.client_secret {
            request = request.basic_auth(&spec.client_id, Some(secret));
        }
        let response = request
            .send()
            .await
            .map_err(|err| http_error("token refresh request", &err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| http_error("token refresh response", &err))?;
        if !status.is_success() {
            return Err(provider_error_from_body(spec.id, status.as_u16(), &body));
        }
        let refreshed = apply_refresh_response(spec.id, &existing, &body, now_unix())?;
        self.persist_token(provider, refreshed)
    }

    /// Remove the stored grant (and any in-flight login) for provider.
    pub fn logout(&self, provider: &str) -> Result<(), OAuthError> {
        if provider_spec(provider).is_none() {
            return Err(OAuthError::UnknownProvider(provider.to_string()));
        }
        self.clear_pending(provider);
        let mut store = self.load_store();
        store.remove(provider);
        self.save_store(&store)
    }

    /// Every provider with a stored grant, in lexical order.
    pub fn list(&self) -> Vec<String> {
        self.load_store().list()
    }

    // -- Device flow --

    async fn begin_device_login(&self, spec: ResolvedProvider) -> Result<LoginStart, OAuthError> {
        let device_url = spec.device_url.as_deref().ok_or_else(|| {
            missing_endpoint(&spec, "device authorization endpoint", "DEVICE_URL")
        })?;
        let form = device_code_form(&spec.client_id, &spec.scopes);
        let mut request = self
            .inner
            .http
            .post(device_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form);
        if let Some(secret) = &spec.client_secret {
            request = request.basic_auth(&spec.client_id, Some(secret));
        }
        let response = request
            .send()
            .await
            .map_err(|err| http_error("device authorization request", &err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| http_error("device authorization response", &err))?;
        if !status.is_success() {
            return Err(provider_error_from_body(spec.id, status.as_u16(), &body));
        }
        let login = parse_device_code_response(spec.id, &body)?;
        let interval = Duration::from_secs(device_poll_interval(login.interval));
        let deadline = Instant::now() + Duration::from_secs(login.expires_in.max(1));
        self.store_pending(
            spec.id,
            PendingLogin {
                kind: PendingKind::Device {
                    device_code: String::clone(&login.device_code),
                    interval,
                },
                deadline,
            },
        );
        Ok(LoginStart::Device(login))
    }

    async fn poll_device_token(
        &self,
        spec: &ResolvedProvider,
        provider: &str,
        device_code: &str,
        interval: Duration,
    ) -> Result<TokenStatus, OAuthError> {
        let token_url = spec
            .token_url
            .as_deref()
            .ok_or_else(|| missing_endpoint(spec, "token endpoint", "TOKEN_URL"))?;
        let form = token_poll_form(&spec.client_id, device_code);
        let mut request = self
            .inner
            .http
            .post(token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form);
        if let Some(secret) = &spec.client_secret {
            request = request.basic_auth(&spec.client_id, Some(secret));
        }
        let response = request
            .send()
            .await
            .map_err(|err| http_error("device token poll", &err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| http_error("device token poll", &err))?;
        if status.is_success() {
            let token = parse_token_payload(spec.id, &body)?.into_token(None, now_unix());
            self.persist_token(provider, token)?;
            self.clear_pending(provider);
            return Ok(TokenStatus::Authorized);
        }
        let (code, description) = parse_error_body(status.as_u16(), &body);
        match code.as_str() {
            "authorization_pending" => Ok(TokenStatus::Pending {
                interval: interval.as_secs().max(1),
            }),
            "slow_down" => {
                let grown = (interval.as_secs() + 1).min(DEVICE_AUTH_SLOW_DOWN_CAP_SECONDS);
                self.update_device_interval(provider, Duration::from_secs(grown));
                Ok(TokenStatus::SlowDown { interval: grown })
            }
            _ => {
                self.clear_pending(provider);
                Ok(TokenStatus::Denied { code, description })
            }
        }
    }

    // -- PKCE flow --

    async fn begin_pkce_login(&self, spec: ResolvedProvider) -> Result<LoginStart, OAuthError> {
        let authorize_url = spec
            .authorize_url
            .as_deref()
            .ok_or_else(|| missing_endpoint(&spec, "authorization endpoint", "AUTHORIZE_URL"))?;
        // A provider that registered one exact redirect URI (Codex) needs that port and path;
        // everything else takes an ephemeral port and a nonce-carrying path.
        let listener = TcpListener::bind(("127.0.0.1", spec.callback_port))
            .await
            .map_err(|err| OAuthError::CallbackBind(err.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|err| OAuthError::CallbackBind(err.to_string()))?
            .port();
        let nonce = random_nonce();
        let callback_path = if spec.callback_path.is_empty() {
            format!("/callback/{nonce}")
        } else {
            spec.callback_path.to_string()
        };
        // OpenAI matches the registered URI literally, and it is spelled with "localhost".
        let host = if spec.callback_port == 0 {
            "127.0.0.1"
        } else {
            "localhost"
        };
        let redirect_uri = format!("http://{host}:{port}{callback_path}");
        let code_verifier = pkce_code_verifier();
        let code_challenge = pkce_code_challenge(&code_verifier);
        let state = random_nonce();
        let key_label = env_override(spec.id, "KEY_LABEL")
            .unwrap_or_else(|| DEFAULT_OPENROUTER_KEY_LABEL.to_string());
        let url = match spec.pkce_style {
            PkceStyle::OpenRouter => {
                openrouter_authorize_url(authorize_url, &redirect_uri, &code_challenge, &key_label)
            }
            PkceStyle::Standard => standard_authorize_url(
                authorize_url,
                &spec.client_id,
                &spec.scopes,
                &redirect_uri,
                &code_challenge,
                &state,
            ),
        };
        let timeout = pkce_timeout_seconds();
        self.store_pending(
            spec.id,
            PendingLogin {
                kind: PendingKind::Pkce {
                    code_verifier,
                    redirect_uri: String::clone(&redirect_uri),
                    code: None,
                    error: None,
                    error_description: None,
                    state_ok: true,
                    redirected: false,
                },
                deadline: Instant::now() + Duration::from_secs(timeout),
            },
        );

        let inner = Arc::clone(&self.inner);
        let provider = spec.id.to_string();
        let expected_path = callback_path;
        let expected_state = String::clone(&state);
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                Duration::from_secs(timeout),
                accept_loopback_callback(&listener, &expected_path, &expected_state, &provider),
            )
            .await;
            match outcome {
                Ok(Ok(callback)) => record_pkce_callback(&inner, &provider, callback),
                Ok(Err(err)) => {
                    tracing::debug!(provider = %provider, error = %err, "loopback callback listener stopped");
                }
                Err(_) => {
                    tracing::debug!(provider = %provider, "loopback callback window elapsed");
                }
            }
        });

        Ok(LoginStart::Pkce(PkceLogin {
            authorize_url: url,
            redirect_uri,
            state,
        }))
    }

    async fn exchange_pkce_code(
        &self,
        spec: &ResolvedProvider,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, OAuthError> {
        let token_url = spec
            .token_url
            .as_deref()
            .ok_or_else(|| missing_endpoint(spec, "token endpoint", "TOKEN_URL"))?;
        let request = self
            .inner
            .http
            .post(token_url)
            .header(reqwest::header::ACCEPT, "application/json");
        let request = match spec.pkce_style {
            PkceStyle::OpenRouter => request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&json!({
                    "code": code,
                    "code_verifier": code_verifier,
                    "code_challenge_method": "S256",
                })),
            // RFC 6749 §4.1.3 with the PKCE verifier and the redirect URI the code was
            // issued against, which the provider re-checks.
            PkceStyle::Standard => request.form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", code_verifier),
                ("client_id", spec.client_id.as_str()),
                ("redirect_uri", redirect_uri),
            ]),
        };
        let response = request
            .send()
            .await
            .map_err(|err| http_error("PKCE code exchange", &err))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| http_error("PKCE code exchange", &err))?;
        if status.as_u16() == 403 {
            return Err(OAuthError::Provider {
                provider: spec.id.to_string(),
                code: "openrouter_token_exchange_denied".to_string(),
                description: "OpenRouter rejected the authorization code (invalid, already used, \
                              or older than 10 minutes). Run the login again."
                    .to_string(),
                relogin_required: true,
            });
        }
        if !status.is_success() {
            return Err(provider_error_from_body(spec.id, status.as_u16(), &body));
        }
        match spec.pkce_style {
            PkceStyle::OpenRouter => Ok(OAuthToken {
                access_token: parse_exchange_key(spec.id, &body)?,
                refresh_token: None,
                expires_at: 0,
                token_type: "Bearer".to_string(),
            }),
            PkceStyle::Standard => {
                let payload = parse_token_payload(spec.id, &body)?;
                Ok(payload.into_token(None, chrono::Utc::now().timestamp()))
            }
        }
    }

    // -- Store plumbing --

    fn load_store(&self) -> OAuthStore<'_> {
        let _guard = self
            .inner
            .store_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        OAuthStore::open(self.inner.store_path.as_path())
    }

    fn save_store(&self, store: &OAuthStore<'_>) -> Result<(), OAuthError> {
        let _guard = self
            .inner
            .store_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        store.save()
    }

    fn persist_token(&self, provider: &str, token: OAuthToken) -> Result<(), OAuthError> {
        let mut store = self.load_store();
        store.set(provider, token);
        self.save_store(&store)
    }

    fn store_pending(&self, provider: &str, pending: PendingLogin) {
        let mut map = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        map.insert(provider.to_string(), pending);
    }

    fn pending_snapshot(&self, provider: &str) -> Option<PendingLogin> {
        let map = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        map.get(provider).cloned()
    }

    fn clear_pending(&self, provider: &str) {
        let mut map = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        map.remove(provider);
    }

    fn update_device_interval(&self, provider: &str, interval: Duration) {
        let mut map = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(pending) = map.get_mut(provider) {
            if let PendingKind::Device {
                interval: current, ..
            } = &mut pending.kind
            {
                *current = interval;
            }
        }
    }

    fn pending_providers(&self) -> Vec<String> {
        let map = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        map.keys().cloned().collect()
    }

    fn spawn_background_refresh(&self, provider: &str) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let manager = OAuthManager::clone(self);
        let provider = provider.to_string();
        handle.spawn(async move {
            if let Err(err) = manager.refresh(&provider).await {
                tracing::debug!(provider = %provider, error = %err, "background OAuth refresh failed");
            }
        });
    }
}

// -- Request/response builders (pure) -----------------------------------------------------------

fn device_poll_interval(server_interval: u64) -> u64 {
    let cap = std::env::var("SILVER_OAUTH_DEVICE_POLL_CAP_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEVICE_AUTH_POLL_INTERVAL_CAP_SECONDS);
    server_interval.max(1).min(cap.max(1))
}

fn pkce_timeout_seconds() -> u64 {
    std::env::var("SILVER_OAUTH_PKCE_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PKCE_TIMEOUT_SECONDS)
}

fn device_code_form(client_id: &str, scope: &str) -> Vec<(&'static str, String)> {
    let mut form = vec![("client_id", client_id.to_string())];
    if !scope.trim().is_empty() {
        form.push(("scope", scope.to_string()));
    }
    form
}

fn token_poll_form(client_id: &str, device_code: &str) -> Vec<(&'static str, String)> {
    vec![
        ("grant_type", DEVICE_CODE_GRANT_TYPE.to_string()),
        ("client_id", client_id.to_string()),
        ("device_code", device_code.to_string()),
    ]
}

fn refresh_form(
    client_id: &str,
    refresh_token: &str,
    uses_header: bool,
) -> Vec<(&'static str, String)> {
    let mut form = vec![("grant_type", "refresh_token".to_string())];
    if !uses_header {
        form.push(("refresh_token", refresh_token.to_string()));
    }
    if !client_id.trim().is_empty() {
        form.push(("client_id", client_id.to_string()));
    }
    form
}

/// Parse a device-authorization response, preferring verification_uri_complete.
fn parse_device_code_response(provider: &str, body: &str) -> Result<DeviceLogin, OAuthError> {
    let value: Value = serde_json::from_str(body).map_err(|_not_json| OAuthError::InvalidJson {
        provider: provider.to_string(),
    })?;
    let device_code = json_str(&value, "device_code").ok_or_else(|| OAuthError::MissingField {
        provider: provider.to_string(),
        field: "device_code",
    })?;
    let user_code = json_str(&value, "user_code").ok_or_else(|| OAuthError::MissingField {
        provider: provider.to_string(),
        field: "user_code",
    })?;
    let verification_uri = json_str(&value, "verification_uri_complete")
        .or_else(|| json_str(&value, "verification_uri"))
        .ok_or_else(|| OAuthError::MissingField {
            provider: provider.to_string(),
            field: "verification_uri",
        })?;
    let expires_in = json_u64(&value, "expires_in").ok_or_else(|| OAuthError::MissingField {
        provider: provider.to_string(),
        field: "expires_in",
    })?;
    let interval = json_u64(&value, "interval").unwrap_or(5);
    Ok(DeviceLogin {
        user_code,
        verification_uri,
        device_code,
        interval: interval.max(1),
        expires_in: expires_in.max(1),
    })
}

#[derive(Clone)]
struct TokenPayload {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
}

impl TokenPayload {
    fn into_token(self, previous_refresh: Option<String>, now_unix: i64) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token.or(previous_refresh),
            expires_at: now_unix.saturating_add(self.expires_in as i64),
            token_type: if self.token_type.trim().is_empty() {
                "Bearer".to_string()
            } else {
                self.token_type
            },
        }
    }
}

fn parse_token_payload(provider: &str, body: &str) -> Result<TokenPayload, OAuthError> {
    let value: Value = serde_json::from_str(body).map_err(|_not_json| OAuthError::InvalidJson {
        provider: provider.to_string(),
    })?;
    let access_token =
        json_str(&value, "access_token").ok_or_else(|| OAuthError::MissingField {
            provider: provider.to_string(),
            field: "access_token",
        })?;
    Ok(TokenPayload {
        access_token,
        refresh_token: json_str(&value, "refresh_token"),
        expires_in: json_u64(&value, "expires_in").unwrap_or(DEFAULT_TOKEN_TTL_SECONDS),
        token_type: json_str(&value, "token_type").unwrap_or_else(|| "Bearer".to_string()),
    })
}

/// Apply a canned/real refresh body to an existing grant, keeping the old refresh token when the
/// response does not rotate one.
fn apply_refresh_response(
    provider: &str,
    existing: &OAuthToken,
    body: &str,
    now_unix: i64,
) -> Result<OAuthToken, OAuthError> {
    let payload = parse_token_payload(provider, body)?;
    Ok(payload.into_token(Option::clone(&existing.refresh_token), now_unix))
}

fn parse_exchange_key(provider: &str, body: &str) -> Result<String, OAuthError> {
    let value: Value = serde_json::from_str(body).map_err(|_not_json| OAuthError::InvalidJson {
        provider: provider.to_string(),
    })?;
    json_str(&value, "key").ok_or_else(|| OAuthError::MissingField {
        provider: provider.to_string(),
        field: "key",
    })
}

/// Build the OpenRouter authorize URL:
/// ?callback_url=&code_challenge=&code_challenge_method=S256[&key_label=].
fn openrouter_authorize_url(
    authorize_url: &str,
    redirect_uri: &str,
    code_challenge: &str,
    key_label: &str,
) -> String {
    let mut params = vec![
        ("callback_url", redirect_uri.to_string()),
        ("code_challenge", code_challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
    ];
    if !key_label.trim().is_empty() {
        params.push(("key_label", key_label.to_string()));
    }
    format!("{authorize_url}?{}", encode_query(&params))
}

/// Build an RFC 6749 authorization URL with PKCE.
fn standard_authorize_url(
    authorize_url: &str,
    client_id: &str,
    scopes: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let mut params = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_challenge", code_challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("state", state.to_string()),
    ];
    if !scopes.trim().is_empty() {
        params.push(("scope", scopes.to_string()));
    }
    format!("{authorize_url}?{}", encode_query(&params))
}

fn encode_query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_form_component(key),
                encode_form_component(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// application/x-www-form-urlencoded component encoding (space becomes +), matching Python's
/// urllib.parse.urlencode.
fn encode_form_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn parse_error_body(status: u16, body: &str) -> (String, String) {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = json_str(&value, "error").unwrap_or_else(|| status.to_string());
    let description = json_str(&value, "error_description")
        .or_else(|| json_str(&value, "error_message"))
        .unwrap_or_else(|| clip_and_redact(body));
    (code, description)
}

fn provider_error_from_body(provider: &str, status: u16, body: &str) -> OAuthError {
    let (code, description) = parse_error_body(status, body);
    let relogin_required = matches!(
        code.as_str(),
        "invalid_grant" | "invalid_token" | "refresh_token_reused"
    ) || description.to_lowercase().contains("reuse");
    OAuthError::Provider {
        provider: provider.to_string(),
        code,
        description,
        relogin_required,
    }
}

fn http_error(context: &str, err: &reqwest::Error) -> OAuthError {
    OAuthError::Http {
        context: context.to_string(),
        detail: redact(&err.to_string()),
    }
}

fn clip_and_redact(body: &str) -> String {
    let clipped: String = body.chars().take(ERROR_BODY_LIMIT).collect();
    redact(clipped.trim())
}

fn json_str(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(text)) => nonempty(text),
        _ => None,
    }
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

// -- PKCE primitives ----------------------------------------------------------------------------

/// Generate an RFC 7636 code verifier: 64 unreserved hex characters.
fn pkce_code_verifier() -> String {
    let mut verifier = format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    );
    verifier.truncate(64);
    verifier
}

/// S256: base64url(no padding) of SHA-256(verifier), per RFC 7636 section 4.2.
fn pkce_code_challenge(code_verifier: &str) -> String {
    base64url_nopad(&sha256(code_verifier.as_bytes()))
}

fn random_nonce() -> String {
    format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    )
}

fn base64url_nopad(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((combined >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((combined >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((combined >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(combined & 0x3f) as usize] as char);
        }
    }
    out
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Minimal SHA-256 (FIPS 180-4). Vendored because sha2 is a silver-core-only dependency and
/// this module may not touch Cargo.toml.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

// -- Loopback callback listener (RFC 8252) ------------------------------------------------------

struct PkceCallback {
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    state_ok: bool,
}

async fn accept_loopback_callback(
    listener: &TcpListener,
    expected_path: &str,
    expected_state: &str,
    display_name: &str,
) -> std::io::Result<PkceCallback> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = [0u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]);
        let request_line = request.lines().next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();

        if !method.eq_ignore_ascii_case("GET") {
            respond(&mut socket, "405 Method Not Allowed", "Method not allowed.").await?;
            continue;
        }
        let (path, query) = split_target(target);
        if path != expected_path {
            respond(&mut socket, "404 Not Found", "Not found.").await?;
            continue;
        }
        let params = parse_query(query);
        let state_ok = match params.get("state") {
            Some(state) => state == expected_state,
            None => true, // OpenRouter echoes no state; the nonce lives in the callback path.
        };
        let callback = PkceCallback {
            code: params.get("code").cloned(),
            error: params.get("error").cloned(),
            error_description: params.get("error_description").cloned(),
            state_ok,
        };
        let outcome = if callback.error.is_some() {
            "failed"
        } else {
            "received"
        };
        let body = format!(
            "<html><body><h1>{display_name} authorization {outcome}.</h1>\
             You can close this tab.</body></html>"
        );
        respond(&mut socket, "200 OK", &body).await?;
        return Ok(callback);
    }
}

async fn respond(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    drop(socket.shutdown().await);
    Ok(())
}

fn record_pkce_callback(inner: &OAuthInner, provider: &str, callback: PkceCallback) {
    let mut map = inner.pending.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(pending) = map.get_mut(provider) {
        if let PendingKind::Pkce {
            code,
            error,
            error_description,
            state_ok,
            redirected,
            ..
        } = &mut pending.kind
        {
            *code = callback.code;
            *error = callback.error;
            *error_description = callback.error_description;
            *state_ok = callback.state_ok;
            *redirected = true;
        }
    }
}

fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// -- Token store persistence --------------------------------------------------------------------

fn read_tokens(path: &Path) -> Option<BTreeMap<String, OAuthToken>> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<BTreeMap<String, OAuthToken>>(&bytes) {
        Ok(tokens) => Some(tokens),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "ignoring corrupt OAuth token store"
            );
            None
        }
    }
}

// -- Tests (offline) ----------------------------------------------------------------------------
