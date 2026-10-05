//! Read-only view of the workspace's ai-memory pages, for the Messages memory panel. silver
//! resolves the folder to an ai-memory project and proxies the server's `/api/v1`; memory is
//! written by the agents through MCP, never from here.

use super::{ApiFailure, AppState};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use silver_core::error::CoreError;
use silver_protocol::{MemoryPageBody, MemoryPageView, MemoryView, WorkspaceId};
use std::path::Path as StdPath;
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

/// A page row from ai-memory's page list, recent list or search.
#[derive(Deserialize)]
struct AiPage {
    path: String,
    title: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct AiPageBody {
    path: String,
    title: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    body_markdown: String,
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
    let (workspace_name, project) = resolve_project(&workspace.canonical_root);
    let query = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let client = client()?;
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
    let (workspace_name, project) = resolve_project(&workspace.canonical_root);
    let path = params.path.trim();
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return Err(CoreError::InvalidRequest("invalid memory page path".into()).into());
    }
    let url =
        format!("{endpoint}/api/v1/workspaces/{workspace_name}/projects/{project}/pages/{path}");
    let response = client()?.get(url).send().await.map_err(upstream)?;
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
    let body: AiPageBody = response.json().await.map_err(upstream)?;
    Ok(Json(MemoryPageBody {
        path: body.path,
        title: body.title,
        kind: body.kind.unwrap_or_else(|| "fact".to_string()),
        body_markdown: body.body_markdown,
    }))
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
    let rows: Vec<AiPage> = response.json().await.map_err(upstream)?;
    Ok(rows
        .into_iter()
        .map(|row| MemoryPageView {
            path: row.path,
            title: row.title,
            kind: row.kind.unwrap_or_else(|| "fact".to_string()),
            updated_at: row.updated_at,
        })
        .collect())
}

fn client() -> Result<reqwest::Client, ApiFailure> {
    reqwest::Client::builder()
        .timeout(MEMORY_TIMEOUT)
        .build()
        .map_err(upstream)
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

/// The `(workspace, project)` ai-memory resolves for this folder: the nearest `.ai-memory.toml`
/// when it names them, else `default` and the folder's name. ai-memory's git-remote identity can
/// name a project differently; the panel then shows an empty list rather than a wrong one.
fn resolve_project(root: &StdPath) -> (String, String) {
    let marker = std::fs::read_to_string(root.join(".ai-memory.toml")).ok();
    let named = |key: &str| {
        marker
            .as_deref()?
            .parse::<toml::Value>()
            .ok()?
            .get(key)?
            .as_str()
            .map(str::to_string)
    };
    let workspace = named("workspace").unwrap_or_else(|| "default".to_string());
    let project = named("project").unwrap_or_else(|| {
        root.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("default")
            .to_string()
    });
    (workspace, project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_resolves_to_its_name_or_the_marker_file() {
        let plain = std::env::temp_dir().join(format!("silver-mem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&plain).unwrap();
        let (workspace, project) = resolve_project(&plain);
        assert_eq!(workspace, "default");
        assert_eq!(project, plain.file_name().unwrap().to_str().unwrap());

        let marked = std::env::temp_dir().join(format!("silver-mem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&marked).unwrap();
        std::fs::write(
            marked.join(".ai-memory.toml"),
            "workspace = \"team\"\nproject = \"silver\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_project(&marked),
            ("team".to_string(), "silver".to_string())
        );
    }
}
