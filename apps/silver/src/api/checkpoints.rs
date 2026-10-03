//! Checkpoint inventory and restore endpoints.

use super::{resolve_workspace_root, ApiFailure, AppState};
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use silver_core::error::CoreError;
use silver_protocol::SessionId;

#[derive(Deserialize)]
pub struct ListQuery {
    session_id: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /v1/checkpoints?session_id=...&limit=...`: the newest checkpoint rows for one session,
/// as a bare array of the serde-ready Checkpoint rows.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<crate::db::Checkpoint>>, ApiFailure> {
    let session_id: SessionId = query.session_id.parse()?;
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    let rows = state.db.list_checkpoints(session_id, limit).await?;
    Ok(Json(rows))
}

/// `POST /v1/checkpoints/{id}/restore`: restore one snapshot into its workspace and report the
/// absolute path that was rewritten.
pub async fn restore(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiFailure> {
    let store = state.checkpoints.as_ref().ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(
            "checkpoints are disabled in config".into(),
        ))
    })?;
    let checkpoint = state.db.get_checkpoint(&id).await?.ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(format!(
            "checkpoint {id} not found"
        )))
    })?;

    // Always the checkpoint's own workspace: restoring into another would carry one
    // project's file into a different one.
    let session = state
        .db
        .get_session(checkpoint.session_id)
        .await?
        .ok_or(ApiFailure(CoreError::SessionNotFound(
            checkpoint.session_id,
        )))?;
    let workspace_id = session.workspace_id.ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(
            "checkpoint session has no workspace".into(),
        ))
    })?;
    let workspace_root = resolve_workspace_root(&state, &workspace_id.to_string()).await?;

    store.restore(&checkpoint, &workspace_root)?;
    let restored_path = workspace_root.join(&checkpoint.path);
    Ok(Json(json!({
        "checkpoint_id": checkpoint.id,
        "session_id": checkpoint.session_id,
        "workspace_root": workspace_root.to_string_lossy(),
        "path": restored_path.to_string_lossy(),
        "restored": true,
    })))
}
