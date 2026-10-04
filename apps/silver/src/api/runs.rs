//! Run endpoints.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use silver_core::error::CoreError;
use silver_protocol::{ApprovalDecisionRequest, CreateRunRequest, RunId, RunView, SteerRequest};

/// Longest accepted Idempotency-Key. Longer values are rejected rather than stored.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateRunRequest>,
) -> Result<Response, ApiFailure> {
    if state.runs.is_paused() {
        return Ok(paused_response());
    }
    let key = idempotency_key(&headers)?;
    let response = state.runs.create_run_idempotent(request, key).await?;
    Ok((StatusCode::ACCEPTED, Json(response)).into_response())
}

/// Read the Idempotency-Key header, trimming surrounding whitespace. An empty value is
/// treated as absent, matching the "no key" behaviour.
fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiFailure> {
    let Some(raw) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let value = raw
        .to_str()
        .map_err(|_not_visible_ascii| {
            ApiFailure(CoreError::InvalidRequest(
                "invalid idempotency-key header".into(),
            ))
        })?
        .trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(ApiFailure(CoreError::InvalidRequest(format!(
            "idempotency-key exceeds {MAX_IDEMPOTENCY_KEY_LEN} bytes"
        ))));
    }
    Ok(Some(value.to_string()))
}

/// The fail-fast body for new work while the emergency stop is engaged. The code is the
/// stable "daemon_paused" string; pausing is a local operational state rather than a
/// protocol domain error, so the ErrorCode enum is deliberately not extended.
pub(crate) fn paused_response() -> Response {
    let body = serde_json::json!({
        "error": {
            "code": "daemon_paused",
            "message": "daemon is paused; new runs are not accepted until it is resumed",
        }
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<RunView>, ApiFailure> {
    let id: RunId = run_id.parse()?;
    Ok(Json(state.runs.get_run_view(id).await?))
}

pub async fn stop(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<RunView>, ApiFailure> {
    let id: RunId = run_id.parse()?;
    Ok(Json(state.runs.stop_run(id).await?))
}

pub async fn steer(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(request): Json<SteerRequest>,
) -> Result<StatusCode, ApiFailure> {
    let id: RunId = run_id.parse()?;
    if request.message.trim().is_empty() {
        return Err(ApiFailure(CoreError::InvalidRequest(
            "steer message is empty".into(),
        )));
    }
    state.runs.steer(id, request.message).await?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn approval(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(request): Json<ApprovalDecisionRequest>,
) -> Result<StatusCode, ApiFailure> {
    let id: RunId = run_id.parse()?;
    state.runs.decide_approval(id, request)?;
    Ok(StatusCode::NO_CONTENT)
}
