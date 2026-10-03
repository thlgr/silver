//! Approval-mode endpoints.

use super::{ApiFailure, AppState};
use axum::{extract::State, Json};
use silver_core::error::CoreError;
use silver_protocol::{ApprovalStatus, SetApprovalModeRequest};

/// GET /v1/approvals: the effective global mode and whether --yolo pinned it.
pub async fn get(State(state): State<AppState>) -> Json<ApprovalStatus> {
    Json(state.runs.approval().status())
}

/// POST /v1/approvals: persist a new global mode. Refused while --yolo pins the mode.
pub async fn set(
    State(state): State<AppState>,
    Json(request): Json<SetApprovalModeRequest>,
) -> Result<Json<ApprovalStatus>, ApiFailure> {
    state
        .runs
        .approval()
        .set_mode(request.mode)
        .map_err(|err| ApiFailure(CoreError::InvalidRequest(err.to_string())))?;
    Ok(Json(state.runs.approval().status()))
}
