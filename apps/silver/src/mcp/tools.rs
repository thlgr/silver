//! MCP tool registration: naming, schema normalization, injection scanning, risk classification
//! and CallToolResult rendering. Media is reported inline rather than cached.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use silver_core::error::CoreResult;
use silver_core::tool::{Tool, ToolContext, ToolOutcome};
use silver_core::toolset;
use silver_protocol::RiskLevel;

use super::client::{McpServerConnection, McpToolDef};
use super::{
    sanitize_error_text, strip_unicode_tags, truncate_head_tail, MCP_HARD_RESULT_CAP_CHARS,
    MCP_TOOL_NAME_MAX_LENGTH,
};

/// Replace every char outside [A-Za-z0-9_] with _ (mcp_tool_schema.sanitize_mcp_name_component).
pub fn sanitize_mcp_name_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Registry/wire name: <sanitizedServer>__<sanitizedTool>, clamped to 64 chars with a
/// deterministic hash suffix when the natural name is longer
/// (mcp_tool_schema.mcp_prefixed_tool_name, with the port's <server>__<tool> form).
pub fn mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    let full = format!(
        "{}__{}",
        sanitize_mcp_name_component(server_name),
        sanitize_mcp_name_component(tool_name)
    );
    if full.len() <= MCP_TOOL_NAME_MAX_LENGTH {
        return full;
    }
    let suffix = format!("_{:08x}", fnv1a32(&full));
    let keep = MCP_TOOL_NAME_MAX_LENGTH - suffix.len();
    format!("{}{}", &full[..keep], suffix)
}

/// Deterministic FNV-1a hash (Rust port of the SHA-256-based clamp; the invariant preserved is
/// a stable, collision-resistant-enough suffix with no extra dependency).
fn fnv1a32(value: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn empty_object_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

// ---------------------------------------------------------------- schema normalization

/// Normalize an MCP input schema so one form works on OpenAI, Anthropic, Gemini and Moonshot.
/// Order matters: definitions -> $defs, nullable unions collapsed, const unions to enum, repair.
pub fn normalize_mcp_input_schema(schema: Value) -> Value {
    if !schema.as_object().is_some_and(|object| !object.is_empty()) {
        return empty_object_schema();
    }
    let normalized = rewrite_local_refs(schema);
    let normalized = strip_nullable_unions(normalized);
    let normalized = collapse_const_unions(normalized);
    let mut normalized = repair_object_shape(normalized);
    if let Value::Object(map) = &mut normalized {
        if map.get("type").and_then(Value::as_str) == Some("object")
            && !map.contains_key("properties")
        {
            map.insert("properties".to_string(), Value::Object(Map::new()));
        }
    }
    if normalized.is_object() {
        normalized
    } else {
        empty_object_schema()
    }
}

fn rewrite_schema<F: Fn(Value) -> Value>(node: Value, apply: &F) -> Value {
    match node {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| rewrite_schema(item, apply))
                .collect(),
        ),
        Value::Object(map) => apply(Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, rewrite_schema(value, apply)))
                .collect(),
        )),
        other => other,
    }
}

/// Promote legacy definitions to $defs, but never as a property NAME
/// (mcp_tool_schema._rewrite_local_refs).
fn rewrite_local_refs(node: Value) -> Value {
    match node {
        Value::Array(items) => Value::Array(items.into_iter().map(rewrite_local_refs).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                match value {
                    Value::Object(props) if key == "properties" || key == "patternProperties" => {
                        let nested = props
                            .into_iter()
                            .map(|(name, prop)| (name, rewrite_local_refs(prop)))
                            .collect();
                        out.insert(key, Value::Object(nested));
                    }
                    value if key == "definitions" => {
                        out.insert("$defs".to_string(), rewrite_local_refs(value));
                    }
                    value => {
                        out.insert(key, rewrite_local_refs(value));
                    }
                }
            }
            if let Some(Value::String(reference)) = out.get("$ref") {
                if let Some(rest) = reference.strip_prefix("#/definitions/") {
                    out.insert("$ref".to_string(), Value::String(format!("#/$defs/{rest}")));
                }
            }
            Value::Object(out)
        }
        other => other,
    }
}

const SCHEMA_MAP_KEYS: [&str; 5] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Recursively fill a missing object type, ensure properties and prune required
/// (mcp_tool_schema._repair_object_shape).
fn repair_object_shape(node: Value) -> Value {
    match node {
        Value::Array(items) => Value::Array(items.into_iter().map(repair_object_shape).collect()),
        Value::Object(map) => {
            let mut repaired = Map::new();
            for (key, value) in map {
                let value = match value {
                    Value::Object(schemas) if SCHEMA_MAP_KEYS.contains(&key.as_str()) => {
                        Value::Object(
                            schemas
                                .into_iter()
                                .map(|(name, schema)| (name, repair_object_shape(schema)))
                                .collect(),
                        )
                    }
                    value => repair_object_shape(value),
                };
                repaired.insert(key, value);
            }
            if repaired.get("type").is_none()
                && (repaired.contains_key("properties") || repaired.contains_key("required"))
            {
                repaired.insert("type".to_string(), Value::String("object".to_string()));
            }
            if repaired.get("type").and_then(Value::as_str) == Some("object") {
                if !repaired
                    .get("properties")
                    .map(Value::is_object)
                    .unwrap_or(false)
                {
                    repaired.insert("properties".to_string(), Value::Object(Map::new()));
                }
                if let Some(Value::Array(required)) = repaired.get("required") {
                    let properties = repaired
                        .get("properties")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let valid: Vec<Value> = required
                        .iter()
                        .filter(|item| {
                            item.as_str()
                                .map(|name| properties.contains_key(name))
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    if valid.len() != required.len() {
                        if valid.is_empty() {
                            repaired.remove("required");
                        } else {
                            repaired.insert("required".to_string(), Value::Array(valid));
                        }
                    }
                }
            }
            Value::Object(repaired)
        }
        other => other,
    }
}

const UNION_KEYS: [&str; 2] = ["anyOf", "oneOf"];
const UNION_META_KEYS: [&str; 4] = ["title", "description", "default", "examples"];

fn is_null_branch(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("null")
}

fn carry_union_meta(
    outer: &Map<String, Value>,
    replacement: &mut Map<String, Value>,
    skip_default_on_ref: bool,
) {
    for meta_key in UNION_META_KEYS {
        if outer.contains_key(meta_key) && !replacement.contains_key(meta_key) {
            let skip =
                skip_default_on_ref && meta_key == "default" && replacement.contains_key("$ref");
            if !skip {
                if let Some(value) = outer.get(meta_key) {
                    replacement.insert(meta_key.to_string(), Value::clone(value));
                }
            }
        }
    }
}

/// Collapse anyOf/oneOf nullable unions to the single non-null branch, keeping nullable: true
/// (schema_sanitizer.strip_nullable_unions with keep_nullable_hint=True).
pub fn strip_nullable_unions(schema: Value) -> Value {
    rewrite_schema(schema, &strip_nullable_collapse)
}

fn strip_nullable_collapse(value: Value) -> Value {
    let Some(map) = value.as_object() else {
        return value;
    };
    for key in UNION_KEYS {
        let Some(variants) = map.get(key).and_then(Value::as_array) else {
            continue;
        };
        let non_null: Vec<&Value> = variants
            .iter()
            .filter(|item| !is_null_branch(item))
            .collect();
        if non_null.len() == 1 && non_null.len() != variants.len() {
            let mut replacement = non_null[0].as_object().cloned().unwrap_or_default();
            replacement
                .entry("nullable".to_string())
                .or_insert(Value::Bool(true));
            carry_union_meta(map, &mut replacement, true);
            return rewrite_schema(Value::Object(replacement), &strip_nullable_collapse);
        }
    }
    value
}

/// Collapse anyOf/oneOf unions of same-typed consts to an enum
/// (schema_sanitizer.collapse_const_unions, ported from block/goose).
pub fn collapse_const_unions(schema: Value) -> Value {
    rewrite_schema(schema, &collapse_const_collapse)
}

fn collapse_const_collapse(value: Value) -> Value {
    let Some(map) = value.as_object() else {
        return value;
    };
    for key in UNION_KEYS {
        let Some(variants) = map.get(key).and_then(Value::as_array) else {
            continue;
        };
        if variants.is_empty() {
            continue;
        }
        let null_count = variants
            .iter()
            .filter(|item| is_null_branch(item) && item.get("const").is_none())
            .count();
        let const_branches: Vec<&Value> = variants
            .iter()
            .filter(|item| !(is_null_branch(item) && item.get("const").is_none()))
            .collect();
        if null_count > 1 || const_branches.is_empty() {
            continue;
        }
        let types: Vec<Option<&'static str>> = const_branches
            .iter()
            .map(|item| const_branch_type(item))
            .collect();
        let Some(first) = types.first().copied().flatten() else {
            continue;
        };
        if types.iter().any(|item| *item != Some(first)) {
            continue;
        }
        let enum_values: Vec<Value> = const_branches
            .iter()
            .map(|item| item.get("const").cloned().unwrap_or(Value::Null))
            .collect();
        let mut replacement = Map::new();
        replacement.insert("type".to_string(), Value::String(first.to_string()));
        replacement.insert("enum".to_string(), Value::Array(enum_values));
        if null_count > 0 {
            replacement.insert("nullable".to_string(), Value::Bool(true));
        }
        carry_union_meta(map, &mut replacement, false);
        return Value::Object(replacement);
    }
    value
}

fn const_branch_type(branch: &Value) -> Option<&'static str> {
    let map = branch.as_object()?;
    let constant = map.get("const")?;
    if map
        .keys()
        .any(|key| !matches!(key.as_str(), "const" | "type" | "title" | "description"))
    {
        return None;
    }
    let json_type = match constant {
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        _ => return None,
    };
    match map.get("type").and_then(Value::as_str) {
        None => Some(json_type),
        Some(declared) if declared == json_type => Some(json_type),
        _ => None,
    }
}

// ---------------------------------------------------------------- injection scanning

/// Scan a tool description for prompt-injection indicators
/// (mcp_tool_schema._scan_mcp_description). WARNING-level only: findings are returned and
/// logged, never used to block.
pub fn scan_description(
    server_name: &str,
    tool_name: &str,
    description: &str,
) -> Vec<&'static str> {
    if description.is_empty() {
        return Vec::new();
    }
    let normalized = normalize_for_scan(description);
    let compact: String = normalized
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let mut findings = Vec::new();

    if normalized.contains("ignore previous instructions")
        || normalized.contains("ignore all previous instructions")
    {
        findings.push("prompt override attempt ('ignore previous instructions')");
    }
    if normalized.contains("you are now a") {
        findings.push("identity override attempt ('you are now a...')");
    }
    let new_task = [
        "your new task is",
        "your new tasks are",
        "your new role is",
        "your new roles are",
        "your new instruction is",
        "your new instructions are",
    ];
    if new_task.iter().any(|needle| normalized.contains(needle)) {
        findings.push("task override attempt");
    }
    if normalized.contains("system:") || normalized.contains("system :") {
        findings.push("system prompt injection attempt");
    }
    if compact.contains("<system>")
        || compact.contains("<human>")
        || compact.contains("<assistant>")
    {
        findings.push("role tag injection attempt");
    }
    if [
        "do not tell",
        "do not inform",
        "do not mention",
        "do not reveal",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        findings.push("concealment instruction");
    }
    if [
        "curl http://",
        "curl https://",
        "wget http://",
        "wget https://",
        "fetch http://",
        "fetch https://",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        findings.push("network command in description");
    }
    if normalized.contains("base64.b64decode") || normalized.contains("base64.decodebytes") {
        findings.push("base64 decode reference");
    }
    if compact.contains("exec(") || compact.contains("eval(") {
        findings.push("code execution reference");
    }
    if [
        "import subprocess",
        "import os",
        "import shutil",
        "import socket",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        findings.push("dangerous import reference");
    }
    if !findings.is_empty() {
        tracing::warn!(
            server = %server_name,
            tool = %tool_name,
            findings = %findings.join("; "),
            description = %description.chars().take(200).collect::<String>(),
            "MCP tool description contains suspicious content (treated as untrusted)"
        );
    }
    findings
}

/// Lowercase and collapse whitespace runs so regex \s+ patterns become literal single spaces.
fn normalize_for_scan(description: &str) -> String {
    description
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- result rendering

/// Pure: a CallToolResult JSON object -> ToolOutcome (mcp_tool_handlers._render_call_tool_result).
/// content and structuredContent are alternatives, never both forwarded.
pub fn render_call_tool_result(result: &Value, server_name: &str) -> ToolOutcome {
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        let text = error_result_text(result);
        let text = if text.is_empty() {
            "MCP tool returned an error".to_string()
        } else {
            text
        };
        let text = truncate_head_tail(&text, MCP_HARD_RESULT_CAP_CHARS, "MCP RESULT");
        return ToolOutcome::error(sanitize_error_text(&text));
    }
    let (text_result, usable_parts) = render_content_blocks(result, server_name);
    let mut structured = capped_structured_content(result);
    if structured.is_some() && usable_parts > 0 {
        structured = None;
    }
    let content = match structured {
        Some(structured) => {
            let serialized = structured_to_text(structured);
            if text_result.trim().is_empty() {
                serialized
            } else {
                format!("{text_result}\n\n{serialized}")
            }
        }
        None => text_result,
    };
    ToolOutcome::ok(content)
}

fn structured_to_text(structured: Value) -> String {
    match structured {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

fn error_result_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                out.push_str(text);
            } else if let Some(text) = block
                .get("resource")
                .and_then(|resource| resource.get("text"))
                .and_then(Value::as_str)
            {
                out.push_str(text);
            }
        }
    }
    out
}

fn render_content_blocks(result: &Value, server_name: &str) -> (String, usize) {
    let mut parts: Vec<String> = Vec::new();
    let mut usable_parts = 0usize;
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    parts.push(strip_unicode_tags(text));
                    if !text.trim().is_empty() {
                        usable_parts += 1;
                    }
                    continue;
                }
            }
            if let Some(rendered) = render_resource_block(block, server_name) {
                parts.push(rendered);
                usable_parts += 1;
                continue;
            }
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if matches!(block_type, "text" | "resource" | "audio" | "image") {
                continue; // benign empty render
            }
            parts.push(render_dropped_block_notice(block, block_type));
        }
    }
    let joined = parts.join("\n");
    (
        truncate_head_tail(&joined, MCP_HARD_RESULT_CAP_CHARS, "MCP RESULT"),
        usable_parts,
    )
}

fn render_resource_block(block: &Value, server_name: &str) -> Option<String> {
    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
    let looks_like_link = block_type == "resource_link"
        || (block.get("uri").is_some() && block.get("resource").is_none() && block_type != "text");
    if looks_like_link {
        let uri = block.get("uri").and_then(Value::as_str)?;
        if uri.is_empty() {
            return None;
        }
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        let mime = block
            .get("mimeType")
            .or_else(|| block.get("mime_type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut details = format!("uri={uri}");
        if !name.is_empty() {
            details.push_str(&format!(", name={name}"));
        }
        if !mime.is_empty() {
            details.push_str(&format!(", mimeType={mime}"));
        }
        return Some(format!(
            "[MCP resource link: {details} — fetch it with the MCP server {server_name} read_resource tool]"
        ));
    }
    let resource = block.get("resource")?;
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        return Some(strip_unicode_tags(text));
    }
    if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
        let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
        let mime = resource
            .get("mimeType")
            .or_else(|| resource.get("mime_type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let approx_bytes = blob.len() as u64 * 3 / 4;
        let kind = if mime.is_empty() { uri } else { mime };
        return Some(format!(
            "[MCP embedded resource received (~{approx_bytes} bytes, {kind}) but media caching is unavailable in this client]"
        ));
    }
    None
}

fn render_dropped_block_notice(block: &Value, block_type: &str) -> String {
    let mut details = vec![format!("type={block_type}")];
    if let Some(mime) = block
        .get("mimeType")
        .or_else(|| block.get("mime_type"))
        .and_then(Value::as_str)
    {
        details.push(format!("mimeType={mime}"));
    }
    let uri = block.get("uri").and_then(Value::as_str).or_else(|| {
        block
            .get("resource")
            .and_then(|resource| resource.get("uri"))
            .and_then(Value::as_str)
    });
    if let Some(uri) = uri {
        details.push(format!("uri={uri}"));
    }
    for key in ["size", "sizeInBytes"] {
        if let Some(size) = block.get(key).and_then(Value::as_i64) {
            details.push(format!("size={size}"));
            break;
        }
    }
    if let Some(name) = block.get("name").and_then(Value::as_str) {
        details.push(format!("name={name}"));
    }
    format!(
        "[MCP content dropped: unsupported block ({})]",
        details.join(", ")
    )
}

fn capped_structured_content(result: &Value) -> Option<Value> {
    let structured = result
        .get("structuredContent")
        .or_else(|| result.get("structured_content"))?;
    if structured.is_null() {
        return None;
    }
    let serialized = serde_json::to_string(structured).unwrap_or_default();
    if serialized.chars().count() > MCP_HARD_RESULT_CAP_CHARS {
        Some(Value::String(truncate_head_tail(
            &serialized,
            MCP_HARD_RESULT_CAP_CHARS,
            "MCP RESULT",
        )))
    } else {
        Some(Value::clone(structured))
    }
}

// ---------------------------------------------------------------- registry tool

/// One MCP server tool. Its description, schema and results are attacker-controlled, so it
/// defaults to Write/Process risk and passes the approval gate.
pub struct McpTool {
    registry_name: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
    tool_name: String,
    connection: Arc<McpServerConnection>,
}

impl McpTool {
    /// Build the registry-facing tool from one discovered definition.
    pub fn new(connection: Arc<McpServerConnection>, def: McpToolDef) -> Self {
        let registry_name = mcp_tool_name(connection.name(), &def.name);
        let description = if def.description.trim().is_empty() {
            format!("MCP tool {} from {}", def.name, connection.name())
        } else {
            def.description
        };
        let description = strip_unicode_tags(&description);
        let _findings = scan_description(connection.name(), &def.name, &description);
        let schema = normalize_mcp_input_schema(def.input_schema);
        Self {
            registry_name: leak(registry_name),
            description: leak(description),
            schema,
            read_only: def.read_only,
            tool_name: def.name,
            connection,
        }
    }

    /// The raw MCP tool name (the server's own name).
    pub fn mcp_name(&self) -> &str {
        &self.tool_name
    }

    /// The name the registry and model see.
    pub fn registry_name(&self) -> &str {
        self.registry_name
    }

    /// Conservative risk: Read only when the server's readOnlyHint is exactly true or the name
    /// is unambiguously read-only; Process for process-like names; Write otherwise.
    fn risk_level(&self) -> RiskLevel {
        if self.read_only || looks_read_only(&self.tool_name) {
            RiskLevel::Read
        } else if looks_process(&self.tool_name) {
            RiskLevel::Process
        } else {
            RiskLevel::Write
        }
    }
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

const READ_ONLY_VERBS: [&str; 22] = [
    "read",
    "get",
    "list",
    "search",
    "query",
    "fetch",
    "find",
    "describe",
    "show",
    "view",
    "inspect",
    "status",
    "info",
    "lookup",
    "resolve",
    "head",
    "stat",
    "cat",
    "ls",
    "enumerate",
    "discover",
    "browse",
];

const PROCESS_VERBS: [&str; 14] = [
    "exec", "execute", "run", "shell", "sh", "bash", "command", "cmd", "process", "spawn", "kill",
    "terminal", "launch", "restart",
];

/// First camelCase/snake_case token of a tool name, lowercased.
fn first_token(name: &str) -> String {
    let mut token = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && !token.is_empty() {
                break;
            }
            token.push(ch.to_ascii_lowercase());
        } else if token.is_empty() {
            continue;
        } else {
            break;
        }
    }
    token
}

/// True when the leading token is a read-only verb and no token is write-capable. A name like
/// get_or_create or getOrCreate is treated as write-capable (conservative).
pub fn looks_read_only(tool_name: &str) -> bool {
    let tokens = name_tokens(tool_name);
    let Some(first) = tokens.first() else {
        return false;
    };
    READ_ONLY_VERBS.contains(&first.as_str())
        && !tokens
            .iter()
            .any(|token| WRITE_VERBS.contains(&token.as_str()))
}

const WRITE_VERBS: [&str; 27] = [
    "create", "write", "update", "delete", "set", "put", "post", "add", "remove", "modify",
    "patch", "upsert", "import", "upload", "send", "insert", "save", "apply", "enable", "disable",
    "start", "stop", "restart", "exec", "execute", "run", "spawn",
];

/// Split a tool name into lowercase tokens on non-alphanumerics and camelCase boundaries.
fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// True when the leading token signals process/shell execution.
pub fn looks_process(tool_name: &str) -> bool {
    let token = first_token(tool_name);
    PROCESS_VERBS.contains(&token.as_str())
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.registry_name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        Value::clone(&self.schema)
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        self.risk_level()
    }

    fn toolset(&self) -> &'static str {
        toolset::MCP
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        Ok(self
            .connection
            .call_tool(&self.tool_name, args, self.read_only)
            .await)
    }
}
