//! write_file and patch, confined to the workspace. write_file replaces atomically; patch replaces
//! one exact (fuzzily matched) string, or applies a unified diff all or nothing.

use crate::error::{CoreError, CoreResult};
use crate::safety;
use crate::services::{CheckpointKind, CheckpointTarget};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use crate::tools::lsp::edit_errors;
use crate::tools::patch::apply_unified_diff;
use crate::tools::replace;
use silver_protocol::RiskLevel;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

/// Maximum accepted size for a single 'write_file' call.
pub const MAX_WRITE_BYTES: usize = 1024 * 1024;

/// A write-denied tool result carrying the stable `write_denied` code.
fn write_denied(path: &Path, reason: &str) -> ToolOutcome {
    let message = format!("Write denied: {} {reason}", path.display());
    ToolOutcome::error(safety::denied_error_body(
        safety::WRITE_DENIED_CODE,
        path,
        &message,
    ))
}

struct WriteFileTool;

struct PatchTool;

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write a complete file inside the workspace, creating parent directories. Refuses content larger than 1 MiB."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the workspace root."
                },
                "content": {
                    "type": "string",
                    "description": "Complete new content for the file."
                }
            },
            "required": ["path", "content"]
        })
    }

    fn risk(&self, args: &serde_json::Value) -> RiskLevel {
        // '~/.ssh/config' carries no key bytes but can execute code (ProxyCommand / Match
        // exec), so it is approval-gated rather than hard-denied.
        if args
            .get("path")
            .and_then(|value| value.as_str())
            .is_some_and(|path| safety::write_approval_required(Path::new(path)))
        {
            RiskLevel::Destructive
        } else {
            RiskLevel::Write
        }
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome> {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                CoreError::InvalidRequest("write_file requires a 'path' string".into())
            })?;
        let content = args
            .get("content")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                CoreError::InvalidRequest("write_file requires a 'content' string".into())
            })?;

        // The NT/device-namespace guard runs on the raw string before any resolution.
        if let Some(reason) = safety::nt_namespace_error(Path::new(path)) {
            return Ok(write_denied(Path::new(path), reason));
        }
        let resolved = ctx.run.resolve_path(path)?;
        // Confinement runs first; the credential denylist is an additional gate on the
        // resolved path and never widens what the workspace boundary already allows.
        if let Some(reason) = safety::write_denied_reason(&resolved) {
            return Ok(write_denied(&resolved, reason));
        }

        if resolved.is_dir() {
            return Ok(ToolOutcome::error(format!(
                "{path} is a directory; give the path of a file inside it, such as {path}/name.ext"
            )));
        }
        if content.len() > MAX_WRITE_BYTES {
            return Ok(ToolOutcome::error(format!(
                "refusing to write {} bytes; the maximum is {} bytes",
                content.len(),
                MAX_WRITE_BYTES
            )));
        }

        // What the file held, so only errors on the lines this write changed are reported.
        let before = std::fs::read_to_string(&resolved).unwrap_or_default();
        checkpoint_before_write(ctx, &resolved, path).await;
        write_atomic(&resolved, content)?;
        let note = edit_errors(ctx, &resolved, &before, content)
            .await
            .unwrap_or_default();
        Ok(ToolOutcome::ok(format!(
            "wrote {} bytes to {}{note}",
            content.len(),
            path
        )))
    }
}

#[async_trait::async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &'static str {
        "patch"
    }

    fn description(&self) -> &'static str {
        "Targeted find-and-replace edits in files. Use this instead of sed/awk in bash. Finds a unique string and replaces it; fuzzy matching tolerates minor whitespace/indentation drift. Read the file first and copy old_string from it verbatim."
    }

    fn schema(&self) -> serde_json::Value {
        // Replace mode only: every field is paid for on every request, and small models get exact
        // replacement right far more often than a diff. `mode` + `patch` still work, unadvertised.
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to find and replace. Must be unique in the file unless replace_all=true. Include surrounding context lines to ensure uniqueness."
                },
                "new_string": {
                    "type": "string",
                    "description": "Changed replacement text; it must differ from old_string. Pass empty string '' to delete the matched text."
                },
                "replace_all": {
                    "type": "boolean",
                    "default": false,
                    "description": "Replace all occurrences instead of requiring a unique match (default: false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn risk(&self, args: &serde_json::Value) -> RiskLevel {
        let target = args
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| {
                args.get("patch")
                    .and_then(|value| value.as_str())
                    .and_then(target_from_patch)
            });
        if target
            .as_deref()
            .is_some_and(|path| safety::write_approval_required(Path::new(path)))
        {
            RiskLevel::Destructive
        } else {
            RiskLevel::Write
        }
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome> {
        let edit = match PatchEdit::from_args(&args) {
            Ok(edit) => edit,
            Err(message) => return Ok(ToolOutcome::error(message)),
        };
        let target = edit.target();

        // The NT/device-namespace guard runs on the raw string before any resolution.
        if let Some(reason) = safety::nt_namespace_error(Path::new(&target)) {
            return Ok(write_denied(Path::new(&target), reason));
        }
        let resolved = ctx.run.resolve_path(target)?;
        // Confinement runs first; the credential denylist is an additional gate.
        if let Some(reason) = safety::write_denied_reason(&resolved) {
            return Ok(write_denied(&resolved, reason));
        }

        let original = match std::fs::read_to_string(&resolved) {
            Ok(text) => text,
            Err(error) => {
                return Ok(ToolOutcome::error(format!(
                    "could not read {target}: {error}"
                )))
            }
        };

        // read_file shows lines without their CR and models send LF, so edit a CRLF file as
        // LF and put its line endings back afterwards.
        let crlf = original.contains("\r\n")
            && original.matches('\n').count() == original.matches("\r\n").count();
        let text = if crlf {
            Cow::Owned(original.replace("\r\n", "\n"))
        } else {
            Cow::Borrowed(original.as_str())
        };
        let (updated, summary) = match edit.apply(&text) {
            Ok(Applied::Changed { content, summary }) => (content, summary),
            Ok(Applied::AlreadyPresent) => {
                return Ok(ToolOutcome::ok(format!(
                    "no change: {target} already contains new_string and no longer contains old_string, so this edit was applied earlier"
                )))
            }
            Err(message) => return Ok(ToolOutcome::error(message)),
        };

        let updated = if crlf {
            updated.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            updated
        };

        checkpoint_before_write(ctx, &resolved, target).await;
        write_atomic(&resolved, &updated)?;
        let note = edit_errors(ctx, &resolved, &original, &updated)
            .await
            .unwrap_or_default();
        Ok(ToolOutcome::ok(format!(
            "{summary} ({} to {} bytes){note}",
            original.len(),
            updated.len()
        )))
    }
}

/// One `patch` call, decoded from its arguments.
enum PatchEdit {
    /// Replace mode: the advertised shape.
    Replace {
        path: String,
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    /// Patch mode: a unified diff, accepted from any model without being advertised.
    Diff { path: String, patch: String },
}

/// What applying an edit to the file's current text produced.
enum Applied {
    Changed {
        content: String,
        summary: String,
    },
    /// A re-sent replace whose result is already in the file: success-shaped, nothing written.
    AlreadyPresent,
}

impl PatchEdit {
    fn from_args(args: &serde_json::Value) -> Result<Self, String> {
        let str_arg = |key: &str| args.get(key).and_then(|value| value.as_str());
        let path = str_arg("path")
            .map(str::trim)
            .filter(|path| !path.is_empty());
        let patch = str_arg("patch").filter(|patch| !patch.trim().is_empty());
        let old_string = str_arg("old_string");
        let new_string = str_arg("new_string");

        // Explicit mode wins; otherwise a diff with no old_string is patch mode (the shape
        // this tool used to advertise) and everything else is replace mode.
        let patch_mode = match str_arg("mode").map(str::trim) {
            Some("patch") => true,
            Some("replace") => false,
            Some(other) => return Err(format!("Unknown mode: {other}")),
            None => patch.is_some() && old_string.is_none(),
        };

        if patch_mode {
            let Some(patch) = patch else {
                return Err("patch content required".to_string());
            };
            let path = target_from_patch(patch).or_else(|| path.map(str::to_string));
            let Some(path) = path else {
                return Err("patch could not determine the target file: the patch has no '---'/'+++' header and no 'path' argument was supplied".to_string());
            };
            return Ok(Self::Diff {
                path,
                patch: patch.to_string(),
            });
        }

        let Some(path) = path else {
            return Err("path required".to_string());
        };
        let (Some(old_string), Some(new_string)) = (old_string, new_string) else {
            return Err("old_string and new_string required".to_string());
        };
        Ok(Self::Replace {
            path: path.to_string(),
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
            // Small models sometimes send the flag as the string "true".
            replace_all: args.get("replace_all").is_some_and(|value| {
                value.as_bool().unwrap_or_else(|| {
                    value
                        .as_str()
                        .is_some_and(|flag| flag.trim().eq_ignore_ascii_case("true"))
                })
            }),
        })
    }

    fn target(&self) -> &str {
        match self {
            Self::Replace { path, .. } | Self::Diff { path, .. } => path,
        }
    }

    fn apply(&self, original: &str) -> Result<Applied, String> {
        match self {
            Self::Diff { path, patch } => {
                let content = apply_unified_diff(original, patch)
                    .map_err(|error| format!("patch failed for {path}: {error}"))?;
                Ok(Applied::Changed {
                    content,
                    summary: format!("applied patch to {path}"),
                })
            }
            Self::Replace {
                path,
                old_string,
                new_string,
                replace_all,
            } => {
                if replace::is_already_applied(original, old_string, new_string) {
                    return Ok(Applied::AlreadyPresent);
                }
                let replacement =
                    replace::fuzzy_find_and_replace(original, old_string, new_string, *replace_all)
                        .map_err(|error| {
                            let hint = replace::no_match_hint(&error, old_string, original);
                            format!("patch failed for {path}: {error}{hint}")
                        })?;
                let mut summary = if replacement.matches == 1 {
                    format!(
                        "replaced 1 occurrence in {path} at line {}",
                        replacement.line
                    )
                } else {
                    format!(
                        "replaced {} occurrences in {path}, first at line {}",
                        replacement.matches, replacement.line
                    )
                };
                if replacement.strategy != "exact" {
                    // The file differed from old_string in whitespace, indentation, escaping
                    // or Unicode; say so, so the model copies text verbatim next time.
                    summary.push_str(&format!(
                        "; old_string only matched after {} normalization — copy text exactly from read_file next time",
                        replacement.strategy.replace('_', " ")
                    ));
                }
                Ok(Applied::Changed {
                    content: replacement.content,
                    summary,
                })
            }
        }
    }
}

/// Register the mutating file tools.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(WriteFileTool));
    registry.register(Arc::new(PatchTool));
}

/// Extract the target path from a unified diff preamble, preferring the new-file header.
fn target_from_patch(patch: &str) -> Option<String> {
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;

    for line in patch.lines() {
        if line.starts_with("@@") {
            break;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            new_path = Some(clean_header_path(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            old_path = Some(clean_header_path(rest));
        }
    }

    new_path
        .filter(|path| !path.is_empty() && path != "/dev/null")
        .or_else(|| old_path.filter(|path| !path.is_empty() && path != "/dev/null"))
}

/// Strip the 'a/' or 'b/' prefix, surrounding quotes and any tab-separated timestamp.
fn clean_header_path(raw: &str) -> String {
    let without_timestamp = raw.split('\t').next().unwrap_or(raw).trim();
    let unquoted = without_timestamp.trim_matches('"');
    unquoted
        .strip_prefix("a/")
        .or_else(|| unquoted.strip_prefix("b/"))
        .unwrap_or(unquoted)
        .to_string()
}

/// Snapshot the bytes about to be overwritten. Best effort: a full disk must not block the edit.
async fn checkpoint_before_write(ctx: &ToolContext<'_>, absolute: &Path, display_path: &str) {
    let Some(sink) = ctx.run.services.checkpoints.as_ref() else {
        return;
    };
    let kind = if absolute.is_file() {
        CheckpointKind::Replace
    } else {
        CheckpointKind::Create
    };
    let target = CheckpointTarget {
        session_id: ctx.run.session.id,
        run_id: Some(ctx.run.run_id),
        path: display_path.to_string(),
        absolute_path: absolute.to_path_buf(),
        kind,
    };
    if let Err(error) = sink.before_write(target).await {
        tracing::debug!(
            path = %absolute.display(),
            %error,
            "pre-write checkpoint failed (non-fatal)"
        );
    }
}
/// Write 'content' through a sibling temp file and rename it into place.
fn write_atomic(path: &Path, content: &str) -> CoreResult<()> {
    let parent = path.parent().ok_or_else(|| {
        CoreError::Internal(format!("path has no parent directory: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| io_error(parent, &error))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = parent.join(format!(".{}.silver-{}", file_name, uuid::Uuid::now_v7()));

    if let Err(error) = std::fs::write(&temp, content) {
        drop(std::fs::remove_file(&temp));
        return Err(io_error(&temp, &error));
    }
    // The rename replaces the file, so carry its mode over or a script loses its exec bit.
    if let Ok(metadata) = std::fs::metadata(path) {
        drop(std::fs::set_permissions(&temp, metadata.permissions()));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        drop(std::fs::remove_file(&temp));
        return Err(io_error(path, &error));
    }
    Ok(())
}

fn io_error(path: &Path, error: &std::io::Error) -> CoreError {
    CoreError::Internal(format!("{}: {error}", path.display()))
}
