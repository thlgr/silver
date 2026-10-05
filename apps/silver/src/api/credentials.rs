//! Provider credential endpoints behind `/login`. A stored key is never echoed back.

use super::{ApiFailure, AppState};
use crate::auth::{AuthError, AuthStore};
use crate::routed::RoutedModel;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use silver_core::error::CoreError;
use silver_protocol::providers::PROVIDER_PRESETS;
use std::borrow::Cow;
use std::sync::Arc;

/// Body of `POST /v1/auth/{provider}`.
#[derive(Debug, Deserialize)]
pub struct SaveCredentialRequest {
    /// The provider's API key. Absent for a provider signed in with OAuth or needing no key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Endpoint override; the preset's base URL is used when absent.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Model override; the preset's default (or the endpoint's catalog) is used when absent.
    #[serde(default)]
    pub model: Option<String>,
    /// Whether new runs should route through this provider right away.
    #[serde(default)]
    pub activate: bool,
}

fn store(state: &AppState) -> Result<&AuthStore, ApiFailure> {
    state.auth.as_deref().ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(
            "sign-in is not configured".into(),
        ))
    })
}

fn routes(state: &AppState) -> Result<&RoutedModel, ApiFailure> {
    state.routes.as_deref().ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(
            "sign-in is not configured".into(),
        ))
    })
}

/// Whether an HTTP route has no endpoint yet: the `custom` preset (or Azure) before the user
/// names one. Bedrock, Vertex and ACP derive theirs, so a blank base URL is fine there.
fn missing_endpoint(route: &crate::routed::Route) -> bool {
    use silver_protocol::providers::ProviderKind::*;
    matches!(route.kind, OpenAiCompatible | Ollama | Anthropic) && route.base_url.trim().is_empty()
}

fn require_endpoint(route: &crate::routed::Route) -> Result<(), ApiFailure> {
    if missing_endpoint(route) {
        return Err(ApiFailure(CoreError::InvalidRequest(format!(
            "{} needs an endpoint URL, such as http://localhost:8080/v1",
            route.provider
        ))));
    }
    Ok(())
}

fn http_client(state: &AppState) -> Option<reqwest::Client> {
    crate::config::http_client_builder(&state.config.security)
        .ok()
        .and_then(|builder| builder.build().ok())
}

/// Refuse to route runs to a keyless server (LM Studio, Ollama, llama.cpp, vLLM, a custom
/// endpoint) that does not answer, so a mistyped URL fails here and not on the first message.
async fn ensure_reachable(
    state: &AppState,
    provider: &str,
    base_url: Option<&str>,
) -> Result<(), ApiFailure> {
    use silver_protocol::providers::ProviderKind::{Ollama, OpenAiCompatible};
    let keyless_server = silver_protocol::providers::preset(provider).is_some_and(|preset| {
        !preset.requires_key && matches!(preset.kind, OpenAiCompatible | Ollama)
    });
    let url = base_url.map(str::trim).unwrap_or_default();
    if !keyless_server || url.is_empty() {
        return Ok(());
    }
    let answers = match http_client(state) {
        Some(http) => crate::provider::endpoint_reachable(&http, url).await,
        None => true,
    };
    if answers {
        return Ok(());
    }
    Err(ApiFailure(CoreError::InvalidRequest(format!(
        "Couldn't reach {url}: start the server or fix the URL"
    ))))
}

/// Map a credential-store failure onto the stable API error. A bad provider id or a blank key
/// is the caller's fault; a store write failure is not.
fn map_auth(error: &AuthError) -> ApiFailure {
    let message = error.to_string();
    let core = match error {
        AuthError::UnknownProvider(_) | AuthError::MissingCredential(_) => {
            CoreError::InvalidRequest(message)
        }
        AuthError::Io { .. } | AuthError::Serialize(_) => CoreError::Internal(message),
    };
    ApiFailure(core)
}

/// `GET /v1/auth`: every provider with its endpoint, model and sign-in state.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiFailure> {
    // Which agent modes are installed is read from the login-shell PATH; hydrate it once here.
    crate::agent_modes::ensure_path().await;
    let store = store(&state)?;
    let routes = routes(&state)?;
    let active = store.active();
    let providers: Vec<Value> = PROVIDER_PRESETS
        .iter()
        .map(|preset| {
            let route = routes.describe(preset.id);
            let authenticated = route
                .as_ref()
                .is_some_and(|route| route.authenticated() && !missing_endpoint(route));
            json!({
                "id": preset.id,
                "label": preset.label,
                "kind": preset.kind.as_str(),
                "base_url": route.as_ref().map(|route| &route.base_url),
                "model": route.as_ref().map(|route| &route.model),
                "requires_key": preset.requires_key,
                "oauth": preset.oauth,
                "signup_url": preset.signup_url,
                "api_key_env": preset.api_key_env,
                "key_source": route.as_ref().map(|route| route.source.as_str()),
                "authenticated": authenticated,
                // An agent mode whose CLI is installed on this machine; null for any other preset.
                "installed": crate::agent_modes::installed(preset.id),
                "active": active.as_deref() == Some(preset.id),
                // Set up by the user: saved here, active, or a key found in the environment. A
                // keyless preset nobody touched is not, though it could answer.
                "configured": store.credential(preset.id).is_some()
                    || active.as_deref() == Some(preset.id)
                    || (preset.requires_key && authenticated),
            })
        })
        .collect();
    Ok(Json(json!({ "active": active, "providers": providers })))
}

/// `POST /v1/auth/{provider}`: store a credential and optionally route runs through it. Activated
/// without a model, the endpoint's own `/models` picks one so the next run has a model to name.
pub async fn save(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(request): Json<SaveCredentialRequest>,
) -> Result<Json<Value>, ApiFailure> {
    let store = store(&state)?;
    let routes = routes(&state)?;
    if request.activate {
        let endpoint = request
            .base_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .map(Cow::Borrowed)
            .or_else(|| {
                routes
                    .describe(&provider)
                    .map(|route| Cow::Owned(route.base_url))
            });
        ensure_reachable(&state, &provider, endpoint.as_deref()).await?;
    }
    // Nothing to store just activates: a keyless local server works as its preset stands.
    let given = |field: &Option<String>| field.as_deref().is_some_and(|v| !v.trim().is_empty());
    if given(&request.api_key) || given(&request.base_url) || given(&request.model) {
        store
            .set_credential(
                &provider,
                request.api_key.as_deref(),
                request.base_url.as_deref(),
                request.model.as_deref(),
            )
            .map_err(|err| map_auth(&err))?;
    }

    let mut route = routes.resolve(&provider).await?;
    require_endpoint(&route)?;
    if !route.authenticated() {
        return Err(ApiFailure(CoreError::InvalidRequest(format!(
            "{provider} still has no credential; pass an api_key or sign in with OAuth"
        ))));
    }
    let picked = if route.model.trim().is_empty() {
        first_catalog_model(&state, &route).await
    } else if !given(&request.model) {
        unlisted_local_model(&state, &route).await
    } else {
        None
    };
    if let Some(model) = picked {
        store
            .set_credential(&provider, None, None, Some(&model))
            .map_err(|err| map_auth(&err))?;
        route.model = model;
    }
    if request.activate {
        store.activate(&provider).map_err(|err| map_auth(&err))?;
    }
    Ok(Json(json!({
        "provider": provider,
        "base_url": route.base_url,
        "model": route.model,
        "key_source": route.source.as_str(),
        "active": store.active().as_deref() == Some(provider.as_str()),
    })))
}

/// `POST /v1/auth/{provider}/activate`: route new runs through an already-signed-in provider.
pub async fn activate(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<Value>, ApiFailure> {
    let store = store(&state)?;
    let routes = routes(&state)?;
    let route = routes.resolve(&provider).await?;
    if !route.authenticated() {
        return Err(ApiFailure(CoreError::InvalidRequest(format!(
            "{provider} is not signed in; store a key or run an OAuth login first"
        ))));
    }
    require_endpoint(&route)?;
    ensure_reachable(&state, &provider, Some(&route.base_url)).await?;
    store.activate(&provider).map_err(|err| map_auth(&err))?;
    let model =
        crate::context_length::model_served_for(&local_details(&state, &route).await, &route.model)
            .unwrap_or(route.model);
    Ok(Json(json!({
        "provider": provider,
        "base_url": route.base_url,
        "model": model,
        "key_source": route.source.as_str(),
        "active": true,
    })))
}

/// `DELETE /v1/auth/{provider}`: forget a stored credential.
///
/// The OAuth grant, if any, is untouched: `POST /v1/oauth/{provider}/logout` owns that.
pub async fn forget(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    store(&state)?
        .forget(&provider)
        .map_err(|err| map_auth(&err))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Every model the provider's endpoint advertises. Copilot's catalog rejects the stored GitHub
/// token, so it is asked with a minted bearer.
async fn catalog_models(state: &AppState, route: &crate::routed::Route) -> Vec<String> {
    let Some(http) = http_client(state) else {
        return Vec::new();
    };
    let (key, base_url) = match route.kind {
        silver_protocol::providers::ProviderKind::Copilot => {
            crate::copilot::CopilotProvider::new(
                String::clone(&route.base_url),
                route.key().to_string(),
                String::new(),
            )
            .with_http_client(reqwest::Client::clone(&http))
            .credentials()
            .await
        }
        _ => (route.key().to_string(), String::clone(&route.base_url)),
    };
    crate::provider::list_models(&http, &base_url, &key, route.kind).await
}

/// What a local server's native API says about its models (purpose, loaded, window), which the
/// OpenAI-compatible `/models` omits. Empty for a hosted route.
async fn local_details(
    state: &AppState,
    route: &crate::routed::Route,
) -> Vec<crate::context_length::LocalModelInfo> {
    if !route.authenticated() || !crate::context_length::is_local_base_url(&route.base_url) {
        return Vec::new();
    }
    match http_client(state) {
        Some(http) => crate::context_length::probe_local_models(&http, &route.base_url).await,
        None => Vec::new(),
    }
}

/// The first model the provider's endpoint advertises, when it serves a catalog.
async fn first_catalog_model(state: &AppState, route: &crate::routed::Route) -> Option<String> {
    catalog_models(state, route).await.into_iter().next()
}

/// What a local server serves in place of a model it does not list (LM Studio's `local-model`
/// placeholder, a stale preset id): its loaded model, else its first. None when it is listed.
async fn unlisted_local_model(state: &AppState, route: &crate::routed::Route) -> Option<String> {
    if !crate::context_length::is_local_base_url(&route.base_url) {
        return None;
    }
    let catalog = catalog_models(state, route).await;
    if catalog.is_empty() || catalog.contains(&route.model) {
        return None;
    }
    crate::context_length::model_served_for(&local_details(state, route).await, &route.model)
        .or_else(|| catalog.into_iter().next())
}

/// Per-model supported reasoning-effort sets from the models.dev registry, for `ids` plus the
/// default; a model the registry does not know falls back to the global accepted set.
fn model_efforts(
    registry: Option<&Value>,
    provider: &str,
    ids: &[String],
    default: &str,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let mut seen: Vec<String> = Vec::new();
    let mut insert = |id: &str| {
        if !id.is_empty() && !seen.iter().any(|s| s == id) {
            seen.push(id.to_string());
            out.insert(
                id.to_string(),
                json!(crate::context_length::supported_reasoning_efforts(
                    registry,
                    Some(provider),
                    id
                )),
            );
        }
    };
    for id in ids {
        insert(id);
    }
    insert(default);
    out
}

/// Query of `GET /v1/models`.
#[derive(Debug, Deserialize)]
pub struct ModelsQuery {
    /// Provider to ask; the active route (or the configured provider) when absent.
    #[serde(default)]
    pub provider: Option<String>,
}

/// `GET /v1/models`: whatever the endpoint's own `/models` returns (empty when it serves none),
/// plus `default`, the model a run uses when it names none.
pub async fn models(
    State(state): State<AppState>,
    Query(query): Query<ModelsQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let store = store(&state)?;
    let routes = routes(&state)?;
    let provider = query
        .provider
        .map(|provider| provider.trim().to_ascii_lowercase())
        .filter(|provider| !provider.is_empty())
        .or_else(|| store.active())
        .unwrap_or_else(|| state.config.model.provider.trim().to_ascii_lowercase());
    if provider.is_empty() {
        let registry = registry(&state).await;
        let default = state.config.resolved_model();
        let efforts = model_efforts(registry.as_deref(), "", &[], default);
        return Ok(Json(json!({
            "provider": "",
            "default": default,
            "authenticated": false,
            "models": Vec::<String>::new(),
            "efforts": efforts,
            "effort": state.config.resolved_reasoning_effort(),
        })));
    }
    let route = routes.resolve(&provider).await?;
    let authenticated = route.authenticated();
    let models = if authenticated {
        catalog_models(&state, &route).await
    } else {
        Vec::new()
    };
    let details = local_details(&state, &route).await;
    // The model a run will name, as create_run resolves it: a local server's placeholder
    // shows as the model the server has loaded.
    let default =
        crate::context_length::model_served_for(&details, &route.model).unwrap_or(route.model);
    let registry = registry(&state).await;
    let efforts = model_efforts(registry.as_deref(), &provider, &models, &default);
    Ok(Json(json!({
        "provider": provider,
        "default": default,
        "authenticated": authenticated,
        "models": models,
        "details": details,
        "efforts": efforts,
        "effort": state.config.resolved_reasoning_effort(),
    })))
}

/// The models.dev registry, ready for a model's supported-effort lookup. None when the daemon
/// has no resolver.
async fn registry(state: &AppState) -> Option<Arc<Value>> {
    match &state.context_resolver {
        Some(resolver) => resolver.registry().await,
        None => None,
    }
}
