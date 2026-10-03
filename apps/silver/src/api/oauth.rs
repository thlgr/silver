//! OAuth device / PKCE login endpoints.

use super::{ApiFailure, AppState};
use crate::oauth::{LoginStart, OAuthError, OAuthManager, TokenStatus};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use silver_core::error::CoreError;

fn manager(state: &AppState) -> Result<&OAuthManager, ApiFailure> {
    state
        .oauth
        .as_deref()
        .ok_or_else(|| ApiFailure(CoreError::InvalidRequest("OAuth is not configured".into())))
}

/// Map an OAuth failure onto the stable API error. Disabled/unknown-provider and missing-grant
/// conditions are caller-visible 400s; transport and store failures stay opaque 500s.
fn map_oauth(error: &OAuthError) -> ApiFailure {
    let message = error.to_string();
    let core = match error {
        OAuthError::Disabled
        | OAuthError::UnknownProvider(_)
        | OAuthError::MissingEndpoint { .. }
        | OAuthError::NoLoginInProgress(_)
        | OAuthError::NoToken(_)
        | OAuthError::NoRefreshToken(_)
        | OAuthError::StateMismatch
        | OAuthError::Callback(_) => CoreError::InvalidRequest(message),
        _ => CoreError::Internal(message),
    };
    ApiFailure(core)
}

fn login_start_json(start: &LoginStart) -> Value {
    match start {
        LoginStart::Device(device) => json!({
            "flow": "device",
            "user_code": device.user_code,
            "verification_uri": device.verification_uri,
            "device_code": device.device_code,
            "interval": device.interval,
            "expires_in": device.expires_in,
        }),
        LoginStart::Pkce(pkce) => json!({
            "flow": "pkce",
            "authorize_url": pkce.authorize_url,
            "redirect_uri": pkce.redirect_uri,
            "state": pkce.state,
        }),
    }
}

fn token_status_json(status: TokenStatus) -> Value {
    match status {
        TokenStatus::Pending { interval } => json!({ "status": "pending", "interval": interval }),
        TokenStatus::SlowDown { interval } => {
            json!({ "status": "slow_down", "interval": interval })
        }
        TokenStatus::Waiting => json!({ "status": "waiting" }),
        TokenStatus::Authorized => json!({ "status": "authorized" }),
        TokenStatus::Denied { code, description } => json!({
            "status": "denied",
            "code": code,
            "description": description,
        }),
        TokenStatus::Expired => json!({ "status": "expired" }),
    }
}

/// `POST /v1/oauth/{provider}/login`: start a device or PKCE sign-in.
pub async fn login(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<Value>, ApiFailure> {
    let start = manager(&state)?
        .begin_login(&provider)
        .await
        .map_err(|err| map_oauth(&err))?;
    Ok(Json(login_start_json(&start)))
}

/// `GET /v1/oauth/{provider}/poll`: advance an in-flight sign-in by one step.
pub async fn poll(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<Value>, ApiFailure> {
    let status = manager(&state)?
        .poll_login(&provider)
        .await
        .map_err(|err| map_oauth(&err))?;
    Ok(Json(token_status_json(status)))
}

/// `GET /v1/oauth/status`: one entry per provider holding a stored grant.
pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiFailure> {
    let manager = manager(&state)?;
    let providers: Vec<Value> = manager
        .list()
        .iter()
        .map(|provider| json!({ "provider": provider, "authorized": true }))
        .collect();
    Ok(Json(json!({ "providers": providers })))
}

/// `POST /v1/oauth/{provider}/logout`: drop the stored grant and any in-flight login.
pub async fn logout(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    manager(&state)?
        .logout(&provider)
        .map_err(|err| map_oauth(&err))?;
    Ok(StatusCode::NO_CONTENT)
}
