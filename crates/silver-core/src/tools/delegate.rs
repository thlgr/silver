//! The delegate_task tool: independent tasks for subagents, each with its own prompt and tools and
//! no memory of this conversation; only its report comes back. Tasks in one call run in parallel
//! up to `delegation.max_concurrent`, with no steering or continuation.

use crate::error::{CoreError, CoreResult};
use crate::subagent::{Isolation, SubagentOutcome, SubagentRequest, SubagentTask, DELEGATE_TASK};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use crate::toolset;
use async_trait::async_trait;
use serde_json::{json, Value};
use silver_protocol::{RiskLevel, ToolStatus};
use std::sync::Arc;
use std::time::Duration;

/// Register the delegate_task tool, with the daemon's concurrency limit baked into the schema
/// so the model sees it instead of having a call refused.
pub fn register(registry: &mut ToolRegistry, max_concurrent: usize) {
    registry.register(Arc::new(DelegateTask {
        max_concurrent: max_concurrent.max(1),
    }));
}

struct DelegateTask {
    max_concurrent: usize,
}

#[async_trait]
impl Tool for DelegateTask {
    fn name(&self) -> &'static str {
        DELEGATE_TASK
    }

    fn description(&self) -> &'static str {
        "Delegate one or more independent tasks to subagents. Each subagent works on its own, \
         with its own context and its own tools, and reports back when it is done; you get the \
         reports, not their transcripts. Use it for work that would take you many steps: \
         answering a question by sweeping many files, designing an implementation, or verifying a \
         change from scratch. Independent tasks go in ONE call and run in parallel. A subagent \
         starts with nothing, so its prompt must carry the whole brief: what to do, why, and what \
         you already know. Say in the prompt whether it should only research or may change \
         files. The reports are not shown to the user, so relay what matters."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "The tasks to delegate. Independent tasks go in the same call.",
                    "minItems": 1,
                    "maxItems": self.max_concurrent,
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent": {
                                "type": "string",
                                "description": "Which subagent to run. Omit for the general-purpose agent.",
                            },
                            "description": {
                                "type": "string",
                                "description": "A short (3-5 word) label for this task.",
                            },
                            "prompt": {
                                "type": "string",
                                "description": "The full brief for the subagent. It starts with no context, so include what it needs to know.",
                            },
                            "isolation": {
                                "type": "string",
                                "enum": ["worktree"],
                                "description": "Set to 'worktree' to let this task change files in a temporary git worktree, away from the working tree.",
                            }
                        },
                        "required": ["description", "prompt"],
                    }
                }
            },
            "required": ["tasks"],
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        // Delegating is not itself a mutation: each tool the subagent calls is gated by the
        // same policy, and for the same approvals, as the parent's own calls.
        RiskLevel::Read
    }

    fn toolset(&self) -> &'static str {
        toolset::DELEGATION
    }

    fn timeout_hint(&self) -> Option<Duration> {
        // A batch drives whole model turns, so it needs minutes where a tool needs seconds.
        // The loop also takes the model's own `timeout` argument and the configured ceiling.
        Some(Duration::from_secs(900))
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let tasks = read_tasks(&args, self.max_concurrent)?;
        let Some(subagents) = ctx.run.services.subagents.as_deref() else {
            return Err(CoreError::ToolNotAllowed(format!(
                "{DELEGATE_TASK} is not available in this daemon"
            )));
        };
        for task in &tasks {
            if task.isolation.is_some() && ctx.run.workspace.is_none() {
                return Ok(ToolOutcome::error(
                    "isolation: worktree needs a workspace, and this run has none. Delegate \
                     without isolation, or ask the user to open a folder as a workspace."
                        .to_string(),
                ));
            }
        }
        let outcomes = subagents
            .run(
                tasks,
                SubagentRequest {
                    run: &ctx.run,
                    call_id: &ctx.call_id,
                    events: &ctx.events,
                    cancel: &ctx.cancel,
                    gate: &ctx.gate,
                },
            )
            .await?;
        Ok(render_outcomes(outcomes))
    }
}

/// Read the batch, forgiving about a missing `agent` and exact about the rest: a small model
/// that omits a field should be told which, not handed a half-run batch.
fn read_tasks(args: &Value, max_concurrent: usize) -> CoreResult<Vec<SubagentTask>> {
    let tasks = args
        .get("tasks")
        .and_then(Value::as_array)
        .filter(|tasks| !tasks.is_empty())
        .ok_or_else(|| {
            CoreError::InvalidRequest(format!(
                "{DELEGATE_TASK} needs a `tasks` list with at least one task"
            ))
        })?;
    if tasks.len() > max_concurrent {
        return Err(CoreError::InvalidRequest(format!(
            "{} tasks were asked for and at most {max_concurrent} run at once. Split the batch, \
             or leave the extra ones for a later call.",
            tasks.len()
        )));
    }
    tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let at = format!("task {index}");
            let text = |field: &str| {
                task.get(field)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let prompt = text("prompt");
            if prompt.is_empty() {
                return Err(CoreError::InvalidRequest(format!(
                    "{at} has no prompt: a subagent starts with no context, so the brief is the \
                     whole task"
                )));
            }
            let isolation = match task.get("isolation").and_then(Value::as_str) {
                Some(value) if !value.trim().is_empty() => {
                    Some(Isolation::parse(value).ok_or_else(|| {
                        CoreError::InvalidRequest(format!("{at}: unknown isolation {value:?}"))
                    })?)
                }
                _ => None,
            };
            let agent = task.get("agent").and_then(Value::as_str).map(str::trim);
            Ok(SubagentTask {
                agent: agent.filter(|name| !name.is_empty()).map(str::to_string),
                description: {
                    let label = text("description");
                    if label.is_empty() {
                        first_line(&prompt)
                    } else {
                        label
                    }
                },
                prompt,
                isolation,
            })
        })
        .collect()
}

/// A short label for a task the model left undescribed: its first line, clipped.
fn first_line(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("task");
    let line = line.trim();
    if line.chars().count() <= 60 {
        return line.to_string();
    }
    let cut = line.char_indices().nth(57).map(|(i, _)| i).unwrap_or(0);
    format!("{}…", line[..cut].trim_end())
}

/// The parent's result: one block per task, headed by what ran and what it cost.
fn render_outcomes(outcomes: Vec<SubagentOutcome>) -> ToolOutcome {
    let mut text = String::new();
    for outcome in &outcomes {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let seconds = outcome.duration_ms as f64 / 1000.0;
        let status = match outcome.status {
            ToolStatus::Completed => "done".to_string(),
            // A cancelled subagent is reported as a failure: the run it belonged to is
            // stopping too, so there is nothing left for the parent to do with a status.
            status => format!("{}: {}", status_word(status), outcome.text),
        };
        text.push_str(&format!(
            "[{}] {} — {status}",
            outcome.agent, outcome.description
        ));
        // A task that never started has no timing worth showing.
        if outcome.status == ToolStatus::Completed || outcome.tool_uses > 0 {
            text.push_str(&format!(
                ", {seconds:.1}s, {} tool calls",
                outcome.tool_uses
            ));
        }
        if let Some(worktree) = &outcome.worktree {
            text.push_str(&format!(", worktree {worktree}"));
        }
        text.push('\n');
        let report = outcome.text.trim();
        if !report.is_empty() && outcome.status == ToolStatus::Completed {
            text.push_str(report);
        }
    }
    // One failed task does not spoil the batch: the parent still has the reports that worked.
    let failed = outcomes.iter().filter(|outcome| outcome.is_error()).count();
    let outcome = if failed == outcomes.len() {
        ToolOutcome::error(text)
    } else {
        ToolOutcome::ok(text)
    };
    let count = outcomes.len();
    outcome.with_summary(if count == 1 {
        outcomes
            .into_iter()
            .next()
            .map(|one| one.text)
            .unwrap_or_default()
    } else {
        format!(
            "{count} task{} delegated, {} done",
            if count == 1 { "" } else { "s" },
            count - failed
        )
    })
}

fn status_word(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Completed => "done",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied => "denied",
        ToolStatus::Blocked => "blocked",
    }
}
