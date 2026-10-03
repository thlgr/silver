//! Git working-tree diff and worktree endpoints.

use super::{resolve_workspace_root, ApiFailure, AppState};
use crate::git_extras::{self, DiffScope};
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use silver_core::error::CoreError;

/// Map a git-layer failure onto the stable API error. Git failures are caller-visible (not a
/// repository, unknown name, ...), so they surface as a 400 rather than an opaque 500.
fn git_failure(error: &anyhow::Error) -> ApiFailure {
    ApiFailure(CoreError::InvalidRequest(error.to_string()))
}

fn join_failure(error: &tokio::task::JoinError) -> ApiFailure {
    ApiFailure(CoreError::Internal(format!("git task failed: {error}")))
}

// ── Diff ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DiffQuery {
    workspace_id: String,
    /// working | staged | all | session. Defaults to working.
    #[serde(default)]
    scope: Option<String>,
    /// Return only the stat block when true.
    #[serde(default)]
    stat: bool,
    /// Comma-separated pathspecs to narrow the diff.
    #[serde(default)]
    path: Option<String>,
}

/// `GET /v1/diff?workspace_id=...&scope=...&stat=bool&path=...`
pub async fn diff(
    State(state): State<AppState>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let workspace_root = resolve_workspace_root(&state, &query.workspace_id).await?;
    let scope = match query.scope.as_deref() {
        Some(value) => DiffScope::parse(value).ok_or_else(|| {
            ApiFailure(CoreError::InvalidRequest(format!(
                "unknown diff scope {value:?}; expected working, staged, all or session"
            )))
        })?,
        None => DiffScope::Working,
    };
    let paths = split_paths(query.path.as_deref());
    let stat = query.stat;

    let (result, text) = tokio::task::spawn_blocking(move || {
        let result = git_extras::collect_diff(&workspace_root, scope, &paths);
        let text = git_extras::render_diff(&result, scope, stat);
        (result, text)
    })
    .await
    .map_err(|err| join_failure(&err))?;

    Ok(Json(json!({
        "scope": scope.as_str(),
        // Whether the caller asked for stat-only; the rendered stat block itself is "stat" below.
        "stat_only": stat,
        "success": result.success,
        "empty": result.empty,
        "error": result.error,
        "untracked": result.untracked,
        "stat": result.stat,
        "diff": result.diff,
        "text": text,
    })))
}

/// Split a comma-separated pathspec string, dropping blanks.
fn split_paths(raw: Option<&str>) -> Vec<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

// ── Worktrees ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct WorkspaceQuery {
    workspace_id: String,
}

fn worktree_entry_json(entry: &git_extras::WorktreeEntry) -> Value {
    let name = entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "name": name,
        "path": entry.path.to_string_lossy(),
        "head": entry.head,
        "branch": entry.branch,
        "detached": entry.detached,
        "bare": entry.bare,
        "locked": entry.locked,
        "lock_reason": entry.lock_reason,
    })
}

/// `GET /v1/worktrees?workspace_id=...`: every worktree registered on the workspace repo.
pub async fn worktree_list(
    State(state): State<AppState>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let workspace_root = resolve_workspace_root(&state, &query.workspace_id).await?;
    let entries = tokio::task::spawn_blocking(move || git_extras::worktree_list(&workspace_root))
        .await
        .map_err(|err| join_failure(&err))?
        .map_err(|err| git_failure(&err))?;
    let worktrees: Vec<Value> = entries.iter().map(worktree_entry_json).collect();
    Ok(Json(json!({
        "workspace_id": query.workspace_id,
        "worktrees": worktrees,
    })))
}

#[derive(Deserialize)]
pub struct CreateWorktreeRequest {
    workspace_id: String,
    /// Requested name; a random silver-<hex> name is used when omitted.
    #[serde(default)]
    name: Option<String>,
    /// Branch from the freshly fetched remote tip.
    #[serde(default)]
    sync: bool,
}

/// `POST /v1/worktrees` with `{workspace_id, name?, sync?}`.
pub async fn worktree_create(
    State(state): State<AppState>,
    Json(request): Json<CreateWorktreeRequest>,
) -> Result<Json<Value>, ApiFailure> {
    let workspace_root = resolve_workspace_root(&state, &request.workspace_id).await?;
    let info = tokio::task::spawn_blocking(move || {
        git_extras::worktree_create(
            &state.config,
            &workspace_root,
            request.name.as_deref(),
            request.sync,
        )
    })
    .await
    .map_err(|err| join_failure(&err))?
    .map_err(|err| git_failure(&err))?;

    Ok(Json(json!({
        "path": info.path.to_string_lossy(),
        "branch": info.branch,
        "repo_root": info.repo_root.to_string_lossy(),
        "base": info.base,
        "base_label": info.base_label,
    })))
}

#[derive(Deserialize)]
pub struct RemoveWorktreeQuery {
    workspace_id: String,
    #[serde(default)]
    force: bool,
}

/// `DELETE /v1/worktrees/{name}?workspace_id=...&force=bool`.
pub async fn worktree_remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<RemoveWorktreeQuery>,
) -> Result<Json<Value>, ApiFailure> {
    let workspace_root = resolve_workspace_root(&state, &query.workspace_id).await?;
    let removed = tokio::task::spawn_blocking(move || {
        git_extras::worktree_remove(&state.config, &workspace_root, &name, query.force)
    })
    .await
    .map_err(|err| join_failure(&err))?
    .map_err(|err| git_failure(&err))?;

    Ok(Json(json!({
        "path": removed.path.to_string_lossy(),
        "branch": removed.branch,
        "removed": removed.removed,
        "forced": removed.forced,
    })))
}
