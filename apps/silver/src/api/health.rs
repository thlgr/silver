//! Health and capabilities.

use super::{ApiFailure, AppState};
use axum::{extract::State, Json};
use chrono::Utc;
use serde_json::json;
use silver_protocol::{CapabilitiesResponse, LimitsView, ToolCapability};

pub async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiFailure> {
    let database = match state.db.list_workspaces().await {
        Ok(_) => "ok",
        Err(_) => "error",
    };
    Ok(Json(json!({
        "status": if database == "ok" { "ok" } else { "degraded" },
        "database": database,
        "active_runs": state.runs.active_run_count().await,
        "paused": state.runs.is_paused(),
        "uptime_seconds": (Utc::now() - state.started_at).num_seconds().max(0),
    })))
}

pub async fn capabilities(
    State(state): State<AppState>,
) -> Result<Json<CapabilitiesResponse>, ApiFailure> {
    let empty = serde_json::json!({});
    let agent = state.runs.agent();
    // Only the tools a run would be given, filtered exactly as `GET /v1/tools` filters them.
    let tools = agent
        .tools()
        .all()
        .iter()
        .filter(|tool| agent.tool_enabled(tool.name(), tool.toolset()))
        .map(|tool| ToolCapability {
            name: tool.name().to_string(),
            risk: tool.risk(&empty),
            requires_workspace: tool.requires_workspace(),
        })
        .collect();
    Ok(Json(CapabilitiesResponse {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: "v1".to_string(),
        providers: vec![String::clone(&state.config.model.provider)],
        model: state.config.resolved_model().to_string(),
        context_length: state.context_length,
        tools,
        limits: LimitsView {
            max_concurrent_runs: state.config.server.max_concurrent_runs,
            run_timeout_seconds: state.config.server.run_timeout_seconds,
            max_message_bytes: state.config.server.max_message_bytes,
        },
        features: vec![
            "sse".into(),
            "replay".into(),
            "approvals".into(),
            "stop".into(),
            "steer".into(),
            "workspaces".into(),
            "session_search".into(),
            "documents".into(),
            "image_view".into(),
            "loop_detection".into(),
            "pause".into(),
            "idempotency".into(),
        ],
    }))
}
