//! The todo_list tool: pass `todos` to write, omit it to read; every call returns the list and a
//! summary. Kept per session by the daemon. Order is priority, and a single in_progress step is
//! lifted ahead of an earlier pending placeholder.

use crate::error::CoreResult;
use crate::services::TodoItem;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

/// Status words accepted from the model, matching the upstream tool.
pub const VALID_STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];
/// Cap on a single item's content kept in the list, matching the upstream tool.
pub const MAX_TODO_CONTENT_CHARS: usize = 4000;
/// Hard cap on the number of items kept for one session, matching the upstream tool.
pub const MAX_TODO_ITEMS: usize = 256;
/// Appended when content is truncated; the actionable head is always kept.
const TRUNCATION_MARKER: &str = "\u{2026} [truncated]";

/// Register the todo_list tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(TodoListTool));
}

struct TodoListTool;

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &'static str {
        "todo_list"
    }

    fn description(&self) -> &'static str {
        "Track a task list for multi-step work (3+ steps) or when the user gives multiple tasks. \
         For 'all N items' tasks, enumerate every instance as its own item so none are dropped. \
         Call with no parameters to read the current list. List order is priority; only ONE item \
         in_progress at a time. Mark completed only after the work is verified, never on intent; \
         if something fails, cancel it and add a revised item. Always returns the full list."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Task items to write.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string"
                            },
                            "content": {
                                "type": "string",
                                "description": "Task description"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["id", "content", "status"]
                    }
                },
                "merge": {
                    "type": "boolean",
                    "description": "true: update existing items by id, add new ones. \
                                    false (default): replace the entire list with a fresh plan.",
                    "default": false
                }
            },
            "required": []
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Memory
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &ToolContext<'_>, mut args: Value) -> CoreResult<ToolOutcome> {
        let Some(store) = ctx.run.services.todos.as_ref() else {
            return Ok(ToolOutcome::error("todo store is unavailable"));
        };
        let session = ctx.run.session.id;

        let items = match parse_todos_arg(&mut args) {
            Ok(None) => store.list(session).await?,
            Ok(Some(incoming)) => {
                let merge = args.get("merge").and_then(Value::as_bool).unwrap_or(false);
                let current = store.list(session).await?;
                let next = build_list(current, &incoming, merge);
                store.write(session, next).await?
            }
            Err(message) => return Ok(ToolOutcome::error(message)),
        };

        Ok(ToolOutcome::ok(render(&items)))
    }
}

/// Parse the optional 'todos' argument. Absent or null means read. A JSON string is accepted
/// because models sometimes send a stringified list, mirroring the upstream handler.
fn parse_todos_arg(args: &mut Value) -> Result<Option<Vec<Value>>, String> {
    let Some(value) = args.get_mut("todos").map(Value::take) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => Ok(Some(items)),
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Array(items)) => Ok(Some(items)),
            Ok(Value::Null) => Ok(None),
            Ok(other) => Err(format!("todos must be a list, got {}", type_name(&other))),
            Err(_) => Err("todos must be a list of objects, got unparseable string".to_string()),
        },
        other => Err(format!("todos must be a list, got {}", type_name(&other))),
    }
}

/// Build the next stored list: a replace validates and dedupes, a merge patches by id and
/// appends new items. Both cap the list and normalize its order.
fn build_list(current: Vec<TodoItem>, incoming: &[Value], merge: bool) -> Vec<TodoItem> {
    let mut items = if merge {
        merge_items(current, incoming)
    } else {
        fresh_items(incoming)
    };
    if items.len() > MAX_TODO_ITEMS {
        items.truncate(MAX_TODO_ITEMS);
    }
    items
}

/// Validate, dedupe and order a whole new list (replace).
fn fresh_items(incoming: &[Value]) -> Vec<TodoItem> {
    let validated = dedupe_by_id(incoming).into_iter().map(validate).collect();
    normalize_order(validated)
}

/// Update existing items only in the fields provided; append new ones (validated). Rebuild
/// preserving the original order for existing items.
fn merge_items(current: Vec<TodoItem>, incoming: &[Value]) -> Vec<TodoItem> {
    // A repeated id keeps its first position and its last value.
    fn upsert(items: &mut Vec<TodoItem>, item: TodoItem) {
        match items.iter_mut().find(|known| known.id == item.id) {
            Some(known) => *known = item,
            None => items.push(item),
        }
    }
    let mut items = Vec::with_capacity(current.len());
    for item in current {
        upsert(&mut items, item);
    }

    for raw in dedupe_by_id(incoming) {
        let id = id_text(raw);
        if id.is_empty() {
            continue; // cannot merge without an id
        }
        if let Some(current_item) = items.iter_mut().find(|item| item.id == id) {
            if let Some(content) = content_text(raw) {
                current_item.content = cap_content(content.trim());
            }
            if let Some(status) = status_text(raw) {
                current_item.status = status;
            }
        } else {
            upsert(&mut items, validate(raw));
        }
    }
    normalize_order(items)
}

/// Normalize one item, using placeholders when fields are missing, exactly like upstream.
fn validate(item: &Value) -> TodoItem {
    let Some(map) = item.as_object() else {
        return TodoItem {
            id: "?".to_string(),
            content: "(invalid item)".to_string(),
            status: "pending".to_string(),
        };
    };

    let id = map
        .get("id")
        .map(as_text)
        .unwrap_or_default()
        .trim()
        .to_string();
    let id = if id.is_empty() { "?".to_string() } else { id };

    let content = map
        .get("content")
        .map(as_text)
        .unwrap_or_default()
        .trim()
        .to_string();
    let content = if content.is_empty() {
        "(no description)".to_string()
    } else {
        cap_content(&content)
    };

    let status = map
        .get("status")
        .map(as_text)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let status = if VALID_STATUSES.contains(&status.as_str()) {
        status
    } else {
        "pending".to_string()
    };

    TodoItem {
        id,
        content,
        status,
    }
}

/// Collapse duplicate ids, keeping the last occurrence in its position.
fn dedupe_by_id(todos: &[Value]) -> Vec<&Value> {
    let mut last_index: HashMap<String, usize> = HashMap::new();
    for (index, item) in todos.iter().enumerate() {
        let key = match item {
            Value::Object(map) => {
                let raw = map
                    .get("id")
                    .map(as_text)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if raw.is_empty() {
                    "?".to_string()
                } else {
                    raw
                }
            }
            // Non-objects get a synthetic key; validate handles them.
            _ => format!("__invalid_{index}"),
        };
        last_index.insert(key, index);
    }
    let mut indices: Vec<usize> = last_index.into_values().collect();
    indices.sort_unstable();
    indices.into_iter().map(|index| &todos[index]).collect()
}

/// Lift the in_progress step ahead of any earlier pending placeholder. The upstream tool skips
/// this for nested lists; this port has no nesting, so it always applies.
fn normalize_order(items: Vec<TodoItem>) -> Vec<TodoItem> {
    let statuses: Vec<&str> = items.iter().map(|item| item.status.as_str()).collect();
    let Some(active_index) = statuses.iter().position(|status| *status == "in_progress") else {
        return items;
    };
    if !statuses[..active_index].contains(&"pending") {
        return items;
    }
    let Some(pending_index) = statuses.iter().position(|status| *status == "pending") else {
        return items;
    };
    let mut normalized = items;
    let active = normalized.remove(active_index);
    normalized.insert(pending_index, active);
    normalized
}

/// Truncate content longer than the cap, keeping the actionable head plus a marker.
fn cap_content(content: &str) -> String {
    if content.chars().count() > MAX_TODO_CONTENT_CHARS {
        let keep = MAX_TODO_CONTENT_CHARS - TRUNCATION_MARKER.chars().count();
        let head: String = content.chars().take(keep).collect();
        format!("{head}{TRUNCATION_MARKER}")
    } else {
        content.to_string()
    }
}

/// Render the stored list as the upstream JSON payload of todos plus a status summary.
fn render(items: &[TodoItem]) -> String {
    let todos: Vec<Value> = items.iter().map(todo_value).collect();
    let mut summary = serde_json::Map::new();
    summary.insert("total".to_string(), json!(items.len()));
    for status in VALID_STATUSES {
        let count = items.iter().filter(|item| item.status == status).count();
        summary.insert(status.to_string(), json!(count));
    }
    json!({ "todos": todos, "summary": Value::Object(summary) }).to_string()
}

/// The pending and in_progress items, in list order, as a note for a compacted request.
pub fn open_items_note(items: &[TodoItem]) -> Option<String> {
    let open: Vec<String> = items
        .iter()
        .filter(|item| item.status == "pending" || item.status == "in_progress")
        .map(|item| format!("- [{}] {}. {}", item.status, item.id, item.content))
        .collect();
    (!open.is_empty()).then(|| {
        format!(
            "Open todo_list items (call todo_list to see the full list):\n{}",
            open.join("\n")
        )
    })
}

fn todo_value(item: &TodoItem) -> Value {
    json!({
        "id": item.id,
        "content": item.content,
        "status": item.status
    })
}

fn id_text(item: &Value) -> String {
    match item {
        Value::Object(map) => map
            .get("id")
            .map(as_text)
            .unwrap_or_default()
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// The content field only counts as present when it is truthy, matching upstream 'if content'.
fn content_text(item: &Value) -> Option<Cow<'_, str>> {
    let value = item.get("content")?;
    let text = as_text(value);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// A status only counts as present when it is a valid status word, matching upstream.
fn status_text(item: &Value) -> Option<String> {
    let text = as_text(item.get("status")?);
    let lowered = text.trim().to_lowercase();
    if VALID_STATUSES.contains(&lowered.as_str()) {
        Some(lowered)
    } else {
        None
    }
}

/// Best-effort string view of a JSON scalar; strings are used as-is, numbers and booleans are
/// stringified, and null, arrays and objects become empty.
fn as_text(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(text) => text.into(),
        Value::Number(number) => number.to_string().into(),
        Value::Bool(flag) => flag.to_string().into(),
        _ => "".into(),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
