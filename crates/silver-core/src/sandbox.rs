//! Plan mode's write boundary, in the kernel instead of the classifier: `plan::shell_change`
//! cannot see an interpreter, so a plan-mode command runs inside bubblewrap with the filesystem
//! read-only except the plan directory, and None comes back where bubblewrap is missing.

use crate::plan::Plan;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

const BWRAP: &str = "bwrap";

/// The shell a sandboxed command line names, resolved through PATH like the terminal's own.
const SHELL: &str = "bash";

/// Whether a run's shell commands are confined to writing the plan directory, so the plan-mode
/// notice can say so instead of the model discovering it from a read-only filesystem error.
pub(crate) fn write_boundary(plan: &Plan) -> bool {
    writable_dir(plan).is_some() && available()
}

/// The command line a plan-mode `bash` call runs, or None when the run is not in plan mode or
/// this host cannot confine it.
pub(crate) fn shell_command(plan: &Plan, command: &str) -> Option<String> {
    let writable = writable_dir(plan)?;
    let flags = flags(writable)?;
    let mut line = String::from(BWRAP);
    for flag in flags {
        line.push(' ');
        line.push_str(&flag);
    }
    line.push_str(" -- ");
    line.push_str(SHELL);
    line.push_str(" -c ");
    line.push_str(&quote(command));
    Some(line)
}

/// The argv a plan-mode `run_command` calls, or None on the same conditions.
pub(crate) fn shell_argv(plan: &Plan, argv: &[String]) -> Option<Vec<String>> {
    let mut wrapped = vec![BWRAP.to_string()];
    wrapped.extend(flags(writable_dir(plan)?)?);
    wrapped.push("--".to_string());
    wrapped.extend(argv.iter().cloned());
    Some(wrapped)
}

/// The directory a plan-mode command may write. The plan *directory*, not the plan file: `sed -i`
/// and `mv` write a temporary file beside their target and rename it, which a read-only directory
/// refuses even when the file in it is writable. It holds one plan per session and nothing else.
fn writable_dir(plan: &Plan) -> Option<&Path> {
    if !plan.is_on() {
        return None;
    }
    let dir = plan.file.parent()?;
    (!dir.as_os_str().is_empty()).then_some(dir)
}

/// Everything read-only but `writable`, or None where bubblewrap cannot confine a command.
fn flags(writable: &Path) -> Option<Vec<String>> {
    if !available() {
        return None;
    }
    let path = writable.to_string_lossy().into_owned();
    Some(vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--bind".to_string(),
        String::clone(&path),
        path,
        // The command's own process tree still needs these two.
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        // The sandbox goes when the daemon does, not when the command it wraps returns.
        "--die-with-parent".to_string(),
    ])
}

/// Whether bubblewrap can confine a command here: installed, and user namespaces not disabled.
/// Probed once with a real run, because `bwrap --version` succeeds either way; the probe is the
/// smallest sandbox that still exercises the namespaces, so it needs no writable path.
fn available() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| {
        std::process::Command::new(BWRAP)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Single-quote a word for the shell that runs the sandboxed command line.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
