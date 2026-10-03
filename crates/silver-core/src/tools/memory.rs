//! The memory tool: explicit edits to scope-local memory, in the scope from the run context, never
//! from an argument (INV-8).

use crate::error::{CoreError, CoreResult};
use crate::memory::{
    apply_operation, apply_operation_detailed, check_char_budget, contains_entry, content_hash,
    parse_entries, scan_memory_content, usage_for, MemoryFile, MemoryOpError, MemoryOperation,
};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use silver_protocol::{EventPayload, MemoryOpKind, RiskLevel, RunId};
use std::collections::HashMap;
use std::sync::Arc;

/// Maximum byte length accepted for text written by add or replace.
pub const MAX_MEMORY_CONTENT_BYTES: usize = 32_768;

/// Register the memory tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(MemoryTool::default()));
}

/// Failed consolidation attempts (over-budget / no-match) allowed per turn before the tool
/// returns a TERMINAL result, matching Hermes _MAX_CONSOLIDATION_FAILURES_PER_TURN. One run
/// is one user turn, so the counter is keyed by run id and cleared by any successful write.
pub const MAX_CONSOLIDATION_FAILURES_PER_TURN: u32 = 3;

/// Upper bound on tracked run ids in the consolidation-failure map.
const MAX_TRACKED_RUNS: usize = 256;

/// The memory tool plus its per-turn consolidation-failure counter.
#[derive(Default)]
struct MemoryTool {
    consolidation_failures: std::sync::Mutex<HashMap<RunId, u32>>,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Save short facts that load into every future session. target 'user' = about the user \
         (name, language, preferences); 'memory' = about this project (commands, conventions). \
         action 'add' saves content; 'replace' swaps the entry containing old_text for content; \
         'remove' deletes the entry containing old_text. Pass only target to list the entries."
    }

    fn schema(&self) -> serde_json::Value {
        // Kept to the four fields a small model needs. The executor still accepts the Hermes
        // 'operations' batch and the 'new_text' alias from callers that send them anyway.
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": ["user", "memory"]
                },
                "action": {
                    "type": "string",
                    "enum": ["add", "replace", "remove"]
                },
                "content": {
                    "type": "string",
                    "description": "The fact, for add and replace."
                },
                "old_text": {
                    "type": "string",
                    "description": "For replace and remove: a few words of the entry to change."
                }
            },
            "required": ["target"]
        })
    }

    fn risk(&self, _args: &serde_json::Value) -> RiskLevel {
        RiskLevel::Memory
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome> {
        let Some(store) = ctx.run.services.memory.as_ref() else {
            return Ok(ToolOutcome::error("memory is unavailable"));
        };

        // Hermes defaults an absent or null target to 'memory'; the JSON schema marks
        // target required, so this only covers lenient callers.
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("memory");
        let file = match target {
            "memory" => MemoryFile::Memory,
            "user" => MemoryFile::User,
            other => {
                return Ok(ToolOutcome::error(format!(
                    "Invalid memory target '{other}'. Use 'memory' or 'user'."
                )))
            }
        };

        // The batch shape wins over the single-op fields when 'operations' is present.
        if let Some(operations) = args.get("operations").filter(|v| !v.is_null()) {
            let Some(operations) = operations.as_array() else {
                return Ok(ToolOutcome::error(
                    "operations must be a list of {action, content?, old_text?} objects.",
                ));
            };
            return execute_batch(self, ctx, store, file, operations).await;
        }

        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("new_text").and_then(|v| v.as_str()))
            .unwrap_or("");
        let old_text = args.get("old_text").and_then(|v| v.as_str()).unwrap_or("");

        // A small model calls memory with only a target to look at it; show the entries rather
        // than fail on the missing action. The prompt-safe view, as the system prompt shows.
        if action.is_empty() && content.is_empty() && old_text.is_empty() {
            let snapshot = store.load(&ctx.run.scope).await?;
            let entries = snapshot.for_file(file).trim();
            return Ok(ToolOutcome::ok(if entries.is_empty() {
                format!("No {target} entries yet. To save one, call memory with action 'add'.")
            } else {
                entries.to_string()
            }));
        }

        let operation = match build_operation(action, content, old_text) {
            Ok(operation) => operation,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };

        // The live (unsanitized) view is what read-modify-write edits; the prompt-safe
        // quarantine view is only for the system prompt.
        let snapshot = store.load_raw(&ctx.run.scope).await?;
        let current = snapshot.for_file(file).to_string();

        // Exact duplicate adds are skipped, not appended (Hermes add is idempotent).
        if let MemoryOperation::Add { content } = &operation {
            if contains_entry(&current, content) {
                return Ok(ToolOutcome::ok(single_payload(
                    file,
                    operation.kind(),
                    &content_hash(&current),
                    &usage_for(file, &current),
                    Some("Entry already exists (no duplicate added)."),
                )));
            }
        }

        // Budget the FINAL rendered content before touching the store.
        let next = match apply_operation_detailed(&current, &operation) {
            Ok(next) => next,
            Err(error @ MemoryOpError::NoMatch(_)) => {
                return Ok(self.consolidation_failure(ctx, file, &current, error.message()))
            }
            Err(other) => return Ok(ToolOutcome::error(other.message())),
        };
        match check_char_budget(file, &next) {
            Ok(()) => {}
            Err(CoreError::InvalidRequest(message)) => {
                return Ok(self.consolidation_failure(ctx, file, &current, &message))
            }
            Err(other) => return Err(other),
        }

        let change = match store.apply(&ctx.run.scope, file, &operation).await {
            Ok(change) => change,
            Err(CoreError::InvalidRequest(message)) => return Ok(ToolOutcome::error(message)),
            Err(other) => return Err(other),
        };

        self.clear_consolidation_failures(ctx.run.run_id);
        let payload = single_payload(
            file,
            operation.kind(),
            &change.after_hash,
            &usage_for(file, &next),
            None,
        );
        ctx.events.emit(EventPayload::MemoryChanged {
            file: file.kind(),
            operation: operation.kind(),
            content_hash: change.after_hash,
        });
        Ok(ToolOutcome::ok(payload))
    }
}

impl MemoryTool {
    /// Count a consolidation failure. Under the cap the error echoes the live entries and
    /// usage so the model can consolidate; past it the result is TERMINAL so a fragile call
    /// cannot loop the turn to budget exhaustion (Hermes #42405).
    fn consolidation_failure(
        &self,
        ctx: &ToolContext<'_>,
        file: MemoryFile,
        current: &str,
        message: &str,
    ) -> ToolOutcome {
        let run = ctx.run.run_id;
        let mut failures = self
            .consolidation_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Bound the map in a long-lived daemon. Resetting another run's counter only ever
        // grants extra retries to a UX guard, never a security check.
        if failures.len() > MAX_TRACKED_RUNS {
            failures.retain(|key, _| *key == run);
        }
        let entry = failures.entry(run).or_insert(0);
        *entry += 1;
        let count = *entry;
        if count > MAX_CONSOLIDATION_FAILURES_PER_TURN {
            return ToolOutcome::error(
                serde_json::json!({
                    "success": false,
                    "done": true,
                    "error": format!(
                        "Memory consolidation failed {count} times this turn. Stop retrying memory calls — leave memory unchanged for now and continue with your reply to the user. The fact can be saved in a later turn."
                    ),
                })
                .to_string(),
            );
        }
        ToolOutcome::error(
            serde_json::json!({
                "success": false,
                "error": message,
                "current_entries": parse_entries(current),
                "usage": usage_for(file, current),
            })
            .to_string(),
        )
    }

    fn clear_consolidation_failures(&self, run: RunId) {
        self.consolidation_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&run);
    }
}

/// Build one operation, mirroring the Hermes single-op validation. Replace and remove
/// identify an existing entry with old_text; add and replace carry the new content.
fn build_operation(action: &str, content: &str, old_text: &str) -> Result<MemoryOperation, String> {
    match action {
        "add" => {
            if content.trim().is_empty() {
                return Err("Content is required for 'add' action.".into());
            }
            check_content_cap(content)?;
            if let Some(message) = scan_memory_content(content) {
                return Err(message);
            }
            Ok(MemoryOperation::Add {
                content: content.to_string(),
            })
        }
        "replace" => {
            if old_text.trim().is_empty() {
                return Err(
                    "'replace' needs old_text -- a short unique substring of the entry to replace."
                        .into(),
                );
            }
            if content.trim().is_empty() {
                return Err("content is required for 'replace' action.".into());
            }
            check_content_cap(content)?;
            if let Some(message) = scan_memory_content(content) {
                return Err(message);
            }
            Ok(MemoryOperation::Replace {
                old: old_text.to_string(),
                new: content.to_string(),
            })
        }
        "remove" => {
            if old_text.trim().is_empty() {
                return Err(
                    "'remove' needs old_text -- a short unique substring of the entry to remove."
                        .into(),
                );
            }
            Ok(MemoryOperation::Remove {
                content: old_text.to_string(),
            })
        }
        other => Err(format!(
            "Unknown action '{other}'. Use: add, replace, remove"
        )),
    }
}

fn check_content_cap(content: &str) -> Result<(), String> {
    if content.len() > MAX_MEMORY_CONTENT_BYTES {
        return Err(format!(
            "memory content exceeds the {MAX_MEMORY_CONTENT_BYTES} byte limit"
        ));
    }
    Ok(())
}

/// Apply a batch of operations in order. Hermes applies a batch all-or-nothing: the
/// whole list is validated against a working copy first, so one bad operation writes
/// nothing and the first failure is reported.
async fn execute_batch(
    tool: &MemoryTool,
    ctx: &ToolContext<'_>,
    store: &Arc<dyn crate::memory::MemoryStore>,
    file: MemoryFile,
    operations: &[serde_json::Value],
) -> CoreResult<ToolOutcome> {
    if operations.is_empty() {
        return Ok(ToolOutcome::error("operations list is empty."));
    }

    let snapshot = store.load_raw(&ctx.run.scope).await?;
    let original = snapshot.for_file(file);
    let mut working = original.to_string();

    // Validate the whole batch against a working copy. Nothing is written until every
    // operation has applied and the FINAL budget is satisfied.
    let mut built = Vec::with_capacity(operations.len());
    let mut hashes = Vec::with_capacity(operations.len());
    for (index, op) in operations.iter().enumerate() {
        let action = op.get("action").and_then(|v| v.as_str()).unwrap_or("");
        // In a batch, 'content' and 'new_text' alias each other and either may be empty.
        let content = op
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|value| !value.is_empty())
            .or_else(|| op.get("new_text").and_then(|v| v.as_str()))
            .unwrap_or("");
        let old_text = op.get("old_text").and_then(|v| v.as_str()).unwrap_or("");

        let operation = match build_operation(action, content, old_text) {
            Ok(operation) => operation,
            Err(message) => {
                let message = batch_error_message(index, action, &message);
                return Ok(tool.consolidation_failure(ctx, file, original, &message));
            }
        };
        working = match apply_operation(&working, &operation) {
            Ok(next) => next,
            Err(CoreError::InvalidRequest(message)) => {
                let message = batch_error_message(index, action, &message);
                return Ok(tool.consolidation_failure(ctx, file, original, &message));
            }
            Err(other) => return Err(other),
        };
        hashes.push(content_hash(&working));
        built.push(operation);
    }

    // #103419: a consolidation batch that removes the last entry would commit an empty
    // file as a normal success. Refuse; a single remove is the deliberate-wipe path.
    if !original.trim().is_empty() && working.trim().is_empty() {
        let message = "Refusing to empty a previously non-empty memory store: this batch would remove every entry. Nothing was applied (batch is all-or-nothing). Keep at least one entry or use a single remove.";
        return Ok(tool.consolidation_failure(ctx, file, original, message));
    }

    // Budget check is against the FINAL state only, so one batch can free space and add.
    match check_char_budget(file, &working) {
        Ok(()) => {}
        Err(CoreError::InvalidRequest(message)) => {
            let message =
                format!("{message}. No operations were applied (batch is all-or-nothing).");
            return Ok(tool.consolidation_failure(ctx, file, original, &message));
        }
        Err(other) => return Err(other),
    }

    let usage = usage_for(file, &working);
    // A single atomic store write: the validated final content replaces the file, so a
    // failure cannot leave a partially-applied batch on disk.
    let change = store
        .apply(
            &ctx.run.scope,
            file,
            &MemoryOperation::SetContent { content: working },
        )
        .await?;

    tool.clear_consolidation_failures(ctx.run.run_id);
    for (operation, hash) in built.iter().zip(hashes) {
        ctx.events.emit(EventPayload::MemoryChanged {
            file: file.kind(),
            operation: operation.kind(),
            content_hash: hash,
        });
    }

    Ok(ToolOutcome::ok(
        serde_json::json!({
            "success": true,
            "done": true,
            "target": target_name(file),
            "file_name": file.file_name(),
            "applied": built.len(),
            "usage": usage,
            "content_hash": change.after_hash,
        })
        .to_string(),
    ))
}

fn batch_error_message(index: usize, action: &str, message: &str) -> String {
    let label = if action.is_empty() { "unknown" } else { action };
    format!(
        "Operation {} ({label}): {message}. No operations were applied (batch is all-or-nothing).",
        index + 1
    )
}

fn single_payload(
    file: MemoryFile,
    operation: MemoryOpKind,
    content_hash: &str,
    usage: &str,
    message: Option<&str>,
) -> String {
    let mut payload = serde_json::json!({
        "success": true,
        "done": true,
        "target": target_name(file),
        "file_name": file.file_name(),
        "operation": operation_name(operation),
        "usage": usage,
        "content_hash": content_hash,
    });
    if let Some(message) = message {
        payload["message"] = serde_json::Value::String(message.to_string());
    }
    payload.to_string()
}

fn target_name(file: MemoryFile) -> &'static str {
    match file {
        MemoryFile::Memory => "memory",
        MemoryFile::User => "user",
    }
}

fn operation_name(kind: MemoryOpKind) -> &'static str {
    match kind {
        MemoryOpKind::Add => "add",
        MemoryOpKind::Replace => "replace",
        MemoryOpKind::Remove => "remove",
    }
}
