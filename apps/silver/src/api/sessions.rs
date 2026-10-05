//! Session endpoints.

use super::{ApiFailure, AppState};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use silver_core::error::CoreError;
use silver_core::redact::redact;
use silver_protocol::{
    ContentPart, CreateSessionRequest, MessageRole, MessageView, PlanView, RewindResponse,
    RunEvent, SessionId, SessionView, UpdateSessionRequest, WorkspaceId,
};

pub async fn create(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionView>), ApiFailure> {
    if let Some(workspace_id) = request.workspace_id {
        if state.db.get_workspace(workspace_id).await?.is_none() {
            return Err(ApiFailure(CoreError::WorkspaceNotFound(workspace_id)));
        }
    }
    let now = chrono::Utc::now();
    let session = silver_core::session::Session {
        id: SessionId::new(),
        workspace_id: request.workspace_id,
        source: request.source,
        external_key: request.external_key,
        title: request.title,
        created_at: now,
        updated_at: now,
    };
    let session = state.db.create_session(session).await?;
    Ok((StatusCode::CREATED, Json(session_view(session))))
}

#[derive(Deserialize)]
pub struct ListQuery {
    workspace_id: Option<String>,
    scope: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    /// Only sessions whose title or messages mention this text.
    q: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<SessionView>>, ApiFailure> {
    let limit = query.limit.unwrap_or(50).min(200);
    let (workspace_id, global) = match (query.scope.as_deref(), query.workspace_id.as_deref()) {
        (Some("global"), None) => (None, true),
        (_, Some(raw)) => {
            let id: WorkspaceId = raw.parse()?;
            (Some(id), false)
        }
        _ => {
            return Err(ApiFailure(CoreError::InvalidRequest(
                "specify workspace_id or scope=global".into(),
            )))
        }
    };
    // A malformed cursor is the caller's mistake, not a 500 from deep in the query.
    if let Some(cursor) = query.cursor.as_deref() {
        let valid = cursor
            .split_once('|')
            .is_some_and(|(stamp, _)| chrono::DateTime::parse_from_rfc3339(stamp).is_ok());
        if !valid {
            return Err(ApiFailure(CoreError::InvalidRequest(
                "cursor must be \"<updated_at>|<id>\" of the last session on the previous page"
                    .into(),
            )));
        }
    }
    let sessions = match query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(text) => matching(&state, workspace_id, global, text, limit).await?,
        None => {
            state
                .db
                .list_sessions(workspace_id, global, limit, query.cursor.as_deref())
                .await?
        }
    };
    // One bulk query supplies every row's preview; the page is already bounded by limit.
    let ids: Vec<SessionId> = sessions.iter().map(|session| session.id).collect();
    let mut previews = state.db.session_previews(&ids).await?;
    let mut overrides = state.db.session_model_overrides(&ids).await?;
    let mut efforts = state.db.session_reasoning_efforts(&ids).await?;
    let yolos = state.db.session_yolo_modes(&ids).await?;
    let mut presets = state.db.session_presets(&ids).await?;
    let mut goals = state.db.session_goals(&ids).await?;
    let active = state.db.session_active_runs(&ids).await?;
    Ok(Json(
        sessions
            .into_iter()
            .map(|session| {
                let id = session.id;
                SessionView {
                    goal: goals.remove(&id),
                    active_run: active.get(&id).copied(),
                    ..session_view_with_preview(
                        session,
                        previews.remove(&id),
                        overrides.remove(&id),
                        efforts.remove(&id),
                        yolos.get(&id).copied().unwrap_or(false),
                        presets.remove(&id),
                    )
                }
            })
            .collect(),
    ))
}

/// Sessions in the scope whose title or messages mention `text`, newest first.
async fn matching(
    state: &AppState,
    workspace_id: Option<WorkspaceId>,
    global: bool,
    text: &str,
    limit: u32,
) -> Result<Vec<silver_core::session::Session>, ApiFailure> {
    let needle = text.to_lowercase();
    let mut found: Vec<_> = state
        .db
        .list_sessions(workspace_id, global, 200, None)
        .await?
        .into_iter()
        .filter(|session| {
            session
                .title
                .as_deref()
                .is_some_and(|title| title.to_lowercase().contains(&needle))
        })
        .collect();
    for hit in state
        .db
        .search_messages(workspace_id, global, text, 200)
        .await?
    {
        if found.iter().any(|session| session.id == hit.session_id) {
            continue;
        }
        if let Some(session) = state.db.get_session(hit.session_id).await? {
            found.push(session);
        }
    }
    found.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    found.truncate(limit as usize);
    Ok(found)
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionView>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    Ok(Json(load_view(&state, id).await?))
}

/// One session's full view.
async fn load_view(state: &AppState, id: SessionId) -> Result<SessionView, ApiFailure> {
    let session = state
        .db
        .get_session(id)
        .await?
        .ok_or(ApiFailure(CoreError::SessionNotFound(id)))?;
    let preview = state.db.session_preview(id).await?;
    let model_override = state.db.session_model_override(id).await?;
    let reasoning_effort = state.db.session_reasoning_effort(id).await?;
    let yolo_mode = state.db.session_yolo_mode(id).await?;
    let preset = state.db.session_presets(&[id]).await?.remove(&id);
    Ok(SessionView {
        plan_mode: state.db.session_plan_mode(id).await?,
        goal: state.db.session_goals(&[id]).await?.remove(&id),
        active_run: state.db.session_active_runs(&[id]).await?.remove(&id),
        ..session_view_with_preview(
            session,
            preview,
            model_override,
            reasoning_effort,
            yolo_mode,
            preset,
        )
    })
}

/// PATCH /v1/sessions/{session_id}: an absent field is unchanged; an empty or blank string clears
/// it.
pub async fn update(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateSessionRequest>,
) -> Result<Json<SessionView>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    let session = state
        .db
        .get_session(id)
        .await?
        .ok_or(ApiFailure(CoreError::SessionNotFound(id)))?;
    if let Some(title) = request.title.as_deref() {
        state
            .db
            .set_session_title(id, normalized_optional(title))
            .await?;
    }
    if let Some(model) = request.model.as_deref() {
        state
            .db
            // A model picked now belongs to the provider picking it just activated.
            .set_session_model_override(
                id,
                normalized_optional(model),
                Some(state.runs.current_provider()),
            )
            .await?;
    }
    if let Some(effort) = request.reasoning_effort.as_deref() {
        let effort = normalized_optional(effort);
        if let Some(level) = &effort {
            if !crate::config::is_valid_reasoning_effort(level) {
                return Err(ApiFailure(CoreError::InvalidRequest(format!(
                    "reasoning_effort {level:?} must be one of {}",
                    crate::config::REASONING_EFFORT_LADDER.join(", ")
                ))));
            }
        }
        state.db.set_session_reasoning_effort(id, effort).await?;
    }
    if let Some(yolo_mode) = request.yolo_mode {
        state
            .runs
            .approval()
            .set_session_yolo(id, yolo_mode)
            .await
            .map_err(ApiFailure)?;
    }
    if let Some(preset) = request.preset.as_deref() {
        state
            .runs
            .set_session_preset(id, preset)
            .await
            .map_err(ApiFailure)?;
    }
    if let Some(goal) = request.goal {
        state.runs.update_goal(id, goal).await.map_err(ApiFailure)?;
    }
    if let Some(on) = request.plan_mode {
        state
            .runs
            .set_plan_mode(&session, on)
            .await
            .map_err(ApiFailure)?;
    }
    Ok(Json(load_view(&state, id).await?))
}

/// GET /v1/sessions/{session_id}/plan: the session's plan file and what it says.
pub async fn plan(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<PlanView>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    let path = state.runs.plan_file(id);
    Ok(Json(PlanView {
        content: std::fs::read_to_string(&path).ok(),
        path: path.display().to_string(),
    }))
}

/// A blank edit value clears the attribute (None); otherwise the trimmed value is stored.
fn normalized_optional(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Deserialize)]
pub struct MessagesQuery {
    limit: Option<u32>,
}

pub async fn messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Vec<MessageView>>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    if state.db.get_session(id).await?.is_none() {
        return Err(ApiFailure(CoreError::SessionNotFound(id)));
    }
    let limit = query.limit.unwrap_or(100).min(500);
    let messages = state.db.list_messages(id, limit).await?;
    // The metadata columns live beside the message row, so one extra point read per page
    // annotates the whole response without touching the transcript query.
    let mut metadata = state.db.message_metadata(id, limit).await?;
    Ok(Json(
        messages
            .into_iter()
            .map(|message| {
                let metadata = metadata.remove(&message.id).unwrap_or_default();
                MessageView {
                    id: message.id,
                    session_id: message.session_id,
                    run_id: message.run_id,
                    role: message.role,
                    content: message.content,
                    tool_name: metadata.tool_name,
                    finish_reason: metadata.finish_reason,
                    token_count: metadata.token_count,
                    created_at: message.created_at,
                }
            })
            .collect(),
    ))
}

/// POST /v1/sessions/{session_id}/rewind: remove the newest user turn and every message
/// that followed it, returning the removed user text (absent when there was none). A run
/// that is still active blocks the rewind, so a live transcript can never be truncated.
pub async fn rewind(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<silver_protocol::RewindQuery>,
) -> Result<Json<RewindResponse>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    if state.db.get_session(id).await?.is_none() {
        return Err(ApiFailure(CoreError::SessionNotFound(id)));
    }
    if state.db.has_active_run(id).await? {
        return Err(ApiFailure(CoreError::SessionBusy(id)));
    }
    let turns = query.turns.unwrap_or(1).clamp(1, 100);
    let outcome = state.db.rewind_last_n_turns(id, turns).await?;
    Ok(Json(RewindResponse {
        removed_user_text: outcome.removed_user_text,
        turns_undone: outcome.turns_undone,
        rewound_count: outcome.rewound_count,
        files_changed: outcome.files_changed,
    }))
}

/// Token usage for one session: per-run rows plus the session totals.
pub async fn usage(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<crate::db::SessionUsage>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    if state.db.get_session(id).await?.is_none() {
        return Err(ApiFailure(CoreError::SessionNotFound(id)));
    }
    let usage = state.db.session_usage(id).await?;
    Ok(Json(usage))
}

/// GET /v1/sessions/{session_id}/injected: what the session did besides the transcript. Every
/// advisor.check, every piece of context silver injected, and every step of a delegated
/// subagent, oldest first, so a reopened session shows the work behind a run.
pub async fn injected(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<RunEvent>>, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    let events = state.db.list_session_timeline(id).await?;
    Ok(Json(events))
}

/// Upper bound on a single trace export, so an enormous transcript cannot exhaust memory.
const MAX_TRACE_MESSAGES: u32 = 100_000;

#[derive(Deserialize)]
pub struct TraceQuery {
    format: Option<String>,
}

/// GET /v1/sessions/{session_id}/trace: the transcript as a JSON array, or JSON lines with
/// `?format=jsonl`, every string passed through the secret redactor.
pub async fn trace(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<TraceQuery>,
) -> Result<Response, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    if state.db.get_session(id).await?.is_none() {
        return Err(ApiFailure(CoreError::SessionNotFound(id)));
    }
    let messages = state.db.list_messages(id, MAX_TRACE_MESSAGES).await?;
    match query.format.as_deref() {
        None | Some("json") => Ok(Json(trace_rows(&messages)).into_response()),
        Some("jsonl") => Ok(jsonl_response(&messages)?),
        Some(other) => Err(ApiFailure(CoreError::InvalidRequest(format!(
            "unsupported trace format {other:?}; use json or jsonl"
        )))),
    }
}

/// Serialize the transcript as newline-delimited JSON (one row per line).
fn jsonl_response(messages: &[silver_core::session::Message]) -> Result<Response, ApiFailure> {
    let mut body = String::new();
    for row in trace_rows(messages) {
        let line = serde_json::to_string(&row).map_err(|err| {
            ApiFailure(CoreError::Internal(format!("serialize trace row: {err}")))
        })?;
        body.push_str(&line);
        body.push('\n');
    }
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response())
}

/// Build Claude-Code-like rows for a transcript, redacting every string body.
fn trace_rows(messages: &[silver_core::session::Message]) -> Vec<Value> {
    messages.iter().map(trace_row).collect()
}

/// One transcript row. Tool results ride a user turn as a tool_result block, matching the
/// Claude Code transcript shape.
fn trace_row(message: &silver_core::session::Message) -> Value {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
    };
    json!({
        "uuid": message.id.to_string(),
        "timestamp": message.created_at.to_rfc3339(),
        "role": role,
        "content": content_blocks(&message.content),
    })
}

fn content_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts.iter().map(content_block).collect()
}

fn content_block(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({"type": "text", "text": redact(text)}),
        ContentPart::Reasoning { text } => json!({"type": "thinking", "thinking": redact(text)}),
        ContentPart::ToolCall {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_use",
            "id": id.to_string(),
            "name": name,
            "input": redact_value(arguments),
        }),
        ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_call_id.to_string(),
            "content": redact(content),
            "is_error": is_error,
        }),
        // The pixels never reach the transcript; a client reads the receipt on the tool result.
        ContentPart::Image { .. } => json!({
            "type": "text",
            "text": "[an image was shown to the model; the transcript keeps only the tool result]",
        }),
        ContentPart::Attachment { name, path } => json!({
            "type": "text",
            "text": format!("[attached: {name} → {path}]"),
        }),
    }
}

/// Redact secret-shaped strings inside a JSON value (tool arguments).
fn redact_value(value: &Value) -> Value {
    let text = redact(&value.to_string());
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

pub async fn remove(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let id: SessionId = session_id.parse()?;
    // Deleting under a live run would leave it writing into rows that no longer exist.
    if state.db.has_active_run(id).await? {
        return Err(ApiFailure(CoreError::SessionBusy(id)));
    }
    state.db.delete_session(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn session_view(session: silver_core::session::Session) -> SessionView {
    session_view_with_preview(session, None, None, None, false, None)
}

/// Build a view with the session's preview, model override, reasoning effort and YOLO flag
/// resolved.
fn session_view_with_preview(
    session: silver_core::session::Session,
    preview: Option<String>,
    model_override: Option<String>,
    reasoning_effort: Option<String>,
    yolo_mode: bool,
    preset: Option<String>,
) -> SessionView {
    SessionView {
        id: session.id,
        workspace_id: session.workspace_id,
        source: session.source,
        external_key: session.external_key,
        // An absent or blank title serializes as no title at all rather than a placeholder.
        title: session.title.filter(|title| !title.trim().is_empty()),
        preview,
        // Same convention for the override: blank means the daemon default applies.
        model_override: model_override.filter(|model| !model.trim().is_empty()),
        reasoning_effort: reasoning_effort.filter(|effort| !effort.trim().is_empty()),
        yolo_mode,
        plan_mode: false,
        preset: preset.filter(|preset| !preset.trim().is_empty()),
        goal: None,
        active_run: None,
        created_at: session.created_at,
        updated_at: session.updated_at,
    }
}
