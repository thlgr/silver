//! The execute_code tool: a snippet written to a temp file in the workspace and run by a local
//! interpreter (python3, sh or node) through the terminal backend; no state persists.

use crate::error::{CoreError, CoreResult};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Directory under the workspace root that holds the per-call script files.
const EXEC_DIR: &str = ".silver/exec";
/// Default wall-clock timeout for one snippet, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Maximum number of output bytes returned to the model.
const MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Marker appended when the captured output is clipped.
const TRUNCATION_MARKER: &str = "\n[output truncated]";

#[derive(Debug, Deserialize)]
struct ExecuteCodeArgs {
    code: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    argv: Vec<String>,
}

/// The interpreter and script extension for one supported language.
#[derive(Clone, Copy, Debug)]
struct LanguageSpec {
    interpreter: &'static str,
    extension: &'static str,
}

impl LanguageSpec {
    /// Resolve a language name to its interpreter, or None when unsupported.
    fn resolve(language: &str) -> Option<Self> {
        match language {
            "python" => Some(Self {
                interpreter: "python3",
                extension: "py",
            }),
            "bash" | "sh" => Some(Self {
                interpreter: "sh",
                extension: "sh",
            }),
            "node" => Some(Self {
                interpreter: "node",
                extension: "js",
            }),
            _ => None,
        }
    }
}

struct ExecuteCodeTool;

#[async_trait]
impl Tool for ExecuteCodeTool {
    fn name(&self) -> &'static str {
        "execute_code"
    }

    fn description(&self) -> &'static str {
        "Run a source snippet with a local interpreter. The code is written to a temp file inside the workspace and executed through the terminal backend, then the temp file is removed. Returns stdout, stderr and the exit code. Supported languages: python, bash, sh, node."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Source code to execute. Print the result to stdout."
                },
                "language": {
                    "type": "string",
                    "enum": ["python", "bash", "sh", "node"],
                    "description": "Language and interpreter for the snippet. Defaults to 'python' (python3)."
                },
                "argv": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments passed to the interpreter before the script path."
                }
            },
            "required": ["code"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Process
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let parsed: ExecuteCodeArgs = serde_json::from_value(args).map_err(|e| {
            CoreError::InvalidRequest(format!("invalid execute_code arguments: {e}"))
        })?;
        if parsed.code.trim().is_empty() {
            return Ok(ToolOutcome::error(
                "execute_code requires a non-empty 'code' string",
            ));
        }

        let requested = parsed
            .language
            .as_deref()
            .unwrap_or("python")
            .trim()
            .to_lowercase();
        let spec = match LanguageSpec::resolve(&requested) {
            Some(spec) => spec,
            None => {
                return Ok(ToolOutcome::error(format!(
                    "execute_code does not support language '{requested}'; supported languages are python, bash, sh and node"
                )))
            }
        };

        let backend = match ctx.run.services.terminal.as_ref() {
            Some(backend) => backend,
            None => {
                return Ok(ToolOutcome::error(
                    "execute_code is unavailable: the terminal backend is not configured",
                ))
            }
        };
        let workspace = ctx.run.require_workspace()?;

        // The confined workspace root is also the interpreter's working directory.
        let script = workspace.resolve_path(&script_relative_path(spec.extension))?;
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CoreError::Internal(format!(
                    "execute_code could not create {}: {e}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&script, parsed.code.as_bytes()).map_err(|e| {
            CoreError::Internal(format!(
                "execute_code could not write {}: {e}",
                script.display()
            ))
        })?;

        let command = build_command(spec.interpreter, &parsed.argv, &script);
        let cwd = workspace.canonical_root.to_string_lossy().to_string();
        let run_result = backend
            .run(
                &ctx.run.scope,
                &command,
                Some(cwd.as_str()),
                DEFAULT_TIMEOUT_SECS,
                false,
            )
            .await;
        // Remove the per-call script before surfacing any backend error.
        drop(std::fs::remove_file(&script));
        let output = run_result?;

        // The terminal backend captures stdout and stderr into one stream, so the combined
        // capture is reported under 'stdout' and 'stderr' stays empty.
        let (stdout, stdout_truncated) = truncate_output(&output.output);
        let mut content = json!({
            "stdout": stdout,
            "stderr": "",
            "exit_code": output.exit_code,
        });
        if stdout_truncated || output.truncated {
            content["truncated"] = json!(true);
        }
        let mut outcome = ToolOutcome::ok(content.to_string());
        outcome.is_error = output.exit_code.is_some_and(|code| code != 0);
        Ok(outcome)
    }
}

/// Build the workspace-relative path for one call, for example '.silver/exec/<uuid>.py'.
fn script_relative_path(extension: &str) -> PathBuf {
    Path::new(EXEC_DIR).join(format!("{}.{extension}", uuid::Uuid::now_v7()))
}

/// Build the shell command that runs the interpreter on the script file.
fn build_command(interpreter: &str, argv: &[String], script: &Path) -> String {
    let mut command = String::from(interpreter);
    for arg in argv {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command.push(' ');
    command.push_str(&shell_quote(&script.to_string_lossy()));
    command
}

/// Quote one argument for a POSIX shell, leaving simple tokens untouched.
fn shell_quote(value: &str) -> String {
    let simple = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b',' | b'@'
                )
        });
    if simple {
        return value.to_string();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Clamp captured output to MAX_OUTPUT_BYTES on a char boundary.
fn truncate_output(output: &str) -> (String, bool) {
    if output.len() <= MAX_OUTPUT_BYTES {
        return (output.to_string(), false);
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut clipped = output[..end].to_string();
    clipped.push_str(TRUNCATION_MARKER);
    (clipped, true)
}

/// Register the execute_code tool in the given registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ExecuteCodeTool));
}
