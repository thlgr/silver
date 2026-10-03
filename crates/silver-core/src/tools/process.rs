//! The process tool. It inspects and controls the background processes started by the
//! bash tool. The terminal backend owns the per-scope process table; this tool only maps
//! the model-facing actions onto it.

use crate::error::{CoreError, CoreResult};
use crate::services::ProcessAction;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct ProcessArgs {
    action: String,
    #[serde(default)]
    session_id: Option<String>,
}

struct ProcessTool;

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &'static str {
        "process_manage"
    }

    fn description(&self) -> &'static str {
        "Inspect and control background processes started with bash(background=true). list: every tracked process in this scope. read: current output and status. wait: block until the process exits (bounded). kill: terminate it. A session_id from the background result is required for every action except list; any unique prefix works."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "kill", "wait"],
                    "description": "list: every tracked process. read: current output and status. wait: block until exit. kill: terminate."
                },
                "session_id": {
                    "type": "string",
                    "description": "From the bash background result; any unique prefix works. Required except for 'list'."
                }
            },
            "required": ["action"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Process
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let parsed: ProcessArgs = serde_json::from_value(args).map_err(|e| {
            CoreError::InvalidRequest(format!("invalid process_manage arguments: {e}"))
        })?;
        let action = parsed.action.trim().to_lowercase();

        let backend = match ctx.run.services.terminal.as_ref() {
            Some(backend) => backend,
            None => return Ok(ToolOutcome::error("bash is unavailable")),
        };

        if action == "list" {
            let processes = backend.processes(&ctx.run.scope).await?;
            let items: Vec<Value> = processes
                .iter()
                .map(|process| {
                    json!({
                        "session_id": process.id,
                        "command": process.command,
                        "status": if process.running { "running" } else { "exited" },
                        "started_at": process.started_at.to_rfc3339(),
                    })
                })
                .collect();
            return Ok(ToolOutcome::ok(json!({ "processes": items }).to_string()));
        }

        let process_action = match action.as_str() {
            "read" => ProcessAction::Read,
            "kill" => ProcessAction::Kill,
            "wait" => ProcessAction::Wait,
            other => {
                return Ok(ToolOutcome::error(format!(
                    "Unknown process action: {other}. Use: list, read, kill, wait"
                )))
            }
        };

        let session_id = parsed
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let Some(session_id) = session_id else {
            return Ok(ToolOutcome::error(format!(
                "session_id is required for {action}"
            )));
        };

        let result = backend
            .process_action(&ctx.run.scope, session_id, process_action)
            .await?;
        Ok(ToolOutcome::ok(result))
    }
}

/// Register the process tool in the given registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ProcessTool));
}
