//! Jev advisor endpoints: its switch and its questions.

use super::{ApiFailure, AppState};
use crate::advisor::JevAdvisor;
use axum::{extract::State, Json};
use silver_core::error::CoreError;
use silver_protocol::{AdvisorStatus, SetAdvisorRequest};

fn advisor(state: &AppState) -> Result<&JevAdvisor, ApiFailure> {
    state.advisor.as_deref().ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(
            "the advisor is not configured".into(),
        ))
    })
}

/// GET /v1/advisor: whether Jev hints are on, whether a key is available, and the questions.
pub async fn get(State(state): State<AppState>) -> Result<Json<AdvisorStatus>, ApiFailure> {
    Ok(Json(advisor(&state)?.status()))
}

/// POST /v1/advisor: switch Jev hints for the next check of every run; saved to config.toml.
pub async fn set(
    State(state): State<AppState>,
    Json(request): Json<SetAdvisorRequest>,
) -> Result<Json<AdvisorStatus>, ApiFailure> {
    let advisor = advisor(&state)?;
    advisor
        .set_enabled(request.enabled)
        .map_err(|err| ApiFailure(CoreError::Internal(format!("save agent.jev_hints: {err}"))))?;
    Ok(Json(advisor.status()))
}
