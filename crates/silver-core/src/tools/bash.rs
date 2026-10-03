//! The bash tool: a fresh `bash -c` per call, so no directory or variable carries over; background
//! calls return a process id for `process_manage`.

use crate::context::{is_root, os_name};
use crate::error::{CoreError, CoreResult};
use crate::prompt::NON_ROOT_SUDO_TIP;
use crate::redact::redact;
use crate::services::resolve_cwd;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

/// Default wall-clock timeout for one command, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 180;
/// Hard upper bound accepted for the per-call timeout, in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    pty: bool,
    #[serde(default)]
    notify: Option<Value>,
}

/// Commands that need root: sudo and system package managers. Matched on the command because
/// their error messages are localized ("a menos que seja root").
static ROOT_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsudo\s|\b(pacman|yay|paru)\s+-S|\b(apt|apt-get|dnf|yum|zypper)\s+(install|remove|upgrade)|\bapk\s+add")
        .expect("valid regex")
});

/// Untranslated failures of installs that need root (apt, pip on a managed Python).
const ROOT_OUTPUT_MARKERS: [&str; 3] = [
    "are you root?",
    "must be root",
    "externally-managed-environment",
];

/// A small model forgets the prompt's sudo tip mid-task and keeps trying installs and
/// workarounds, so repeat it right where the attempt fails.
fn needs_root(command: &str, output: &str) -> bool {
    ROOT_COMMAND.is_match(command) || ROOT_OUTPUT_MARKERS.iter().any(|m| output.contains(m))
}

struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a command with bash -c. Each call starts fresh in the workspace root or 'workdir': cd and exported vars do not carry over, so chain steps with && in one call. Returns when the command finishes, even with a high timeout, and stops anything it left running; set 'background' true only for commands that must keep running, then manage them with 'process_manage'."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to run."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run in background, returning a session_id for process_manage to read, wait on, or kill. Only for servers/watchers/daemons that must outlive this call.",
                    "default": false
                },
                "timeout": {
                    "type": "integer",
                    "description": "Max seconds to wait (default 180, cap 600). Returns instantly when the command finishes.",
                    "minimum": 1
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory for this command, relative to the root or absolute inside it. Default: session cwd."
                },
                "pty": {
                    "type": "boolean",
                    "description": "background=true only: pseudo-terminal for interactive CLIs. Not supported by the local backend.",
                    "default": false
                },
                "notify": {
                    "description": "background=true only: true to notify on exit, or an array of output patterns. Not supported by the local backend.",
                    "anyOf": [
                        { "type": "boolean" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                }
            },
            "required": ["command"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Process
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let parsed: BashArgs = serde_json::from_value(args)
            .map_err(|e| CoreError::InvalidRequest(format!("invalid bash arguments: {e}")))?;
        if parsed.command.trim().is_empty() {
            return Ok(ToolOutcome::error("bash requires a non-empty command"));
        }
        if let Some(notify) = &parsed.notify {
            if !(notify.is_boolean() || notify.is_array() || notify.is_null()) {
                return Ok(ToolOutcome::error(
                    "notify must be true/false (notify on exit) or a list of strings (notify on output pattern match)",
                ));
            }
            if !parsed.background {
                return Ok(ToolOutcome::error(
                    "notify only applies to background commands. Either drop notify, or run as bash(command=..., background=true, notify=...)",
                ));
            }
        }
        if parsed.pty {
            if !parsed.background {
                return Ok(ToolOutcome::error(
                    "pty requires background=true. Retry as bash(command=..., background=true, pty=true)",
                ));
            }
            return Ok(ToolOutcome::error(
                "pty is not supported by the local backend",
            ));
        }

        let backend = match ctx.run.services.terminal.as_ref() {
            Some(backend) => backend,
            None => return Ok(ToolOutcome::error("bash is unavailable")),
        };
        let workspace = ctx.run.require_workspace()?;
        let cwd = resolve_cwd(Some(workspace), parsed.workdir.as_deref())?;
        let cwd = cwd.to_string_lossy().to_string();
        let timeout = parsed
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        // Plan mode runs the command inside the sandbox, so the plan file is writable and the
        // kernel refuses every other write.
        let command = crate::sandbox::shell_command(&ctx.run.plan, &parsed.command)
            .map_or(Cow::Borrowed(parsed.command.as_str()), Cow::Owned);
        let output = backend
            .run(
                &ctx.run.scope,
                &command,
                Some(cwd.as_str()),
                timeout,
                parsed.background,
            )
            .await?;

        // Model-visible output is scrubbed of credentials before it leaves the tool.
        let scrubbed = redact(&output.output);
        let content = if let Some(process_id) = output.process_id {
            json!({
                "output": if scrubbed.is_empty() { "Background process started".to_string() } else { scrubbed },
                "session_id": process_id,
                "exit_code": output.exit_code,
                "error": Value::Null,
            })
        } else {
            let mut content = json!({
                "output": scrubbed,
                "exit_code": output.exit_code,
                "error": Value::Null,
            });
            if output.truncated {
                content["truncated"] = json!(true);
            }
            if !is_root() && needs_root(&parsed.command, &output.output) {
                content["note"] = json!(format!("{NON_ROOT_SUDO_TIP} {}.", os_name()));
            }
            content
        };
        let mut outcome = ToolOutcome::ok(content.to_string());
        outcome.is_error = !parsed.background && output.exit_code.is_some_and(|code| code != 0);
        Ok(outcome)
    }
}

/// Register the bash tool in the given registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(BashTool));
}
