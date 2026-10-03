//! Preset endpoints: named tool and skill selections a session runs with.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use silver_core::error::CoreError;
use silver_protocol::Preset;

/// GET /v1/presets: Minimal, Pi, then custom by name.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiFailure> {
    let presets = state.runs.presets().await.map_err(ApiFailure)?;
    Ok(Json(json!({ "presets": presets })))
}

/// POST /v1/presets: create a custom preset.
pub async fn create(
    State(state): State<AppState>,
    Json(request): Json<Preset>,
) -> Result<(StatusCode, Json<Preset>), ApiFailure> {
    let presets = state.runs.presets().await.map_err(ApiFailure)?;
    let name = check_name(&presets, &request.name, None)?;
    let id = format!("preset_{}", uuid::Uuid::now_v7().simple());
    let preset = Preset {
        id,
        name,
        tools: dedupe_tools(request.tools),
        skills: request.skills,
        builtin: false,
    };
    let preset = state.db.save_preset(preset).await?;
    Ok((StatusCode::CREATED, Json(preset)))
}

/// PUT /v1/presets/{id}: replace a custom preset's name, tools and skills.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<Preset>,
) -> Result<Json<Preset>, ApiFailure> {
    let presets = state.runs.presets().await.map_err(ApiFailure)?;
    let existing = editable(&presets, &id)?;
    let name = check_name(&presets, &request.name, Some(&existing.id))?;
    let preset = Preset {
        id,
        name,
        tools: dedupe_tools(request.tools),
        skills: request.skills,
        builtin: false,
    };
    Ok(Json(state.db.save_preset(preset).await?))
}

/// DELETE /v1/presets/{id}: sessions on the preset move to Minimal.
pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let presets = state.runs.presets().await.map_err(ApiFailure)?;
    let existing = editable(&presets, &id)?;
    state.db.delete_preset(&existing.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolve an id to the custom preset it names, refusing built-ins and unknown ids.
fn editable<'a>(presets: &'a [Preset], id: &str) -> Result<&'a Preset, ApiFailure> {
    let Some(preset) = presets.iter().find(|preset| preset.id == id) else {
        return Err(ApiFailure(CoreError::InvalidRequest(format!(
            "unknown preset '{id}'"
        ))));
    };
    if preset.builtin {
        return Err(ApiFailure(CoreError::InvalidRequest(format!(
            "{} is built in and cannot be changed; save a copy under a new name instead",
            preset.name
        ))));
    }
    Ok(preset)
}

/// Trim the name, reject a blank one and a duplicate (built-ins included).
fn check_name(
    presets: &[Preset],
    raw: &str,
    ignore_id: Option<&str>,
) -> Result<String, ApiFailure> {
    let name = raw.trim().to_string();
    if name.is_empty() {
        return Err(ApiFailure(CoreError::InvalidRequest(
            "a preset needs a name".into(),
        )));
    }
    if presets.iter().any(|preset| {
        preset.name.eq_ignore_ascii_case(&name) && Some(preset.id.as_str()) != ignore_id
    }) {
        return Err(ApiFailure(CoreError::Conflict(format!(
            "a preset named \"{name}\" already exists; pick another name"
        ))));
    }
    Ok(name)
}

/// Drop duplicate tool names, keeping the first occurrence. Unknown names are kept:
/// an MCP server may be offline.
fn dedupe_tools(tools: Vec<String>) -> Vec<String> {
    let mut unique = Vec::with_capacity(tools.len());
    for tool in tools {
        if !unique.contains(&tool) {
            unique.push(tool);
        }
    }
    unique
}
