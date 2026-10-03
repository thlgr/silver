//! The process tool. It runs an executable directly inside the workspace, never through a
//! shell, with a filtered environment, a bounded timeout and bounded combined output.

use crate::error::{CoreError, CoreResult};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use silver_protocol::RiskLevel;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Default wall-clock timeout for one command, in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard upper bound accepted for the per-call timeout, in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Maximum number of combined output bytes returned to the model.
const MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Variables a tool-spawned child keeps; never the daemon's whole environment, where provider keys
/// live. Toolchain basics, locale, identity, and the desktop-session handles (`XDG_RUNTIME_DIR`,
/// `WAYLAND_DISPLAY`, `DISPLAY`, the session bus) GUI-backed CLIs need. None holds a secret.
pub const PRESERVED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// The parent variables a tool-spawned child may keep: the PRESERVED_ENV allow-list plus the
/// operator's `[tools] env_passthrough` additions. Names absent from the parent are skipped,
/// and a duplicate name is only carried once.
pub fn filtered_env(extra: &[String]) -> Vec<(String, std::ffi::OsString)> {
    let mut kept: Vec<(String, std::ffi::OsString)> = Vec::new();
    let names = PRESERVED_ENV
        .iter()
        .map(|name| name.to_string())
        .chain(extra.iter().map(|name| name.trim().to_string()));
    for name in names {
        if name.is_empty() || kept.iter().any(|(seen, _)| *seen == name) {
            continue;
        }
        if let Some(value) = std::env::var_os(&name) {
            kept.push((name, value));
        }
    }
    kept
}

/// Start the child in a new session: it leads its own process group for [kill_process_group], and
/// with no controlling terminal a prompt fails at once instead of freezing until the timeout.
#[cfg(unix)]
pub fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid(2) is async-signal-safe, so it may run between fork and exec. It cannot
    // fail here: a freshly forked child is never a process group leader.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

/// Non-Unix fallback: there is no controlling terminal to drop.
#[cfg(not(unix))]
pub fn detach(_command: &mut std::process::Command) {}

/// SIGKILL the process group led by `pid` (a [detach]ed child), reaching every descendant. Errors,
/// such as ESRCH for a group already gone, are ignored.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) {
    if pid == 0 || pid > i32::MAX as u32 {
        return;
    }
    // SAFETY: kill(2) has no memory-safety preconditions; the negative pid addresses the
    // group. The return value is deliberately discarded.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Non-Unix fallback: the caller still kills the direct child itself.
#[cfg(not(unix))]
pub fn kill_process_group(_pid: u32) {}

#[derive(Debug, Deserialize)]
struct RunCommandArgs {
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Run an executable directly (no shell) inside the workspace. Args are an array. Runs with a filtered environment and bounded timeout; stdout and stderr are captured together."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Executable followed by its arguments. No shell interpretation."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory relative to the workspace root. Defaults to '.'."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_SECS,
                    "description": "Wall-clock timeout in seconds. Defaults to 60 and is capped at 600."
                }
            },
            "required": ["argv"],
            "additionalProperties": false
        })
    }

    fn risk(&self, _args: &serde_json::Value) -> RiskLevel {
        RiskLevel::Process
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome> {
        let parsed: RunCommandArgs = serde_json::from_value(args).map_err(|e| {
            CoreError::InvalidRequest(format!("invalid run_command arguments: {e}"))
        })?;
        if parsed.argv.is_empty() {
            return Err(CoreError::InvalidRequest(
                "run_command requires a non-empty argv".into(),
            ));
        }

        let workspace = ctx.run.require_workspace()?;
        let cwd_arg = parsed.cwd.as_deref().unwrap_or(".");
        let resolved_cwd = workspace.resolve_path(Path::new(cwd_arg))?;
        let timeout_secs = parsed
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        // Plan mode runs the program inside the sandbox, so the plan file is writable and the
        // kernel refuses every other write.
        let argv = crate::sandbox::shell_argv(&ctx.run.plan, &parsed.argv).unwrap_or(parsed.argv);
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        command.current_dir(&resolved_cwd);
        // Never inherit the full daemon environment: copy only the allow-list.
        command.env_clear();
        for (key, value) in filtered_env(&ctx.run.services.env_passthrough) {
            command.env(key, value);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        detach(command.as_std_mut());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let content = json!({
                    "exit_code": serde_json::Value::Null,
                    "output": format!("failed to start command: {e}"),
                });
                return Ok(ToolOutcome::error(content.to_string()));
            }
        };

        // Captured before the wait: the pid doubles as the process-group id on Unix.
        let child_pid = child.id();

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Internal("command stdout pipe unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| CoreError::Internal("command stderr pipe unavailable".into()))?;

        // Drain both pipes concurrently so a child that fills a pipe buffer never deadlocks
        // the wait below.
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            drop(stdout.read_to_end(&mut buf).await);
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            drop(stderr.read_to_end(&mut buf).await);
            buf
        });

        let wait = tokio::select! {
            _ = ctx.cancel.cancelled() => None,
            result = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()) => {
                Some(result)
            }
        };

        let status = match wait {
            None => {
                kill_child_group(&mut child, child_pid).await;
                return Err(CoreError::Internal("run_command cancelled".into()));
            }
            Some(Err(_elapsed)) => {
                kill_child_group(&mut child, child_pid).await;
                return Err(CoreError::ToolTimeout(format!(
                    "run_command exceeded its {timeout_secs}s timeout"
                )));
            }
            Some(Ok(Err(e))) => {
                return Err(CoreError::Internal(format!(
                    "failed waiting for command: {e}"
                )));
            }
            Some(Ok(Ok(status))) => status,
        };

        let mut combined = stdout_task.await.unwrap_or_default();
        combined.extend(stderr_task.await.unwrap_or_default());

        let truncated = combined.len() > MAX_OUTPUT_BYTES;
        if truncated {
            combined.truncate(MAX_OUTPUT_BYTES);
        }
        let mut output = String::from_utf8_lossy(&combined).into_owned();
        if truncated {
            output.push_str("[output truncated]");
        }

        let exit_code = status.code();
        let content = json!({
            "exit_code": exit_code,
            "output": output,
        });
        let mut outcome = ToolOutcome::ok(content.to_string());
        outcome.is_error = exit_code != Some(0);
        Ok(outcome)
    }
}

/// Kill a timed-out or cancelled child's whole process group, then reap the child.
async fn kill_child_group(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_process_group(pid);
    }
    drop(child.kill().await);
}

/// Register the process tool in the given registry.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(RunCommandTool));
}
