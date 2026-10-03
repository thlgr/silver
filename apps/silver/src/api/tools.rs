//! Tool inventory: registered tool names and their toolsets, read through the run manager's agent
//! so it includes MCP tools registered at runtime.

use super::{ApiFailure, AppState};
use axum::{extract::State, Json};
use serde_json::{json, Value};

/// `GET /v1/tools`: every registered tool with its toolset, plus the distinct toolset list.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiFailure> {
    let agent = state.runs.agent();
    let registry = agent.tools();
    let tools: Vec<Value> = registry
        .all()
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "toolset": tool.toolset(),
                // Reflect the same filtering the model sees: a tool the toolset selection or the
                // [tools] enabled/disabled name lists exclude reports enabled=false, so the CLI and
                // TUI panel tone it down instead of showing it as live.
                "enabled": agent.tool_enabled(tool.name(), tool.toolset()),
                "requires_workspace": tool.requires_workspace(),
            })
        })
        .collect();
    Ok(Json(json!({
        "toolsets": registry.toolsets(),
        "tools": tools,
    })))
}
