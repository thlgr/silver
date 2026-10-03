//! The lsp tool: diagnostics and code intelligence. rename is Write risk, since callers may apply
//! its edits; a missing server is an error outcome.

use crate::error::CoreResult;
use crate::lsp::{self, LspError, LspManager};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::path::Path;
use std::sync::Arc;

/// The registered tool name.
pub const TOOL_NAME: &str = "lsp";

/// Register the lsp tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(LspTool));
}

struct LspTool;

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Query a language server for code intelligence on a workspace file. Actions: diagnostics \
         (errors/warnings); hover (type/docs at line/character); definition (declaration location); \
         references (every use); symbols (the file's document symbols); rename (the workspace edit \
         renaming the symbol at line/character to query). Needs a language server on PATH \
         (rust-analyzer, pyright/pylsp, typescript-language-server, gopls, clangd) or one under \
         [lsp] servers. Positions are zero-based line and character."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["diagnostics", "hover", "definition", "references", "symbols", "rename"],
                    "description": "The LSP operation to perform."
                },
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to operate on."
                },
                "line": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based line for hover, definition, references and rename."
                },
                "character": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based character for hover, definition, references and rename."
                },
                "query": {
                    "type": "string",
                    "description": "The new symbol name for action=rename."
                }
            },
            "required": ["action", "path"],
            "additionalProperties": false
        })
    }

    fn risk(&self, args: &Value) -> RiskLevel {
        match args.get("action").and_then(Value::as_str) {
            Some("rename") => RiskLevel::Write,
            _ => RiskLevel::Read,
        }
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let workspace = ctx.run.require_workspace()?;
        let Some(action) = args.get("action").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("lsp requires an 'action' string"));
        };
        let Some(path_arg) = args.get("path").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("lsp requires a 'path' string"));
        };
        let path = workspace.resolve_path(Path::new(path_arg))?;
        let workspace_root = &workspace.canonical_root;
        let Some(manager) = ctx.run.services.lsp.as_ref() else {
            return Ok(ToolOutcome::error(
                "LSP is disabled in configuration ([lsp] enabled = false)",
            ));
        };
        let outcome = match action {
            "diagnostics" => run_diagnostics(manager, workspace_root, &path).await,
            "symbols" => run_symbols(manager, workspace_root, &path).await,
            "hover" | "definition" | "references" | "rename" => {
                let Some((line, character)) = position(&args) else {
                    return Ok(ToolOutcome::error(format!(
                        "lsp {action} requires integer 'line' and 'character'"
                    )));
                };
                match action {
                    "hover" => {
                        run_position(manager, workspace_root, &path, line, character, "hover").await
                    }
                    "definition" => {
                        run_position(
                            manager,
                            workspace_root,
                            &path,
                            line,
                            character,
                            "definition",
                        )
                        .await
                    }
                    "references" => {
                        run_position(
                            manager,
                            workspace_root,
                            &path,
                            line,
                            character,
                            "references",
                        )
                        .await
                    }
                    _ => {
                        let Some(query) = args
                            .get("query")
                            .and_then(Value::as_str)
                            .filter(|query| !query.trim().is_empty())
                        else {
                            return Ok(ToolOutcome::error(
                                "lsp rename requires a non-empty 'query' new name",
                            ));
                        };
                        manager
                            .rename(workspace_root, &path, line, character, query)
                            .await
                            .map(|value| format_edit(&value))
                    }
                }
            }
            other => {
                return Ok(ToolOutcome::error(format!("unknown lsp action: {other}")));
            }
        };
        Ok(match outcome {
            Ok(text) => ToolOutcome::ok(text),
            Err(error) => ToolOutcome::error(describe_error(&path, error)),
        })
    }
}

/// Most errors an edit's note lists; the rest are counted.
const MAX_EDIT_ERRORS: usize = 5;

/// Errors a language server reports on the lines an edit changed, as a note for the result. Errors
/// elsewhere predate the edit, and a small model told to fix them wanders off.
pub(crate) async fn edit_errors(
    ctx: &ToolContext<'_>,
    path: &Path,
    before: &str,
    after: &str,
) -> Option<String> {
    let manager = ctx.run.services.lsp.as_ref()?;
    let root = &ctx.run.require_workspace().ok()?.canonical_root;
    let diagnostics = manager.diagnostics(root, path).await.ok()?;
    let errors = errors_on_lines(&diagnostics, &lsp::changed_lines(before, after));
    if errors.is_empty() {
        return None;
    }
    let shown = path.strip_prefix(root).unwrap_or(path).display();
    let mut note = format!(
        "\n{} error(s) on the lines just changed, fix them:",
        errors.len()
    );
    for (line, message) in errors.iter().take(MAX_EDIT_ERRORS) {
        note.push_str(&format!("\n{shown}:{}: {message}", line + 1));
    }
    if errors.len() > MAX_EDIT_ERRORS {
        note.push_str(&format!("\n...and {} more", errors.len() - MAX_EDIT_ERRORS));
    }
    Some(note)
}

/// Error-severity diagnostics that start on a changed line, as (0-indexed line, first line of
/// the message).
fn errors_on_lines(diagnostics: &[Value], changed: &[std::ops::Range<i64>]) -> Vec<(i64, String)> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.get("severity").and_then(Value::as_u64) == Some(1))
        .filter_map(|diagnostic| {
            let line = diagnostic.pointer("/range/start/line")?.as_i64()?;
            let message = diagnostic.get("message")?.as_str()?.lines().next()?;
            changed
                .iter()
                .any(|range| range.contains(&line))
                .then(|| (line, message.to_string()))
        })
        .collect()
}

fn position(args: &Value) -> Option<(u32, u32)> {
    let line = args.get("line").and_then(Value::as_u64)?;
    let character = args.get("character").and_then(Value::as_u64)?;
    Some((line as u32, character as u32))
}

async fn run_diagnostics(
    manager: &LspManager,
    workspace_root: &Path,
    path: &Path,
) -> Result<String, LspError> {
    let diagnostics = manager.diagnostics(workspace_root, path).await?;
    Ok(format_diagnostics(path, &diagnostics))
}

async fn run_symbols(
    manager: &LspManager,
    workspace_root: &Path,
    path: &Path,
) -> Result<String, LspError> {
    let value = manager.document_symbols(workspace_root, path).await?;
    Ok(format_symbols(path, &value))
}

async fn run_position(
    manager: &LspManager,
    workspace_root: &Path,
    path: &Path,
    line: u32,
    character: u32,
    action: &str,
) -> Result<String, LspError> {
    match action {
        "hover" => manager
            .hover(workspace_root, path, line, character)
            .await
            .map(|value| format_hover(path, value)),
        "definition" => manager
            .definition(workspace_root, path, line, character)
            .await
            .map(|value| format_locations(path, &value)),
        _ => manager
            .references(workspace_root, path, line, character)
            .await
            .map(|value| format_locations(path, &value)),
    }
}

/// A clear, actionable message, especially when no server is available.
fn describe_error(path: &Path, error: LspError) -> String {
    match error {
        LspError::NoServer => format!(
            "no LSP server is available for {}. Install one of rust-analyzer, pyright or pylsp, \
             typescript-language-server, gopls or clangd on PATH, or name a server under \
             [lsp] servers.",
            path.display()
        ),
        LspError::Disabled => {
            "LSP is disabled in configuration ([lsp] enabled = false)".to_string()
        }
        other => format!("lsp: {other}"),
    }
}

fn severity_name(severity: u64) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "information",
        4 => "hint",
        _ => "diagnostic",
    }
}

fn format_diagnostics(path: &Path, diagnostics: &[Value]) -> String {
    if diagnostics.is_empty() {
        return format!("{}: no diagnostics", path.display());
    }
    let mut out = format!("{}: {} diagnostic(s)\n", path.display(), diagnostics.len());
    for diagnostic in diagnostics {
        let range = diagnostic.get("range");
        let line = range
            .and_then(|range| range.pointer("/start/line"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let character = range
            .and_then(|range| range.pointer("/start/character"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let severity = severity_name(
            diagnostic
                .get("severity")
                .and_then(Value::as_u64)
                .unwrap_or(1),
        );
        let message = diagnostic
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        out.push_str(&format!("{line}:{character}: {severity}: {message}"));
        if let Some(source) = diagnostic.get("source").and_then(Value::as_str) {
            out.push_str(&format!(" [{source}]"));
        }
        if let Some(code) = diagnostic.get("code") {
            let code = code
                .as_str()
                .map(str::to_string)
                .or_else(|| code.as_u64().map(|number| number.to_string()));
            if let Some(code) = code {
                out.push_str(&format!(" ({code})"));
            }
        }
        out.push('\n');
    }
    out
}

/// A MarkedString or MarkupContent as text: the string itself, or an object's `value`.
fn markup_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Object(mut map) => match map.get_mut("value") {
            Some(Value::String(text)) => std::mem::take(text),
            _ => Value::Object(map).to_string(),
        },
        other => other.to_string(),
    }
}

fn format_hover(path: &Path, mut value: Value) -> String {
    let text = match value["contents"].take() {
        Value::Array(items) => items
            .into_iter()
            .map(markup_text)
            .collect::<Vec<_>>()
            .join("\n"),
        contents => markup_text(contents),
    };
    if text.trim().is_empty() {
        return format!("{}: no hover information", path.display());
    }
    text
}

fn format_range(range: Option<&Value>) -> String {
    let Some(range) = range else {
        return String::new();
    };
    let line = range
        .pointer("/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let character = range
        .pointer("/start/character")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    format!(":{line}:{character}")
}

fn format_locations(path: &Path, value: &Value) -> String {
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    };
    if items.is_empty() {
        return format!("{}: no results", path.display());
    }
    let mut out = format!("{}: {} result(s)\n", path.display(), items.len());
    for item in items {
        if let Some(target) = item.get("targetUri").and_then(Value::as_str) {
            let range = item
                .get("targetSelectionRange")
                .or_else(|| item.get("targetRange"));
            out.push_str(&format!("  {target}{}\n", format_range(range)));
        } else if let Some(uri) = item.get("uri").and_then(Value::as_str) {
            out.push_str(&format!("  {uri}{}\n", format_range(item.get("range"))));
        } else {
            out.push_str(&format!("  {item}\n"));
        }
    }
    out
}

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

fn collect_symbol(symbol: &Value, depth: usize, out: &mut String) {
    let name = symbol.get("name").and_then(Value::as_str).unwrap_or("");
    let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
    let line = symbol
        .pointer("/range/start/line")
        .or_else(|| symbol.pointer("/location/range/start/line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let indent = "  ".repeat(depth);
    out.push_str(&format!("{indent}{kind} {name} (line {line})\n"));
    if let Some(children) = symbol.get("children").and_then(Value::as_array) {
        for child in children {
            collect_symbol(child, depth + 1, out);
        }
    }
}

fn format_symbols(path: &Path, value: &Value) -> String {
    let items = value.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return format!("{}: no symbols", path.display());
    }
    let mut out = format!("{}: {} symbol(s)\n", path.display(), items.len());
    for item in &items {
        collect_symbol(item, 0, &mut out);
    }
    out
}

fn format_edit(value: &Value) -> String {
    if value.is_null() {
        return "rename produced no edit".to_string();
    }
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}
