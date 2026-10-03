//! Skill inventory: the global store (`~/.agents/skills`) as a run would see it. A listing is not
//! scoped to a workspace, so project skills are left out.

use super::{ApiFailure, AppState};
use axum::{extract::State, Json};
use serde_json::{json, Value};

/// `GET /v1/skills`: every skill the backend can list, name + description + category.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiFailure> {
    let skills = match state.runs.skills() {
        Some(backend) => backend
            .list_for_platform(None, None)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let items: Vec<Value> = skills
        .iter()
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "category": skill.category,
            })
        })
        .collect();
    Ok(Json(json!({ "skills": items })))
}
