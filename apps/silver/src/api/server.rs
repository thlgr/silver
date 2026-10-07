//! Runtime server settings the web UI reads and changes.

use super::{ApiFailure, AppState};
use axum::{extract::State, Json};
use silver_core::error::CoreError;
use silver_protocol::ServerSettings;

/// GET /v1/server: the settings the web UI can change.
pub async fn get(State(state): State<AppState>) -> Json<ServerSettings> {
    Json(ServerSettings {
        run_timeout_seconds: state.runs.run_timeout_seconds(),
    })
}

/// POST /v1/server: change the run timeout, persisted and applied to future runs.
pub async fn set(
    State(state): State<AppState>,
    Json(request): Json<ServerSettings>,
) -> Result<Json<ServerSettings>, ApiFailure> {
    state
        .runs
        .set_run_timeout_seconds(request.run_timeout_seconds)
        .map_err(|err| ApiFailure(CoreError::InvalidRequest(err.to_string())))?;
    Ok(Json(ServerSettings {
        run_timeout_seconds: state.runs.run_timeout_seconds(),
    }))
}
