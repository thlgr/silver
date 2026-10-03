//! Emergency-stop endpoints. Pausing holds new runs only: a persistent `data_dir/ESTOP` sentinel is
//! read at admission, runs in flight drain normally, and a restart starts paused until resume.

use super::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};

/// Engage the emergency stop and persist it. Idempotent.
pub async fn pause(State(state): State<AppState>) -> Json<Value> {
    state.runs.pause();
    Json(json!({ "paused": true }))
}

/// Release the emergency stop and remove the sentinel. Idempotent.
pub async fn resume(State(state): State<AppState>) -> Json<Value> {
    state.runs.resume();
    Json(json!({ "paused": false }))
}

/// Report whether new work is currently held.
pub async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "paused": state.runs.is_paused() }))
}
