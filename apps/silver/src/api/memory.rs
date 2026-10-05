//! Read-only view of the workspace's ai-memory pages, for the Messages memory panel. silver
//! resolves the folder to an ai-memory project and proxies the server's `/api/v1`; memory is
//! written by the agents through MCP, never from here.

use super::{ApiFailure, AppState};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use silver_core::error::CoreError;
use silver_protocol::{MemoryPageBody, MemoryPageView, MemoryView, WorkspaceId};
use std::time::Duration;

/// Per-request budget for the ai-memory server.
const MEMORY_TIMEOUT: Duration = Duration::from_secs(10);
/// How many pages the panel lists at once.
const MEMORY_LIMIT: &str = "60";

#[derive(Deserialize, Default)]
pub struct ListParams {
    q: Option<String>,
}

#[derive(Deserialize)]
pub struct PageParams {
    path: String,
}

/// `GET /v1/workspaces/{id}/memory` — the workspace's recent pages, or search hits for `q`.
pub async fn view(
    State(state): State<AppState>,
    Path(workspace_id): Path<WorkspaceId>,
    Query(params): Query<ListParams>,
) -> Result<Json<MemoryView>, ApiFailure> {
    let endpoint = state.memory_endpoint.as_deref().ok_or_else(memory_off)?;
    let workspace = state
        .db
        .get_workspace(workspace_id)
        .await?
        .ok_or(CoreError::WorkspaceNotFound(workspace_id))?;
    let (workspace_name, project) = crate::ai_memory::resolve_project(&workspace.canonical_root);
    let query = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let client = client();
    let request = match query {
        Some(text) => client.get(format!("{endpoint}/api/v1/search")).query(&[
            ("q", text),
            ("workspace", workspace_name.as_str()),
            ("project", project.as_str()),
            ("limit", MEMORY_LIMIT),
        ]),
        None => client
            .get(format!(
                "{endpoint}/api/v1/workspaces/{workspace_name}/projects/{project}/recent"
            ))
            .query(&[("limit", MEMORY_LIMIT)]),
    };
    let pages = fetch_pages(request).await?;
    Ok(Json(MemoryView {
        workspace: workspace_name,
        project,
        pages,
    }))
}

/// `GET /v1/workspaces/{id}/memory/page?path=` — one page with its Markdown body.
pub async fn page(
    State(state): State<AppState>,
    Path(workspace_id): Path<WorkspaceId>,
    Query(params): Query<PageParams>,
) -> Result<Json<MemoryPageBody>, ApiFailure> {
    let endpoint = state.memory_endpoint.as_deref().ok_or_else(memory_off)?;
    let workspace = state
        .db
        .get_workspace(workspace_id)
        .await?
        .ok_or(CoreError::WorkspaceNotFound(workspace_id))?;
    let (workspace_name, project) = crate::ai_memory::resolve_project(&workspace.canonical_root);
    let path = params.path.trim();
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return Err(CoreError::InvalidRequest("invalid memory page path".into()).into());
    }
    let url =
        format!("{endpoint}/api/v1/workspaces/{workspace_name}/projects/{project}/pages/{path}");
    let response = client().get(url).send().await.map_err(upstream)?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(
            CoreError::ProviderUnavailable("That memory page no longer exists.".into()).into(),
        );
    }
    if !response.status().is_success() {
        return Err(upstream(format!(
            "ai-memory answered {}",
            response.status()
        )));
    }
    let body: MemoryPageBody = response.json().await.map_err(upstream)?;
    Ok(Json(body))
}

/// Send one list request; a project ai-memory does not know yet is an empty list, not an error.
async fn fetch_pages(request: reqwest::RequestBuilder) -> Result<Vec<MemoryPageView>, ApiFailure> {
    let response = request.send().await.map_err(upstream)?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => return Ok(Vec::new()),
        status if !status.is_success() => {
            return Err(upstream(format!("ai-memory answered {status}")));
        }
        _ => {}
    }
    let rows: Vec<MemoryPageView> = response.json().await.map_err(upstream)?;
    Ok(rows)
}

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(MEMORY_TIMEOUT)
            .build()
            .expect("the reqwest client builds")
    })
}

fn memory_off() -> ApiFailure {
    CoreError::ProviderUnavailable(
        "Memory is off: install ai-memory, or point [memory] binary at it, and restart silver."
            .into(),
    )
    .into()
}

fn upstream(error: impl std::fmt::Display) -> ApiFailure {
    CoreError::ProviderUnavailable(format!("ai-memory is not answering: {error}")).into()
}
