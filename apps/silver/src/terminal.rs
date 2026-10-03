//! Local terminal backend. Each foreground command is a fresh `bash -c` leading its own process
//! group, stdout and stderr sharing one unlinked temp file. Background commands run detached,
//! tracked per scope for `process_manage`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use silver_core::error::{CoreError, CoreResult};
use silver_core::services::{ProcessAction, ProcessInfo, TerminalBackend, TerminalOutput};
use silver_core::tools::command::{detach, filtered_env, kill_process_group};
use silver_protocol::Scope;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// The shell used for every command.
const SHELL: &str = "bash";
/// Maximum number of output bytes returned to the caller.
const MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Hard upper bound accepted for a per-call timeout, in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Default bound for a blocking 'wait' action, in seconds.
const WAIT_TIMEOUT_SECS: u64 = 60;
/// Marker appended when output is truncated at MAX_OUTPUT_BYTES.
const TRUNCATION_MARKER: &str = "\n[output truncated at 1 MiB]";

/// Per-scope background process table keyed by the generated process id.
type ProcessTable = HashMap<Scope, HashMap<String, Arc<Mutex<BackgroundProcess>>>>;

/// Terminal backend running one 'bash -c' per command plus a per-scope process table.
pub struct TerminalManager {
    processes: Mutex<ProcessTable>,
    next_process: AtomicU64,
    /// Parent variables allowed through on top of the PRESERVED_ENV allow-list.
    env_passthrough: Vec<String>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self::with_env_passthrough(Vec::new())
    }

    /// Build a manager that also copies `extra` parent variables into every child.
    pub fn with_env_passthrough(extra: Vec<String>) -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
            next_process: AtomicU64::new(1),
            env_passthrough: extra,
        }
    }

    /// Kill and forget every background process owned by one scope.
    pub async fn cleanup_scope(&self, scope: &Scope) {
        if let Some(processes) = self.processes.lock().await.remove(scope) {
            for process in processes.into_values() {
                let mut guard = process.lock().await;
                if !guard.finished {
                    if let Some(pid) = guard.child.id() {
                        kill_process_group(pid);
                    }
                    drop(guard.child.kill().await);
                    guard.finished = true;
                }
            }
        }
    }

    async fn run_foreground(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout_secs: u64,
    ) -> CoreResult<TerminalOutput> {
        let start_error =
            |e: std::io::Error| CoreError::Internal(format!("failed to start bash: {e}"));
        let path = std::env::temp_dir().join(format!("silver-bash-{}.out", uuid::Uuid::now_v7()));
        let mut file = std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(start_error)?;
        drop(std::fs::remove_file(&path));

        let mut std_command = std::process::Command::new(SHELL);
        std_command.arg("-c").arg(command);
        prepare_env(&mut std_command, &self.env_passthrough);
        if let Some(dir) = cwd {
            std_command.current_dir(dir);
        }
        std_command
            .stdin(Stdio::null())
            .stdout(file.try_clone().map_err(start_error)?)
            .stderr(file.try_clone().map_err(start_error)?);
        detach(&mut std_command);
        let mut runner: Command = std_command.into();
        runner.kill_on_drop(true);
        let mut child = runner.spawn().map_err(start_error)?;
        // Stops whatever is left of the command when the call ends: finished, timed out, or
        // dropped by a stop or the tool watchdog.
        let _group = KillGroupOnDrop(child.id());

        let status =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
                Ok(status) => status
                    .map_err(|e| CoreError::Internal(format!("failed waiting for bash: {e}")))?,
                Err(_) => {
                    return Err(CoreError::ToolTimeout(format!(
                        "bash command exceeded its {timeout_secs}s timeout"
                    )))
                }
            };

        let mut output = Vec::new();
        file.seek(SeekFrom::Start(0))
            .and_then(|_| {
                file.take(MAX_OUTPUT_BYTES as u64 + 1)
                    .read_to_end(&mut output)
            })
            .map_err(|e| CoreError::Internal(format!("failed reading bash output: {e}")))?;
        let truncated = output.len() > MAX_OUTPUT_BYTES;
        output.truncate(MAX_OUTPUT_BYTES);
        Ok(TerminalOutput {
            output: finalize(output, truncated),
            // A command killed by signal N reads as 128 + N, as a shell reports it.
            exit_code: status.code().or_else(|| status.signal().map(|n| 128 + n)),
            process_id: None,
            truncated,
        })
    }

    async fn spawn_background(
        &self,
        scope: &Scope,
        command: &str,
        cwd: Option<&Path>,
    ) -> CoreResult<TerminalOutput> {
        let serial = self.next_process.fetch_add(1, Ordering::Relaxed);
        let id = format!("proc_{serial:012x}");

        let mut std_command = std::process::Command::new(SHELL);
        std_command.arg("-c").arg(command);
        prepare_env(&mut std_command, &self.env_passthrough);
        if let Some(dir) = cwd {
            std_command.current_dir(dir);
        }
        std_command.stdin(Stdio::null());
        detach(&mut std_command);
        let mut runner: Command = std_command.into();
        runner.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = runner
            .spawn()
            .map_err(|e| CoreError::Internal(format!("failed to start background process: {e}")))?;

        let output = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, Arc::clone(&output));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, Arc::clone(&output));
        }

        let process = Arc::new(Mutex::new(BackgroundProcess {
            command: command.to_string(),
            started_at: Utc::now(),
            child,
            output,
            finished: false,
            exit_code: None,
        }));
        self.processes
            .lock()
            .await
            .entry(Scope::clone(scope))
            .or_default()
            .insert(String::clone(&id), process);

        Ok(TerminalOutput {
            output: "Background process started".into(),
            exit_code: Some(0),
            process_id: Some(id),
            truncated: false,
        })
    }

    async fn find_process(
        &self,
        scope: &Scope,
        process_id: &str,
    ) -> CoreResult<Arc<Mutex<BackgroundProcess>>> {
        let processes = self.processes.lock().await;
        let scope_map = processes.get(scope).ok_or_else(|| {
            CoreError::InvalidRequest(format!("no background process matches '{process_id}'"))
        })?;
        if let Some(found) = scope_map.get(process_id) {
            return Ok(Arc::clone(found));
        }
        let mut matches = scope_map
            .iter()
            .filter(|(id, _)| id.starts_with(process_id))
            .map(|(_, process)| process);
        match (matches.next(), matches.next()) {
            (Some(found), None) => Ok(Arc::clone(found)),
            (None, _) => Err(CoreError::InvalidRequest(format!(
                "no background process matches '{process_id}'"
            ))),
            _ => Err(CoreError::InvalidRequest(format!(
                "'{process_id}' is an ambiguous process prefix"
            ))),
        }
    }
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        if let Ok(mut processes) = self.processes.try_lock() {
            for (_, scope_map) in processes.drain() {
                for (_, process) in scope_map {
                    if let Ok(mut guard) = process.try_lock() {
                        if !guard.finished {
                            if let Some(pid) = guard.child.id() {
                                kill_process_group(pid);
                            }
                            drop(guard.child.start_kill());
                            guard.finished = true;
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl TerminalBackend for TerminalManager {
    async fn cleanup(&self, scope: &Scope) {
        self.cleanup_scope(scope).await;
    }

    async fn run(
        &self,
        scope: &Scope,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
        background: bool,
    ) -> CoreResult<TerminalOutput> {
        let timeout_secs = timeout_secs.clamp(1, MAX_TIMEOUT_SECS);
        let cwd = cwd.map(PathBuf::from);
        if background {
            return self.spawn_background(scope, command, cwd.as_deref()).await;
        }
        self.run_foreground(command, cwd.as_deref(), timeout_secs)
            .await
    }

    async fn processes(&self, scope: &Scope) -> CoreResult<Vec<ProcessInfo>> {
        let handles: Vec<(String, Arc<Mutex<BackgroundProcess>>)> = {
            let processes = self.processes.lock().await;
            match processes.get(scope) {
                Some(scope_map) => scope_map
                    .iter()
                    .map(|(id, handle)| (String::clone(id), Arc::clone(handle)))
                    .collect(),
                None => return Ok(Vec::new()),
            }
        };
        let mut out = Vec::with_capacity(handles.len());
        for (id, handle) in handles {
            let mut guard = handle.lock().await;
            guard.refresh();
            out.push(ProcessInfo {
                id,
                command: String::clone(&guard.command),
                running: !guard.finished,
                started_at: guard.started_at,
            });
        }
        Ok(out)
    }

    async fn process_action(
        &self,
        scope: &Scope,
        process_id: &str,
        action: ProcessAction,
    ) -> CoreResult<String> {
        let target = self.find_process(scope, process_id).await?;
        match action {
            ProcessAction::Read => {
                let mut guard = target.lock().await;
                guard.refresh();
                Ok(guard.to_json(process_id).to_string())
            }
            ProcessAction::Kill => {
                let mut guard = target.lock().await;
                if !guard.finished {
                    if let Some(pid) = guard.child.id() {
                        kill_process_group(pid);
                    }
                    drop(guard.child.kill().await);
                    guard.finished = true;
                }
                guard.refresh();
                Ok(guard.to_json(process_id).to_string())
            }
            ProcessAction::Wait => {
                let mut guard = target.lock().await;
                let deadline = Instant::now() + Duration::from_secs(WAIT_TIMEOUT_SECS);
                while !guard.finished && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    guard.refresh();
                }
                let mut value = guard.to_json(process_id);
                if !guard.finished {
                    value["timed_out"] = serde_json::json!(true);
                }
                Ok(value.to_string())
            }
        }
    }
}

/// Kills a process group when dropped while it still holds a pid.
struct KillGroupOnDrop(Option<u32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_process_group(pid);
        }
    }
}

/// A detached background command tracked per scope.
struct BackgroundProcess {
    command: String,
    started_at: DateTime<Utc>,
    child: Child,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
    finished: bool,
    exit_code: Option<i32>,
}

impl BackgroundProcess {
    fn refresh(&mut self) {
        if !self.finished {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.finished = true;
                self.exit_code = status.code();
            }
        }
    }

    fn render_output(&self) -> String {
        let buffer = self.output.lock().expect("output buffer poisoned");
        let truncated = buffer.len() >= MAX_OUTPUT_BYTES;
        let bytes = buffer[..buffer.len().min(MAX_OUTPUT_BYTES)].to_vec();
        drop(buffer);
        finalize(bytes, truncated)
    }

    fn to_json(&self, id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": id,
            "command": self.command,
            "status": if self.finished { "exited" } else { "running" },
            "exit_code": self.exit_code,
            "output": self.render_output(),
        })
    }
}

/// Copy only the allow-listed parent variables into a child command.
fn prepare_env(command: &mut std::process::Command, extra: &[String]) {
    command.env_clear();
    for (key, value) in filtered_env(extra) {
        command.env(key, value);
    }
}

/// Read a child stream into a shared capped buffer until EOF.
fn spawn_reader<R>(mut reader: R, buffer: Arc<std::sync::Mutex<Vec<u8>>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut guard = buffer.lock().expect("output buffer poisoned");
                    if guard.len() < MAX_OUTPUT_BYTES {
                        let remaining = MAX_OUTPUT_BYTES - guard.len();
                        let take = read.min(remaining);
                        guard.extend_from_slice(&chunk[..take]);
                    }
                }
            }
        }
    });
}

/// Append the truncation marker when output hit the cap.
fn finalize(mut bytes: Vec<u8>, truncated: bool) -> String {
    if truncated {
        bytes.extend_from_slice(TRUNCATION_MARKER.as_bytes());
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
