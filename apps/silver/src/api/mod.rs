//! HTTP API surface and shared state.

pub mod advisor;
pub mod agents;
pub mod agui;
pub mod approvals;
pub mod chat;
pub mod checkpoints;
pub mod commands;
pub mod credentials;
pub mod daemon;
pub mod git;
pub mod health;
pub mod insights;
pub mod memory;
pub mod oauth;
pub mod presets;
pub mod provider;
pub mod rate_limit;
pub mod runs;
pub mod sessions;
pub mod skills;
pub mod sse;
pub mod tools;
pub mod ui;
pub mod workspaces;

use crate::config::Config;
use crate::db::Db;
use crate::run_manager::RunManager;
use axum::{
    extract::Request,
    http::{header, header::AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use silver_core::error::CoreError;
use silver_core::redact::redact;
use silver_protocol::{ApiError, ErrorCode};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub runs: Arc<RunManager>,
    pub config: Arc<Config>,
    /// The managed ai-memory server's base URL, for the Messages memory panel. None when memory
    /// is off.
    pub memory_endpoint: Option<String>,
    pub started_at: DateTime<Utc>,
    /// Filesystem checkpoint store, shared with the write tools. None disables the checkpoint
    /// endpoints (and snapshot capture) for embedding tests.
    pub checkpoints: Option<Arc<crate::checkpoints::Checkpoints>>,
    /// OAuth manager backing the /v1/oauth routes. None disables them.
    pub oauth: Option<Arc<crate::oauth::OAuthManager>>,
    /// Provider credential store backing the /v1/auth routes. None disables them.
    pub auth: Option<Arc<crate::auth::AuthStore>>,
    /// The routed model, so /v1/auth can report and validate what a run would use.
    pub routes: Option<Arc<crate::routed::RoutedModel>>,
    /// The context window resolved for the configured model at startup, so a client can show
    /// it before the first run reports one. None when it could not be determined.
    pub context_length: Option<usize>,
    /// The models.dev registry resolver, so /v1/models can report a model's supported
    /// reasoning-effort levels. None when the daemon has no resolver.
    pub context_resolver: Option<Arc<crate::context_length::ModelContextResolver>>,
    /// The Jev advisor behind /v1/advisor. None disables the route.
    pub advisor: Option<Arc<crate::advisor::JevAdvisor>>,
    /// The subagent definition store behind /v1/agents. None disables the route.
    pub agents: Option<Arc<crate::subagents::AgentStore>>,
    /// The bot chat behind /v1/chat.
    pub chat: Arc<crate::chat::ChatHub>,
    /// Cancelled on SIGINT/SIGTERM so every long-lived stream ends and the graceful shutdown
    /// does not wait for a client still connected.
    pub shutdown: CancellationToken,
}

#[derive(Debug)]
pub struct ApiFailure(pub CoreError);

impl From<CoreError> for ApiFailure {
    fn from(error: CoreError) -> Self {
        ApiFailure(error)
    }
}

/// A persistence failure becomes the stable API error: BUSY/LOCKED is a retryable Conflict and
/// raw SQLite text is never forwarded.
impl From<crate::db::DbError> for ApiFailure {
    fn from(error: crate::db::DbError) -> Self {
        ApiFailure(error.into())
    }
}

impl From<silver_protocol::IdParseError> for ApiFailure {
    fn from(error: silver_protocol::IdParseError) -> Self {
        ApiFailure(CoreError::InvalidRequest(error.to_string()))
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let code = self.0.code();
        let status =
            StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ApiError::new(code, public_message(&self.0));
        (status, Json(body)).into_response()
    }
}

fn public_message(error: &CoreError) -> String {
    match error {
        CoreError::Internal(_) => "internal error".to_string(),
        // `code` already names the kind; the UI shows the message as is, so drop the
        // "invalid request: " / "conflict: " prefix.
        CoreError::InvalidRequest(message) | CoreError::Conflict(message) => redact(message),
        // Non-internal messages can embed upstream bodies; scrub credential shapes before
        // they reach a client.
        other => redact(&other.to_string()),
    }
}

/// Cap on the buffered body of a monitored run-creation response. The body is a small
/// fixed-shape JSON object, so this is far above any real response.
const MAX_MONITORED_BODY_BYTES: usize = 64 * 1024;

/// Security headers applied to every response, including errors.
const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    ("permissions-policy", "interest-cohort=()"),
    (
        "content-security-policy",
        "default-src 'none'; frame-ancestors 'none'",
    ),
    (
        "strict-transport-security",
        "max-age=31536000; includeSubDomains",
    ),
];

/// Attach the hardening headers to a response.
fn apply_security_headers(headers: &mut HeaderMap) {
    for (name, value) in SECURITY_HEADERS {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
}

/// Middleware wrapper: run the inner service, then stamp every response with the headers.
/// UI pages get a same-origin CSP in place of the API's `default-src 'none'`.
async fn security_headers(request: Request, next: Next) -> Response {
    let ui = ui::is_ui_path(request.uri().path());
    let mut response = next.run(request).await;
    if !ui && response.status().is_client_error() && !is_json(&response) {
        response = into_error_envelope(response).await;
    }
    apply_security_headers(response.headers_mut());
    if ui {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(ui::CSP),
        );
    }
    response
}

fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
}

/// Axum's own rejections (malformed JSON, an oversized body, an unknown route) are plain text;
/// rewrap them so every API error has the documented envelope and clients can parse it.
async fn into_error_envelope(response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if text.is_empty() {
        parts
            .status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_lowercase()
    } else {
        text
    };
    let body =
        serde_json::to_vec(&ApiError::new(ErrorCode::InvalidRequest, message)).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, axum::body::Body::from(body))
}

fn body_too_large(limit: u64) -> Response {
    let message = format!(
        "request too large: the server accepts up to {:.1} MiB (server.request_body_limit_bytes), \
         and an attached file counts about a third more once encoded",
        limit as f64 / 1_048_576.0
    );
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(ApiError::new(ErrorCode::InvalidRequest, message)),
    )
        .into_response()
}

fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health::health))
        .route("/v1/capabilities", get(health::capabilities))
        .route(
            "/v1/workspaces",
            post(workspaces::create).get(workspaces::list),
        )
        .route("/v1/workspaces/pick", post(workspaces::pick))
        .route(
            "/v1/workspaces/{workspace_id}",
            get(workspaces::get_one).delete(workspaces::remove),
        )
        .route("/v1/workspaces/{workspace_id}/files", get(workspaces::file))
        .route("/v1/workspaces/{workspace_id}/memory", get(memory::view))
        .route(
            "/v1/workspaces/{workspace_id}/memory/page",
            get(memory::page),
        )
        .route(
            "/v1/workspaces/{workspace_id}/attachments",
            post(workspaces::attach),
        )
        .route("/v1/sessions", post(sessions::create).get(sessions::list))
        .route(
            "/v1/sessions/{session_id}",
            get(sessions::get_one)
                .patch(sessions::update)
                .delete(sessions::remove),
        )
        .route(
            "/v1/sessions/{session_id}/messages",
            get(sessions::messages),
        )
        .route("/v1/sessions/{session_id}/usage", get(sessions::usage))
        .route("/v1/sessions/{session_id}/plan", get(sessions::plan))
        .route("/v1/sessions/{session_id}/trace", get(sessions::trace))
        .route(
            "/v1/sessions/{session_id}/injected",
            get(sessions::injected),
        )
        .route("/v1/sessions/{session_id}/rewind", post(sessions::rewind))
        .route("/v1/daemon/pause", post(daemon::pause))
        .route("/v1/daemon/resume", post(daemon::resume))
        .route("/v1/daemon/status", get(daemon::status))
        .route("/v1/approvals", get(approvals::get).post(approvals::set))
        .route("/v1/advisor", get(advisor::get).post(advisor::set))
        .route("/v1/provider/status", get(provider::status))
        .route("/v1/insights", get(insights::insights))
        .route("/v1/runs", post(runs::create))
        .route("/v1/runs/{run_id}", get(runs::get_one))
        .route("/v1/runs/{run_id}/events", get(sse::events))
        .route("/v1/runs/{run_id}/stop", post(runs::stop))
        .route("/v1/runs/{run_id}/steer", post(runs::steer))
        .route("/v1/runs/{run_id}/approval", post(runs::approval))
        .route("/agent", post(agui::run))
        .route("/v1/chat/bots", get(chat::bots).post(chat::create))
        .route(
            "/v1/chat/bots/{id}",
            axum::routing::patch(chat::update).delete(chat::remove),
        )
        .route("/v1/chat/bots/{id}/entries", get(chat::entries))
        .route("/v1/chat/bots/{id}/send", post(chat::send))
        .route("/v1/chat/bots/{id}/stop", post(chat::stop))
        .route("/v1/chat/bots/{id}/read", post(chat::read))
        .route("/v1/chat/bots/{id}/new-session", post(chat::new_session))
        .route("/v1/chat/entries/{id}/react", post(chat::react))
        .route("/v1/chat/entries/{id}/answer", post(chat::answer))
        .route("/v1/chat/events", get(chat::events))
        .route("/v1/checkpoints", get(checkpoints::list))
        .route("/v1/checkpoints/{id}/restore", post(checkpoints::restore))
        .route("/v1/diff", get(git::diff))
        .route(
            "/v1/worktrees",
            get(git::worktree_list).post(git::worktree_create),
        )
        .route("/v1/worktrees/{name}", delete(git::worktree_remove))
        .route("/v1/models", get(credentials::models))
        .route("/v1/auth", get(credentials::list))
        .route(
            "/v1/auth/{provider}",
            post(credentials::save).delete(credentials::forget),
        )
        .route("/v1/auth/{provider}/activate", post(credentials::activate))
        .route("/v1/oauth/status", get(oauth::status))
        .route("/v1/oauth/{provider}/login", post(oauth::login))
        .route("/v1/oauth/{provider}/poll", get(oauth::poll))
        .route("/v1/oauth/{provider}/logout", post(oauth::logout))
        .route("/v1/tools", get(tools::list))
        .route("/v1/commands", get(commands::list))
        .route("/v1/presets", get(presets::list).post(presets::create))
        .route(
            "/v1/presets/{id}",
            put(presets::update).delete(presets::remove),
        )
        .fallback(ui::serve)
        .route("/v1/skills", get(skills::list))
        .route("/v1/agents", get(agents::list).post(agents::create))
        .route(
            "/v1/agents/{name}",
            get(agents::get_one)
                .put(agents::update)
                .delete(agents::delete),
        )
}

pub fn router(state: AppState) -> Router {
    let body_limit = state.config.server.request_body_limit_bytes;
    let rate_limit_per_minute = state.config.server.rate_limit_per_minute;
    let cors_allowed_origins = &state.config.server.cors_allowed_origins;
    let cors = (!cors_allowed_origins.is_empty()).then(|| cors_layer(cors_allowed_origins));

    let mut router = routes()
        .layer(RequestBodyLimitLayer::new(body_limit as usize))
        .layer(axum::middleware::map_response(
            move |response: Response| async move {
                if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    body_too_large(body_limit)
                } else {
                    response
                }
            },
        ))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn_with_state(
            AppState::clone(&state),
            auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            AppState::clone(&state),
            monitor_runs,
        ));

    // Rate limiting sits outside auth so unauthenticated floods are rejected too. It is only
    // installed when configured, so a default daemon keeps its existing behaviour (and needs no
    // ConnectInfo).
    if rate_limit_per_minute > 0 {
        let limiter = Arc::new(rate_limit::RateLimiter::new(rate_limit_per_minute));
        router = router.layer(axum::middleware::from_fn_with_state(
            limiter,
            rate_limit::rate_limit_middleware,
        ));
    }

    // CORS is outermost so even the limiter's 429 carries the allowed-origin headers.
    if let Some(cors) = cors {
        router = router.layer(cors);
    }

    // Applied last, so it is the outermost layer and every response -- auth 401, rate-limit
    // 429, body-limit 413 and handler 4xx/5xx alike -- carries the hardening headers.
    router = router.layer(axum::middleware::from_fn(security_headers));

    router.with_state(state)
}

/// A CORS layer for `origins`: `"*"` allows any, otherwise the browser's exact `Origin` must match.
fn cors_layer(origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("idempotency-key"),
        ]);
    if origins.iter().any(|origin| origin == "*") {
        return layer.allow_origin(AllowOrigin::any());
    }
    let allowed = origins
        .iter()
        .filter_map(|origin| normalized_origin(origin))
        .filter_map(|origin| HeaderValue::from_str(&origin).ok())
        .collect::<Vec<_>>();
    layer.allow_origin(AllowOrigin::list(allowed))
}

/// Canonicalise a configured origin to the exact `scheme://host[:port]` form browsers send, so a
/// trailing slash or uppercase host still matches.
fn normalized_origin(origin: &str) -> Option<String> {
    reqwest::Url::parse(origin)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

async fn auth(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // The UI's static files are public; only the API behind them is authenticated.
    if ui::is_ui_path(request.uri().path()) {
        return next.run(request).await;
    }
    if let Some(expected) = &state.config.server.bearer_token {
        let presented = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_string);
        let authorized = presented
            .as_deref()
            .is_some_and(|token| constant_time_eq(token, expected));
        if !authorized {
            let body = ApiError::new(ErrorCode::InvalidRequest, "missing or invalid bearer token");
            return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
        }
    }
    next.run(request).await
}

/// Attach the opt-in monitor to successful run creation. Only with monitoring on is the (tiny)
/// response body buffered to read the run id, and a monitoring failure never changes a response.
async fn monitor_runs(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(monitor) = crate::monitoring::current().filter(|monitor| monitor.is_active()) else {
        return next.run(request).await;
    };
    let watch = request.method() == Method::POST && request.uri().path() == "/v1/runs";
    let response = next.run(request).await;
    if !watch || !response.status().is_success() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_MONITORED_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return Response::from_parts(parts, axum::body::Body::empty()),
    };
    if let Ok(created) = serde_json::from_slice::<silver_protocol::RunCreatedResponse>(&bytes) {
        tokio::spawn(crate::monitoring::watch_run(
            monitor,
            state.runs,
            created.run_id,
            created.session_id,
        ));
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// Constant-time comparison for the bearer token: no early return on length or content.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = if a.len() == b.len() { 0u8 } else { 1u8 };
    let len = a.len().max(b.len());
    for index in 0..len {
        let x = a.get(index).copied().unwrap_or(0);
        let y = b.get(index).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Resolve a workspace id from a request into its canonical on-disk root, mapping a bad id to a
/// 400 and an unknown one to a 404.
pub(crate) async fn resolve_workspace_root(
    state: &AppState,
    workspace_id: &str,
) -> Result<std::path::PathBuf, ApiFailure> {
    let id: silver_protocol::WorkspaceId = workspace_id.parse()?;
    let workspace = state
        .db
        .get_workspace(id)
        .await?
        .ok_or(ApiFailure(CoreError::WorkspaceNotFound(id)))?;
    Ok(workspace.canonical_root)
}
