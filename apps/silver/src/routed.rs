//! The model route `/login` steers. [RoutedModel] consults the credential store on every request:
//! it streams through the active provider's endpoint and credential, else is the configured model.
//! Only those are routed; the model id travels on [ModelRequest::model].

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{Model, ModelRequest, ModelStream};
use silver_protocol::providers::{is_local_endpoint, preset, ProviderKind};
use tokio_util::sync::CancellationToken;

use crate::anthropic::AnthropicProvider;
use crate::auth::AuthStore;
use crate::copilot::CopilotProvider;
use crate::oauth::OAuthManager;
use crate::provider::OpenAiCompatibleProvider;

tokio::task_local! {
    /// The provider the current run's session is pinned to; see [with_session_provider].
    static SESSION_PROVIDER: String;
}

/// Run `future` with every route inside it pinned to `provider` instead of the active one.
/// A task-local rather than a request field, so the timeouts, context window and name that
/// the turn asks of the model all follow the same pin as the requests do.
pub async fn with_session_provider<F: std::future::Future>(
    provider: Option<String>,
    future: F,
) -> F::Output {
    match provider {
        Some(provider) => SESSION_PROVIDER.scope(provider, future).await,
        None => future.await,
    }
}

/// Where a request's credential came from, for the auth listing and for logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// Typed at `/login` and kept in the credential store.
    Stored,
    /// An OAuth grant held by the token store.
    OAuth,
    /// The provider's API key environment variable.
    Env,
    /// A local endpoint that needs no credential.
    None,
}

impl KeySource {
    /// Stable label for the API and the TUI.
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::Stored => "stored",
            KeySource::OAuth => "oauth",
            KeySource::Env => "env",
            KeySource::None => "none",
        }
    }
}

/// One resolved route: the endpoint, credential and default model of a provider.
#[derive(Clone, Debug)]
pub struct Route {
    /// Provider id, as in the shared presets.
    pub provider: String,
    /// Transport family to speak.
    pub kind: ProviderKind,
    /// Resolved endpoint.
    pub base_url: String,
    /// Model the provider should serve when the request names none.
    pub model: String,
    /// Where the credential came from.
    pub source: KeySource,
    key: String,
}

impl Route {
    /// Whether the route carries a usable credential.
    pub fn authenticated(&self) -> bool {
        !self.key.is_empty() || self.source == KeySource::None
    }

    /// Whether the route's endpoint is on this machine or its LAN; an ACP command or an
    /// OAuth-backed hosted surface never is.
    pub fn is_local(&self) -> bool {
        matches!(
            self.kind,
            ProviderKind::OpenAiCompatible | ProviderKind::Ollama | ProviderKind::Anthropic
        ) && is_local_endpoint(&self.base_url)
    }

    /// The resolved credential, for daemon-internal calls such as the activation probe.
    /// It is never serialized into an API response.
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

/// A transport kept until its route changes.
struct CachedRoute {
    fingerprint: u64,
    model: Arc<dyn Model>,
}

/// What `[model]` in config.toml names. The `custom` preset has no base URL or model of its own, so
/// its route borrows these; otherwise `/model` under it would route to `""` and fail.
#[derive(Clone, Debug)]
pub struct ConfiguredEndpoint {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub key: String,
}

/// The model the agent holds: the configured transport plus live credential routing.
pub struct RoutedModel {
    auth: Arc<AuthStore>,
    oauth: Arc<OAuthManager>,
    configured: Arc<dyn Model>,
    configured_endpoint: Option<ConfiguredEndpoint>,
    http: reqwest::Client,
    cache: Mutex<Option<CachedRoute>>,
}

impl RoutedModel {
    /// Wrap the configured model with credential routing.
    pub fn new(
        configured: Arc<dyn Model>,
        auth: Arc<AuthStore>,
        oauth: Arc<OAuthManager>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            auth,
            oauth,
            configured,
            configured_endpoint: None,
            http,
            cache: Mutex::new(None),
        }
    }

    /// Let routes for the configured provider fill their blanks from config.toml.
    pub fn with_configured_endpoint(mut self, endpoint: ConfiguredEndpoint) -> Self {
        self.configured_endpoint = Some(endpoint);
        self
    }

    /// config.toml's `[model]` endpoint, when the daemon booted with one.
    pub fn configured_endpoint(&self) -> Option<&ConfiguredEndpoint> {
        self.configured_endpoint.as_ref()
    }

    /// The client every transport shares, with the configured TLS policy.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Fill in what a preset leaves blank from `[model]`, for the configured provider only,
    /// then fall back to the preset's default model. config.toml's model comes first, so a
    /// placeholder like LM Studio's `local-model` never shadows the model the user named.
    fn complete(&self, route: Route) -> Route {
        let mut route = match self.configured_endpoint.as_ref() {
            Some(endpoint) => complete_route(endpoint, route),
            None => route,
        };
        if route.model.trim().is_empty() {
            if let Some(preset) = preset(&route.provider) {
                route.model = preset.default_model.to_string();
            }
        }
        route
    }

    /// The provider requests route through: the running session's pin, else the one
    /// `/login` activated, if any.
    pub fn active_provider(&self) -> Option<String> {
        SESSION_PROVIDER
            .try_with(Clone::clone)
            .ok()
            .or_else(|| self.auth.active())
    }

    /// Resolve the route a request would take, refreshing a stale OAuth token. Credentials are
    /// tried as a request uses them: stored key, OAuth grant, preset key variable.
    pub async fn resolve(&self, provider: &str) -> Result<Route, CoreError> {
        let preset = preset(provider)
            .ok_or_else(|| CoreError::InvalidRequest(format!("unknown provider: {provider}")))?;
        let token = self.oauth_key(provider).await;
        let mut route = self.complete(assemble(
            provider,
            preset,
            self.auth.credential(provider).as_ref(),
            token,
        ));
        // An external agent mode has no endpoint: its base URL is the command to spawn, resolved
        // from the login-shell PATH. A stored override is kept.
        if route.kind == ProviderKind::Acp && route.base_url.trim().is_empty() {
            crate::agent_modes::ensure_path().await;
            route.base_url = crate::agent_modes::command_for(&route.model)?.unwrap_or_default();
        }
        Ok(route)
    }

    /// A route resolved without network calls, for listings: an OAuth grant counts as present
    /// unrefreshed.
    pub fn describe(&self, provider: &str) -> Option<Route> {
        let preset = preset(provider)?;
        let token = self.oauth.access_token(provider);
        Some(self.complete(assemble(
            provider,
            preset,
            self.auth.credential(provider).as_ref(),
            token,
        )))
    }

    /// The OAuth access token for a provider, refreshed when stale. A refresh failure
    /// degrades to "no token" so the request can still fall back to an environment key.
    async fn oauth_key(&self, provider: &str) -> Option<String> {
        match self.oauth.access_token_async(provider).await {
            Ok(token) => token.filter(|token| !token.is_empty()),
            Err(error) => {
                tracing::warn!(%error, provider, "could not refresh the OAuth token");
                None
            }
        }
    }

    /// Build a transport for one provider, resolving its credential the same way a request
    /// would. Used for the side models (Mixture of Agents references) that are not the
    /// active route, so it never touches the request cache.
    pub async fn transport_for(
        &self,
        provider: &str,
        model: Option<&str>,
    ) -> Result<Arc<dyn Model>, CoreError> {
        let mut route = self.resolve(provider).await?;
        if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
            route.model = model.to_string();
        }
        if !route.authenticated() {
            return Err(CoreError::ProviderUnavailable(format!(
                "no credential for {provider}; sign in with /login {provider}"
            )));
        }
        Ok(self.build_transport(&route))
    }

    /// The transport for a route, reusing the cached one while the route is unchanged.
    fn transport(&self, route: &Route) -> Arc<dyn Model> {
        let fingerprint = route_fingerprint(route);
        if let Ok(cache) = self.cache.lock() {
            if let Some(cached) = cache.as_ref() {
                if cached.fingerprint == fingerprint {
                    return Arc::clone(&cached.model);
                }
            }
        }
        let model = self.build_transport(route);
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(CachedRoute {
                fingerprint,
                model: Arc::clone(&model),
            });
        }
        tracing::info!(
            provider = %route.provider,
            base_url = %route.base_url,
            source = route.source.as_str(),
            "routing runs through the signed-in provider"
        );
        model
    }

    /// Build a fresh transport for a resolved route.
    fn build_transport(&self, route: &Route) -> Arc<dyn Model> {
        build_transport(
            route.kind,
            &route.base_url,
            &route.key,
            &route.model,
            &self.http,
        )
    }
}

/// A transport of one provider family.
pub fn build_transport(
    kind: ProviderKind,
    base_url: &str,
    key: &str,
    model: &str,
    http: &reqwest::Client,
) -> Arc<dyn Model> {
    let http = reqwest::Client::clone(http);
    match kind {
        ProviderKind::Anthropic => {
            Arc::new(AnthropicProvider::new(base_url, key, model).with_http_client(http))
        }
        ProviderKind::OpenAiCompatible | ProviderKind::Ollama => {
            Arc::new(OpenAiCompatibleProvider::new(base_url, key, model).with_http_client(http))
        }
        // Copilot holds the GitHub token and mints its own bearer [redacted] turn, so the transport
        // is built once and keeps that minted token cached.
        ProviderKind::Copilot => {
            Arc::new(CopilotProvider::new(base_url, key, model).with_http_client(http))
        }
        ProviderKind::Bedrock => Arc::new(
            crate::bedrock::BedrockProvider::new(base_url, key, model).with_http_client(http),
        ),
        ProviderKind::Vertex => Arc::new(
            crate::vertex::VertexProvider::new(base_url, key, model).with_http_client(http),
        ),
        ProviderKind::Codex => {
            Arc::new(crate::codex::CodexProvider::new(base_url, key, model).with_http_client(http))
        }
        // The ACP agent is a process, not an endpoint: `base_url` is the command to spawn,
        // already resolved by [RoutedModel::resolve] or stored as an override.
        ProviderKind::Acp => Arc::new(crate::acp::AcpProvider::new(base_url.to_string(), model)),
        ProviderKind::OpenCode => Arc::new(
            crate::opencode::OpenCodeProvider::new(base_url, key, model).with_http_client(http),
        ),
    }
}

#[async_trait]
impl Model for RoutedModel {
    fn name(&self) -> &str {
        self.configured.name()
    }

    /// Answers for the route the next request takes: the signed-in provider's endpoint
    /// (resolved without any network call, so an OAuth refresh is never triggered here)
    /// or, with no provider signed in, whatever the configured model says.
    fn is_local(&self) -> bool {
        match self
            .active_provider()
            .and_then(|provider| self.describe(&provider))
        {
            Some(route) => route.is_local(),
            None => self.configured.is_local(),
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let Some(provider) = self.active_provider() else {
            return self.configured.stream(request, cancel).await;
        };
        let route = self.resolve(&provider).await?;
        if !route.authenticated() {
            return Err(CoreError::ProviderUnavailable(format!(
                "no credential for {provider}; sign in again with /login {provider}"
            )));
        }
        self.transport(&route).stream(request, cancel).await
    }
}

/// Borrow config.toml's endpoint, model and key where the route's preset left them blank. Only the
/// configured provider qualifies, and a stored endpoint or model is kept.
pub fn complete_route(endpoint: &ConfiguredEndpoint, mut route: Route) -> Route {
    if !route
        .provider
        .eq_ignore_ascii_case(endpoint.provider.trim())
    {
        return route;
    }
    if route.base_url.trim().is_empty() {
        route.base_url.clone_from(&endpoint.base_url);
    }
    if route.model.trim().is_empty() {
        route.model.clone_from(&endpoint.model);
    }
    if route.key.trim().is_empty() && !endpoint.key.trim().is_empty() {
        route.key.clone_from(&endpoint.key);
        route.source = KeySource::Env;
    }
    route
}

fn assemble(
    provider: &str,
    preset: &'static silver_protocol::providers::ProviderPreset,
    credential: Option<&crate::auth::ProviderCredential>,
    oauth_token: Option<String>,
) -> Route {
    let base_url = credential
        .and_then(|entry| entry.base_url.as_deref())
        .filter(|url| !url.trim().is_empty())
        .unwrap_or(preset.base_url)
        .to_string();
    let model = credential
        .and_then(|entry| entry.model.as_deref())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    let stored = credential
        .and_then(|entry| entry.api_key.as_deref())
        .filter(|key| !key.trim().is_empty())
        .map(str::to_string);
    let (key, source) = match stored {
        Some(key) => (key, KeySource::Stored),
        None => match oauth_token.filter(|token| !token.trim().is_empty()) {
            Some(token) => (token, KeySource::OAuth),
            None => match std::env::var(preset.api_key_env)
                .ok()
                .filter(|key| !key.trim().is_empty())
            {
                Some(key) => (key, KeySource::Env),
                None if !preset.requires_key => (String::new(), KeySource::None),
                None => (String::new(), KeySource::Env),
            },
        },
    };
    Route {
        provider: provider.to_string(),
        kind: preset.kind,
        base_url,
        model,
        source,
        key,
    }
}

/// Hash the parts of a route that decide whether the transport must be rebuilt.
fn route_fingerprint(route: &Route) -> u64 {
    let mut hasher = DefaultHasher::new();
    route.provider.hash(&mut hasher);
    route.kind.as_str().hash(&mut hasher);
    route.base_url.hash(&mut hasher);
    route.model.hash(&mut hasher);
    route.key.hash(&mut hasher);
    hasher.finish()
}
