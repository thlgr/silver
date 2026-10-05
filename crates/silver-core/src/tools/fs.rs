//! Read-only workspace filesystem tools: read_file, list_files and search_files.

use crate::context::RunContext;
use crate::error::{CoreError, CoreResult};
use crate::redact::{redact, redact_source};
use crate::safety;
use crate::tool::{glob_matches, Tool, ToolContext, ToolOutcome, ToolRegistry};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Maximum number of bytes read_file returns before it notes truncation.
const MAX_READ_BYTES: usize = 262_144;
/// Maximum number of entries list_files returns.
const MAX_LIST_ENTRIES: usize = 2_000;
/// Directory names that traversal never descends into: VCS data, dependencies and caches,
/// whose files would bury the project's own in listings and search results.
const SKIP_DIRS: [&str; 6] = [
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
];
/// Default recursion depth for list_files.
const DEFAULT_LIST_DEPTH: usize = 2;
/// Hard recursion cap for list_files.
const MAX_LIST_DEPTH: usize = 8;
/// Default result cap for search_files.
const DEFAULT_MAX_RESULTS: usize = 50;
/// Hard result cap for search_files.
const MAX_MAX_RESULTS: usize = 500;
/// Files larger than this are skipped by search_files.
const MAX_SEARCH_FILE_BYTES: u64 = 1_048_576;

/// Register the three read-only filesystem tools.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ReadFile));
    registry.register(Arc::new(ListFiles));
    registry.register(Arc::new(SearchFiles));
}

/// A read-denied tool result carrying the stable `read_denied` code.
pub(crate) fn read_denied(path: &Path, reason: &str) -> ToolOutcome {
    let message = format!("Access denied: {} {reason}", path.display());
    ToolOutcome::error(safety::denied_error_body(
        safety::READ_DENIED_CODE,
        path,
        &message,
    ))
}

/// Render a path relative to the workspace root with forward slashes.
pub(crate) fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Read a required string argument.
pub(crate) fn required_str(args: &Value, key: &str) -> CoreResult<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CoreError::InvalidRequest(format!("'{key}' must be a string")))
}

/// Read an optional string argument.
fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Read an optional non-negative integer argument.
fn optional_usize(args: &Value, key: &str) -> CoreResult<Option<usize>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(|n| Some(n as usize)).ok_or_else(|| {
            CoreError::InvalidRequest(format!("'{key}' must be a non-negative integer"))
        }),
    }
}

/// Trim a string to at most max bytes on a UTF-8 boundary.
fn cap_bytes(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// Runs a tool's filesystem work on the blocking pool. Inline, a long walk never yields, so
/// neither a stop nor the tool timeout could end it. `ctx.cancel` is cancelled once the tool's
/// future is dropped, so the walk can give up as well.
pub(crate) async fn off_runtime(
    ctx: &ToolContext<'_>,
    args: Value,
    work: fn(&RunContext, &CancellationToken, &Value) -> CoreResult<ToolOutcome>,
) -> CoreResult<ToolOutcome> {
    let run = Arc::clone(&ctx.run);
    let stop = ctx.cancel.child_token();
    let cancel = stop.child_token();
    let _stop = stop.drop_guard();
    tokio::task::spawn_blocking(move || work(&run, &cancel, &args))
        .await
        .map_err(|err| CoreError::Internal(err.to_string()))?
}

/// Sorted directory entries as (path, is_dir, is_symlink).
fn read_sorted(dir: &Path) -> Vec<(PathBuf, bool, bool)> {
    let mut items: Vec<(PathBuf, bool, bool)> = match std::fs::read_dir(dir) {
        Ok(read) => read
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                Some((entry.path(), file_type.is_dir(), file_type.is_symlink()))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

/// The read_file tool.
pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace. Use offset and limit to page through large files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path, relative to the workspace root or absolute inside it."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based first line to return."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        off_runtime(ctx, args, read_file).await
    }
}

fn read_file(
    run: &RunContext,
    _cancel: &CancellationToken,
    args: &Value,
) -> CoreResult<ToolOutcome> {
    let requested = required_str(args, "path")?;
    // The NT/device-namespace guard runs on the raw string before any resolution.
    if let Some(reason) = safety::nt_namespace_error(Path::new(&requested)) {
        return Ok(read_denied(Path::new(&requested), reason));
    }
    let resolved = run.resolve_path(&requested)?;
    if let Some(reason) = safety::read_denied_reason(&resolved) {
        return Ok(read_denied(&resolved, reason));
    }

    let bytes = match std::fs::read(&resolved) {
        Ok(bytes) => bytes,
        Err(_) if resolved.is_dir() => {
            return Ok(ToolOutcome::error(format!(
                "{requested} is a directory; use list_files to see what is inside it"
            )))
        }
        Err(error) => {
            return Ok(ToolOutcome::error(format!(
                "cannot read {}: {error}",
                resolved.display()
            )))
        }
    };
    let Some(content) = extract_text(&resolved, &bytes) else {
        return Ok(ToolOutcome::error(format!(
            "{requested} is not a UTF-8 text file or a PDF, so read_file cannot show it"
        )));
    };

    let offset = optional_usize(args, "offset")?.unwrap_or(1).max(1);
    let limit = optional_usize(args, "limit")?.map(|value| value.max(1));
    let lines: Vec<&str> = content.lines().collect();
    // An empty result reads as "the file is empty", so a small model would stop paging
    // on a wrong offset instead of fixing it.
    if offset > lines.len() && !lines.is_empty() {
        return Ok(ToolOutcome::error(format!(
            "offset {offset} is past the end of {requested}, which has {} lines",
            lines.len()
        )));
    }
    let start = offset.saturating_sub(1).min(lines.len());
    let end = match limit {
        Some(limit) => start.saturating_add(limit).min(lines.len()),
        None => lines.len(),
    };
    // Cut at a whole line and say where to resume, so a small model can page on.
    let mut selected = String::new();
    let mut shown = 0;
    for line in &lines[start..end] {
        if shown > 0 && selected.len() + 1 + line.len() > MAX_READ_BYTES {
            break;
        }
        if shown > 0 {
            selected.push('\n');
        }
        selected.push_str(line);
        shown += 1;
    }
    let (mut body, truncated) = cap_bytes(selected, MAX_READ_BYTES);
    let last = start + shown;
    if last < end {
        body.push_str(&format!(
            "\n[showing lines {}-{last} of {}; continue with offset={}]",
            start + 1,
            lines.len(),
            last + 1
        ));
    } else if truncated {
        body.push_str("\n[truncated]");
    }
    // Secret-bearing reads are fully scrubbed; ordinary source reads use the source-safe
    // pass so fixtures such as a typed api_key field are not rewritten while hardcoded
    // credentials are still masked.
    let body = if is_source_code(&resolved) {
        redact_source(&body)
    } else {
        redact(&body)
    };
    Ok(ToolOutcome::ok(body))
}

/// A file's readable text: its own bytes when they are UTF-8, otherwise the text pdf-extract
/// pulls out of a PDF. None when neither works, so a binary blob stays unreadable.
pub(crate) fn extract_text(path: &Path, bytes: &[u8]) -> Option<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_string());
    }
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        return None;
    }
    extract_pdf(bytes)
}

/// PDF text via pdf-extract; None without the `pdf` feature, so a PDF stays as unreadable as
/// any other binary blob in a fast `--no-default-features` build.
#[cfg(feature = "pdf")]
fn extract_pdf(bytes: &[u8]) -> Option<String> {
    pdf_extract::extract_text_from_mem(bytes)
        .ok()
        .filter(|text| !text.trim().is_empty())
}

#[cfg(not(feature = "pdf"))]
fn extract_pdf(_bytes: &[u8]) -> Option<String> {
    None
}

/// Extensions treated as ordinary source code, where the assignment/YAML passes would rewrite
/// legitimate fixtures. Everything else (config, logs, env dumps and unknown text) is fully
/// scrubbed. A file whose name itself names a credential is never treated as ordinary source.
fn is_source_code(path: &Path) -> bool {
    if looks_like_secret_file(path) {
        return false;
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "rs" | "py"
            | "pyi"
            | "js"
            | "mjs"
            | "cjs"
            | "jsx"
            | "ts"
            | "tsx"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hh"
            | "rb"
            | "php"
            | "cs"
            | "swift"
            | "scala"
            | "clj"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "lua"
            | "pl"
            | "r"
            | "sql"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "vue"
            | "svelte"
            | "dart"
            | "gradle"
    )
}

/// True when a file name itself names a credential store, so a source extension cannot opt it
/// out of full scrubbing.
fn looks_like_secret_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.starts_with(".env")
        || name.contains("secret")
        || name.contains("credential")
        || name.contains("password")
        || name.contains("api_key")
        || name.contains("apikey")
        || matches!(
            name.as_str(),
            ".npmrc"
                | ".pypirc"
                | ".netrc"
                | ".git-credentials"
                | ".htpasswd"
                | "id_rsa"
                | "id_ed25519"
                | "id_ecdsa"
                | "id_dsa"
        )
}

/// The list_files tool.
pub struct ListFiles;

#[async_trait::async_trait]
impl Tool for ListFiles {
    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> &'static str {
        "List files and directories inside the workspace, recursively up to depth. Directories end in '/'. Skips .git, target, node_modules, .venv, venv and __pycache__."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list, relative to the workspace root. Defaults to the root."
                },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 8,
                    "description": "Levels to list: 1 = only this directory's entries. Defaults to 2."
                }
            },
            "additionalProperties": false
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        off_runtime(ctx, args, list_files).await
    }
}

fn list_files(
    run: &RunContext,
    _cancel: &CancellationToken,
    args: &Value,
) -> CoreResult<ToolOutcome> {
    let requested = optional_str(args, "path").unwrap_or_else(|| ".".to_string());
    let resolved = run.resolve_path(&requested)?;
    let root = &run.require_workspace()?.canonical_root;
    let depth = optional_usize(args, "depth")?
        .unwrap_or(DEFAULT_LIST_DEPTH)
        // A model asking for depth 0 means "just this directory", not "nothing".
        .clamp(1, MAX_LIST_DEPTH);

    if resolved.is_file() {
        return Ok(ToolOutcome::ok(relative_to(root, &resolved)));
    }
    if !resolved.exists() {
        return Ok(ToolOutcome::error(format!(
            "{} does not exist",
            resolved.display()
        )));
    }

    let mut entries: Vec<String> = Vec::new();
    let mut truncated = false;
    collect_entries(&resolved, root, depth, &mut entries, &mut truncated);

    let mut body = entries.join("\n");
    if body.is_empty() && !truncated {
        body = format!("{requested} is empty");
    }
    if truncated {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("[truncated]");
    }
    Ok(ToolOutcome::ok(body))
}

/// Recurse into dir, appending workspace-relative entry paths and honoring the caps.
fn collect_entries(
    dir: &Path,
    root: &Path,
    depth: usize,
    entries: &mut Vec<String>,
    truncated: &mut bool,
) {
    if depth == 0 || *truncated {
        return;
    }
    for (path, is_dir, is_symlink) in read_sorted(dir) {
        if *truncated {
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if entries.len() >= MAX_LIST_ENTRIES {
            *truncated = true;
            return;
        }
        // A trailing slash tells a directory from a file, as search_files does.
        let suffix = if is_dir { "/" } else { "" };
        entries.push(format!("{}{suffix}", relative_to(root, &path)));
        // Symlinked directories are not followed so traversal cannot leave the root.
        if is_dir && !is_symlink {
            collect_entries(&path, root, depth.saturating_sub(1), entries, truncated);
        }
    }
}

/// The search_files tool.
pub struct SearchFiles;

#[async_trait::async_trait]
impl Tool for SearchFiles {
    fn name(&self) -> &'static str {
        "search_files"
    }

    fn description(&self) -> &'static str {
        "Search the workspace: file contents for text or a regex (path:line: text), or file and directory names by glob with target 'files'. Skips .git, target, node_modules, .venv, venv, __pycache__ and files over 1 MiB."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Text or regex to find, or a name glob such as '*.py' with target 'files'."
                },
                "target": {
                    "type": "string",
                    "enum": ["content", "files"],
                    "description": "'content' (default) searches file contents; 'files' matches file and directory names."
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search in. Default: the workspace root."
                },
                "file_glob": {
                    "type": "string",
                    "description": "Content search only: restrict to file names matching this glob (e.g. '*.py')."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results. Default 50."
                }
            },
            "required": ["pattern"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        off_runtime(ctx, args, search_files).await
    }
}

fn search_files(
    run: &RunContext,
    cancel: &CancellationToken,
    args: &Value,
) -> CoreResult<ToolOutcome> {
    let pattern = required_str(args, "pattern")?;
    if pattern.is_empty() {
        return Err(CoreError::InvalidRequest(
            "'pattern' must not be empty".into(),
        ));
    }
    let requested = optional_str(args, "path").unwrap_or_else(|| ".".to_string());
    // The NT/device-namespace guard runs on the raw string before any resolution.
    if let Some(reason) = safety::nt_namespace_error(Path::new(&requested)) {
        return Ok(read_denied(Path::new(&requested), reason));
    }
    let resolved = run.resolve_path(&requested)?;
    if let Some(reason) = safety::read_denied_reason(&resolved) {
        return Ok(read_denied(&resolved, reason));
    }
    let root = &run.require_workspace()?.canonical_root;
    let max_results = optional_usize(args, "limit")?
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_MAX_RESULTS);

    if !resolved.exists() {
        return Ok(ToolOutcome::error(format!(
            "{} does not exist",
            resolved.display()
        )));
    }

    // Literal text first, so "[package]" keeps its meaning, then as a regex for the "a|b" and
    // "import.*lite" small models write. Smart case: no capitals ignores case, like ripgrep.
    let smart_case = !pattern.chars().any(char::is_uppercase);
    let compile = |regex: &str| {
        RegexBuilder::new(regex)
            .case_insensitive(smart_case)
            .build()
            .ok()
    };
    let escaped = regex::escape(&pattern);
    let literal = compile(&escaped)
        .ok_or_else(|| CoreError::InvalidRequest("'pattern' is too long".into()))?;
    // Name globs joined with '|' ("AGENTS.md|CLAUDE.md") match any of them.
    let globs: Vec<&str> = pattern
        .split('|')
        .map(str::trim)
        .filter(|glob| !glob.is_empty())
        .collect();
    let file_glob = optional_str(args, "file_glob");
    let names = optional_str(args, "target").as_deref() == Some("files");
    let query = Query {
        text: &literal,
        globs: &globs,
        names,
        file_glob: file_glob.as_deref(),
        // One hit past the limit shows that the list was cut short.
        max_results: max_results + 1,
        cancel,
    };
    let mut hits: Vec<String> = Vec::new();
    let mut omitted = 0usize;
    search_path(&resolved, root, &query, &mut hits, &mut omitted);
    if hits.is_empty() && !names && escaped != pattern {
        if let Some(regex) = compile(&pattern) {
            // The regex pass meets the same denied files again.
            omitted = 0;
            let query = Query {
                text: &regex,
                ..query
            };
            search_path(&resolved, root, &query, &mut hits, &mut omitted);
        }
    }
    // Small models search contents for a file name ("calc.py"); answer with the file.
    let mut header = None;
    if hits.is_empty() && !names {
        let by_name = Query {
            names: true,
            file_glob: None,
            ..query
        };
        search_path(&resolved, root, &by_name, &mut hits, &mut omitted);
        if !hits.is_empty() {
            header = Some(format!("no text matches '{pattern}'; files named like it:"));
        }
    }
    // A silent cut reads as "that is everything", so say that more exist.
    let cut_short = hits.len() > max_results;
    hits.truncate(max_results);
    if let Some(header) = header {
        hits.insert(0, header);
    }
    let mut body = if hits.is_empty() {
        format!("no matches for '{pattern}'")
    } else {
        hits.join("\n")
    };
    if cut_short {
        body.push_str(&format!(
                "\n[first {max_results} matches shown; more exist, so use a more specific pattern, path or file_glob]"
            ));
    }
    if omitted > 0 {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("[{omitted} file(s) omitted by the read denylist]"));
    }
    Ok(ToolOutcome::ok(body))
}

/// What search_files looks for.
struct Query<'a> {
    /// Content search: the lines to report.
    text: &'a Regex,
    /// Name search: a name matches any of these globs.
    globs: &'a [&'a str],
    /// Match file and directory names against `globs` instead of searching contents.
    names: bool,
    /// Content search only: file names must match this glob.
    file_glob: Option<&'a str>,
    max_results: usize,
    /// A walk of a large tree stops once nobody waits for its result.
    cancel: &'a CancellationToken,
}

impl Query<'_> {
    /// A glob with '*' or '?' matches the whole name. A bare word matches any name that
    /// contains it, ignoring case: small models send "pricing" for pricing.ts, "Zen" for zen/.
    /// A pattern with a '/' ("src/main.rs") is matched against the workspace-relative path.
    fn matches_name(&self, name: &str, relative: &str) -> bool {
        self.globs.iter().any(|glob| {
            let subject = if glob.contains('/') { relative } else { name };
            if glob.contains(['*', '?']) {
                glob_matches(glob, subject)
            } else {
                subject.to_lowercase().contains(&glob.to_lowercase())
            }
        })
    }
}

/// Search a single path, recursing through directories until the result cap is hit.
/// Any read-denied path is skipped and counted in `omitted`.
fn search_path(
    path: &Path,
    root: &Path,
    query: &Query,
    hits: &mut Vec<String>,
    omitted: &mut usize,
) {
    if hits.len() >= query.max_results {
        return;
    }
    if path.is_file() {
        search_file(path, root, query, hits, omitted);
        return;
    }
    for (child, is_dir, is_symlink) in read_sorted(path) {
        if hits.len() >= query.max_results || query.cancel.is_cancelled() {
            return;
        }
        let name = child
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_symlink {
            continue;
        }
        if is_dir && !query.names && safety::read_denied_reason(&child).is_some() {
            *omitted += 1;
            continue;
        }
        if is_dir {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            if query.names {
                let relative = relative_to(root, &child);
                if query.matches_name(&name, &relative) {
                    hits.push(format!("{relative}/"));
                }
            }
            search_path(&child, root, query, hits, omitted);
        } else {
            search_file(&child, root, query, hits, omitted);
        }
    }
}

/// Scan one file for matching lines, skipping oversized, denied and non-UTF-8 files.
fn search_file(
    path: &Path,
    root: &Path,
    query: &Query,
    hits: &mut Vec<String>,
    omitted: &mut usize,
) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    // A name search reads no contents (list_files shows the same names), so the denylist
    // only applies, and is only reported, for files a content search would have read.
    if query.names {
        let relative = relative_to(root, path);
        if query.matches_name(&name, &relative) {
            hits.push(relative);
        }
        return;
    }
    if query
        .file_glob
        .is_some_and(|glob| !glob_matches(glob, &name))
    {
        return;
    }
    if safety::read_denied_reason(path).is_some() {
        *omitted += 1;
        return;
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return,
    };
    if metadata.len() > MAX_SEARCH_FILE_BYTES {
        return;
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => return,
    };
    let relative = relative_to(root, path);
    for (index, line) in content.lines().enumerate() {
        if hits.len() >= query.max_results {
            return;
        }
        if query.text.is_match(line) {
            hits.push(format!("{relative}:{}: {line}", index + 1));
        }
    }
}
