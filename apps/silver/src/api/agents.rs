//! Subagent definition endpoints. Definitions are Markdown files; a built-in has no file, so it can
//! be read, and writing it is refused rather than shadowed.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use silver_core::error::CoreError;
use silver_core::subagent::AgentDefinition;
use silver_protocol::WorkspaceId;
use std::path::PathBuf;

fn store(state: &AppState) -> Result<&crate::subagents::AgentStore, ApiFailure> {
    state.agents.as_deref().ok_or_else(|| {
        ApiFailure(CoreError::InvalidRequest(
            "subagents are not available in this daemon".into(),
        ))
    })
}

/// The workspace whose `.silver/agents` a request reads or writes, or None for the global one.
async fn project_root(
    state: &AppState,
    workspace_id: Option<String>,
) -> Result<Option<PathBuf>, ApiFailure> {
    let Some(id) = workspace_id else {
        return Ok(None);
    };
    let id: WorkspaceId = id.parse()?;
    let workspace = state
        .db
        .get_workspace(id)
        .await?
        .ok_or(CoreError::WorkspaceNotFound(id))?;
    Ok(Some(workspace.canonical_root))
}

fn agent_json(agent: &AgentDefinition) -> Value {
    json!({
        "name": agent.name,
        "description": agent.description,
        "tools": agent.tools,
        "disallowed_tools": agent.disallowed_tools,
        "model": agent.model,
        "max_turns": agent.max_turns,
        "isolation": agent.isolation.map(|value| value.as_str()),
        "source": agent.source.as_str(),
        "editable": agent.source.editable(),
        "path": agent.path.as_ref().map(|path| path.display().to_string()),
        "body": agent.body,
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    workspace_id: Option<String>,
}

/// GET /v1/agents: every definition this workspace can delegate to, built-ins first.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let root = project_root(&state, query.workspace_id).await?;
    let agents = store(&state)?.list(root.as_deref());
    Ok(Json(
        json!({ "agents": agents.iter().map(agent_json).collect::<Vec<_>>() }),
    ))
}

/// GET /v1/agents/{name}: the raw Markdown, for the editor. A built-in renders from its own
/// definition, so its body is returned as it would be written.
pub async fn get_one(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let store = store(&state)?;
    let root = project_root(&state, query.workspace_id).await?;
    let agent = store
        .find(root.as_deref(), &name)
        .ok_or_else(|| CoreError::InvalidRequest(format!("no subagent named '{name}'")))?;
    let markdown = store
        .read_raw(root.as_deref(), &name)
        .unwrap_or_else(|| silver_core::subagent::render_definition(&agent));
    Ok(Json(
        json!({ "agent": agent_json(&agent), "markdown": markdown }),
    ))
}

/// The document to write, and where.
#[derive(Deserialize)]
pub struct WriteRequest {
    /// The Markdown document, frontmatter included.
    markdown: String,
}

/// Where a definition lives: `global` (the default) or `project`, which needs a workspace.
#[derive(Deserialize, Default)]
pub struct WriteQuery {
    /// Required by POST, which has no path to carry the name.
    name: Option<String>,
    #[serde(default)]
    scope: String,
    workspace_id: Option<String>,
}

async fn directory(state: &AppState, query: &WriteQuery) -> Result<PathBuf, ApiFailure> {
    let store = store(state)?;
    match query.scope.as_str() {
        "global" | "" => Ok(store.global_root().to_path_buf()),
        "project" => {
            let root = project_root(state, Option::clone(&query.workspace_id))
                .await?
                .ok_or_else(|| {
                    ApiFailure(CoreError::InvalidRequest(
                        "a project agent needs workspace_id".into(),
                    ))
                })?;
            Ok(crate::subagents::AgentStore::project_dir(&root))
        }
        other => Err(ApiFailure(CoreError::InvalidRequest(format!(
            "unknown scope {other:?}: use global or project"
        )))),
    }
}

/// POST /v1/agents?name=...: write a definition. The name is also the frontmatter's, and the
/// document is parsed before anything touches the disk.
pub async fn create(
    State(state): State<AppState>,
    Query(query): Query<WriteQuery>,
    Json(request): Json<WriteRequest>,
) -> Result<(StatusCode, Json<Value>), ApiFailure> {
    let name = query
        .name
        .as_deref()
        .ok_or_else(|| ApiFailure(CoreError::InvalidRequest("name is required".into())))?;
    let dir = directory(&state, &query).await?;
    let agent = store(&state)?.write(&dir, name, &request.markdown)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "agent": agent_json(&agent) })),
    ))
}

/// PUT /v1/agents/{name}: replace a custom definition.
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<WriteQuery>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<Value>, ApiFailure> {
    let dir = directory(&state, &query).await?;
    let agent = store(&state)?.write(&dir, &name, &request.markdown)?;
    Ok(Json(json!({ "agent": agent_json(&agent) })))
}

/// DELETE /v1/agents/{name}?scope=global|project&workspace_id=...
pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<WriteQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let dir = directory(&state, &query).await?;
    store(&state)?.delete(&dir, &name)?;
    Ok(Json(json!({ "deleted": name })))
}
