//! Workspace endpoints.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use silver_core::workspace::{confine_path, Workspace};
use silver_protocol::{CreateWorkspaceRequest, Scope, WorkspaceId, WorkspaceView};
use std::path::PathBuf;

pub async fn create(
    State(state): State<AppState>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceView>), ApiFailure> {
    if request.name.trim().is_empty() {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(
            "workspace name is required".into(),
        )));
    }
    // "~/code/app" is how people type a folder; a relative path would silently resolve
    // against wherever the daemon was started.
    let raw = request.path.trim();
    let requested = match (raw.strip_prefix('~'), silver_core::dirs::home_dir()) {
        (Some(rest), Some(home))
            if rest.is_empty() || rest.starts_with(std::path::is_separator) =>
        {
            home.join(rest.trim_start_matches(std::path::is_separator))
        }
        _ => PathBuf::from(raw),
    };
    if !requested.is_absolute() {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(format!(
            "{raw} is a relative path; give the absolute path of the project folder, such as /home/you/{raw}"
        ))));
    }
    // A missing folder is the caller's typo here, not an unavailable workspace.
    let canonical_root = Workspace::canonicalize_root(&requested).map_err(|error| match error {
        silver_core::CoreError::WorkspaceUnavailable(reason) => {
            ApiFailure(silver_core::CoreError::InvalidRequest(reason))
        }
        other => ApiFailure(other),
    })?;
    let now = chrono::Utc::now();
    let workspace = Workspace {
        id: WorkspaceId::new(),
        name: request.name.trim().to_string(),
        root: requested,
        canonical_root,
        created_at: now,
        updated_at: now,
    };
    let workspace = state.db.create_workspace(workspace).await?;
    drop(std::fs::create_dir_all(
        state.memory.scope_dir(&Scope::Workspace(workspace.id)),
    ));
    Ok((StatusCode::CREATED, Json(workspace_view(workspace))))
}

/// `POST /v1/workspaces/pick`: a browser never reveals a folder's absolute path, so silver opens
/// the folder dialog of the desktop it runs on. `path` is null when the user cancels.
pub async fn pick() -> Result<Json<serde_json::Value>, ApiFailure> {
    let (program, args) = folder_dialog()
        .map_err(|reason| ApiFailure(silver_core::CoreError::InvalidRequest(reason.into())))?;
    let output = tokio::process::Command::new(&program)
        .args(&args)
        .output()
        .await
        .map_err(|error| {
            ApiFailure(silver_core::CoreError::Internal(format!(
                "{}: {error}",
                program.display()
            )))
        })?;
    // kdialog, zenity, osascript and the PowerShell script all exit 1 on cancel.
    if output.status.code() == Some(1) {
        return Ok(Json(serde_json::json!({ "path": null })));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiFailure(silver_core::CoreError::Internal(format!(
            "the folder dialog failed: {}",
            stderr.trim().lines().last().unwrap_or("no output")
        ))));
    }
    let chosen = String::from_utf8_lossy(&output.stdout);
    let chosen = chosen.trim_end_matches(['\r', '\n']);
    let path = match chosen.trim_end_matches('/') {
        "" if chosen.starts_with('/') => "/",
        trimmed => trimmed,
    };
    Ok(Json(
        serde_json::json!({ "path": (!path.is_empty()).then_some(path) }),
    ))
}

const WINDOWS_FOLDER_DIALOG: &str = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
    Add-Type -AssemblyName System.Windows.Forms; \
    $dialog = New-Object System.Windows.Forms.FolderBrowserDialog; \
    if ($dialog.ShowDialog() -eq 'OK') { $dialog.SelectedPath } else { exit 1 }";

/// The platform's folder dialog as a program and its arguments.
fn folder_dialog() -> Result<(PathBuf, Vec<String>), &'static str> {
    if cfg!(target_os = "macos") {
        let script = "POSIX path of (choose folder)";
        return Ok(("osascript".into(), vec!["-e".into(), script.into()]));
    }
    if cfg!(windows) {
        let args = ["-NoProfile", "-STA", "-Command", WINDOWS_FOLDER_DIALOG];
        return Ok(("powershell".into(), args.map(String::from).into()));
    }
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(
            "silver runs without a desktop here, so it can't open a folder dialog; type the path.",
        );
    }
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| desktop.contains("KDE"));
    let order: &[&str] = if kde {
        &["kdialog", "zenity"]
    } else {
        &["zenity", "kdialog"]
    };
    let program =
        silver_core::lsp::servers::find_binary(order, std::env::var_os("PATH").as_deref()).ok_or(
            "No folder dialog on this machine: install zenity or kdialog, or type the path.",
        )?;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let args = if program.ends_with("kdialog") {
        vec!["--getexistingdirectory".into(), home]
    } else {
        vec![
            "--file-selection".into(),
            "--directory".into(),
            format!("--filename={home}/"),
        ]
    };
    Ok((program, args))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<WorkspaceView>>, ApiFailure> {
    let workspaces = state.db.list_workspaces().await?;
    Ok(Json(workspaces.into_iter().map(workspace_view).collect()))
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<WorkspaceView>, ApiFailure> {
    let id: WorkspaceId = workspace_id.parse()?;
    let workspace = state
        .db
        .get_workspace(id)
        .await?
        .ok_or(ApiFailure(silver_core::CoreError::WorkspaceNotFound(id)))?;
    Ok(Json(workspace_view(workspace)))
}

#[derive(Deserialize)]
pub struct FileQuery {
    path: String,
}

/// The content type each served extension is returned as. The header decides what the browser
/// may do with the bytes (`nosniff` is applied to every response), so it is set from the name
/// and anything that is not a picture is refused: this renders a loaded picture, nothing more.
fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Where an attached file lands, inside the workspace so the tools can reach it. The same
/// convention `.silver/agents` uses.
const ATTACHMENT_DIR: &str = ".silver/attachments";

/// Binary extensions an attachment may carry: the pictures view_image reads and the documents
/// read_file extracts. Any other file must be UTF-8 text, which read_file reads as it is.
const ATTACHMENT_TYPES: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "pdf"];

/// Longest accepted file name; a browser sends whatever the filesystem calls it.
const MAX_ATTACHMENT_NAME: usize = 120;

#[derive(Deserialize)]
pub struct AttachmentRequest {
    /// File name, used for the saved name and reported back. Only its last path component
    /// is used, so a name can never escape the attachment directory.
    name: String,
    /// Base64 file bytes, with no data-URL prefix.
    data: String,
}

#[derive(serde::Serialize)]
pub struct AttachmentView {
    /// Workspace-relative path, which is what the model is told to read.
    pub path: String,
    pub name: String,
    pub bytes: u64,
}

/// Store a file the user attached in the composer, so a tool can read it like any workspace
/// file. The name is reduced to its last component and suffixed when taken, so attaching
/// never overwrites something already in the workspace.
pub async fn attach(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(request): Json<AttachmentRequest>,
) -> Result<(StatusCode, Json<AttachmentView>), ApiFailure> {
    let root = super::resolve_workspace_root(&state, &workspace_id).await?;
    let name = attachment_name(&request.name)?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(request.data.as_bytes())
        .map_err(|_not_base64| {
            ApiFailure(silver_core::CoreError::InvalidRequest(
                "attachment data is not valid base64".into(),
            ))
        })?;
    if bytes.is_empty() {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(
            "the attached file is empty".into(),
        )));
    }
    let extension = std::path::Path::new(&name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !ATTACHMENT_TYPES.contains(&extension.as_str()) && std::str::from_utf8(&bytes).is_err() {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(format!(
            "{name} is not text, an image or a PDF, so the agent can't read it"
        ))));
    }
    let dir = root.join(ATTACHMENT_DIR);
    tokio::fs::create_dir_all(&dir).await.map_err(|error| {
        ApiFailure(silver_core::CoreError::Internal(format!(
            "{dir:?}: {error}"
        )))
    })?;
    // Attachments are the user's scratch, not project files: keep the whole .silver folder
    // out of git status and the Changes panel.
    let ignore = root.join(".silver/.gitignore");
    if !ignore.exists() {
        crate::atomic_file::write(&ignore, b"*\n").map_err(|error| {
            ApiFailure(silver_core::CoreError::Internal(format!(
                "{ignore:?}: {error}"
            )))
        })?;
    }
    let taken = free_name(&dir, &name)?;
    let saved = dir.join(&taken);
    crate::atomic_file::write(&saved, &bytes).map_err(|error| {
        ApiFailure(silver_core::CoreError::Internal(format!(
            "{saved:?}: {error}"
        )))
    })?;
    tracing::info!(workspace_id = %workspace_id, name = %taken, bytes = bytes.len(), "attachment stored");
    Ok((
        StatusCode::CREATED,
        Json(AttachmentView {
            path: format!("{ATTACHMENT_DIR}/{taken}"),
            name: taken,
            bytes: bytes.len() as u64,
        }),
    ))
}

/// The name an attachment is stored under: the last path component. A name is never taken from
/// the request beyond that.
fn attachment_name(requested: &str) -> Result<String, ApiFailure> {
    let base = std::path::Path::new(requested.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .trim();
    if base.is_empty() || base.len() > MAX_ATTACHMENT_NAME {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(
            "an attachment needs a file name".into(),
        )));
    }
    Ok(base.to_string())
}

/// `name`, then `name-2`, `name-3`… so a second attach of the same file never replaces the first.
fn free_name(dir: &std::path::Path, name: &str) -> Result<String, ApiFailure> {
    if !dir.join(name).exists() {
        return Ok(name.to_string());
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    for attempt in 2..1_000 {
        let candidate = match extension {
            "" => format!("{stem}-{attempt}"),
            extension => format!("{stem}-{attempt}.{extension}"),
        };
        if !dir.join(&candidate).exists() {
            return Ok(candidate);
        }
    }
    Err(ApiFailure(silver_core::CoreError::Internal(
        "too many copies of this attachment".into(),
    )))
}

/// Read one workspace file for the browser to display. The path is confined exactly as a tool
/// path is, and the size is capped by `tools.max_output_bytes` so it cannot read a large file.
pub async fn file(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiFailure> {
    let root = super::resolve_workspace_root(&state, &workspace_id).await?;
    let resolved = confine_path(&root, std::path::Path::new(&query.path))?;
    if let Some(reason) = silver_core::safety::read_denied_reason(&resolved) {
        return Err(ApiFailure(silver_core::CoreError::PathOutsideWorkspace(
            reason.to_string(),
        )));
    }
    let Some(media_type) = image_media_type(&resolved) else {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(format!(
            "{} is not a picture; this endpoint serves png, jpg, gif and webp",
            query.path
        ))));
    };
    let cap = state.config.tools.max_output_bytes;
    let bytes = match tokio::fs::read(&resolved).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiFailure(silver_core::CoreError::InvalidRequest(format!(
                "{} does not exist",
                query.path
            ))))
        }
        Err(error) => {
            return Err(ApiFailure(silver_core::CoreError::Internal(format!(
                "read {}: {error}",
                resolved.display()
            ))))
        }
    };
    if bytes.len() as u64 > cap {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(format!(
            "{} is larger than the {} byte limit",
            query.path, cap
        ))));
    }
    Ok(([(axum::http::header::CONTENT_TYPE, media_type)], bytes).into_response())
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    force: bool,
}

pub async fn remove(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> Result<StatusCode, ApiFailure> {
    let id: WorkspaceId = workspace_id.parse()?;
    let workspace = state
        .db
        .get_workspace(id)
        .await?
        .ok_or(ApiFailure(silver_core::CoreError::WorkspaceNotFound(id)))?;
    let has_data = state.db.workspace_has_data(id).await?;
    if has_data && !query.force {
        return Err(ApiFailure(silver_core::CoreError::InvalidRequest(
            "workspace has sessions or memory; pass force=true to remove managed data".into(),
        )));
    }
    if state.db.workspace_has_active_run(id).await? {
        return Err(ApiFailure(silver_core::CoreError::Conflict(
            "a run in this workspace is still active; stop it first".into(),
        )));
    }
    state.db.delete_workspace(id).await?;
    state.runs.cleanup_scope(&Scope::Workspace(id)).await;
    drop(std::fs::remove_dir_all(
        state.memory.scope_dir(&Scope::Workspace(id)),
    ));
    tracing::info!(workspace_id = %workspace.id, "workspace removed");
    Ok(StatusCode::NO_CONTENT)
}

fn workspace_view(workspace: Workspace) -> WorkspaceView {
    WorkspaceView {
        id: workspace.id,
        name: workspace.name,
        path: workspace.root.to_string_lossy().to_string(),
        available: workspace.canonical_root.is_dir(),
        created_at: workspace.created_at,
    }
}
