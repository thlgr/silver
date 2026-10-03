//! Git worktree isolation and working-tree diffs. Every git call goes through git_output: no shell,
//! a confined cwd, a timeout, stdin closed and a non-interactive environment (no prompt, pager,
//! editor, hooks or user config); diff subcommands also get --no-ext-diff --no-textconv.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;

// ── Constants (Hermes worktree_ops.py / worktree_gc.py / working_diff.py) ────

/// Default timeout for a git invocation.
pub const GIT_TIMEOUT_SECS: u64 = 10;
/// git worktree add timeout: a ~10k-file checkout measured 113 s under load.
pub const WORKTREE_ADD_TIMEOUT_SECS: u64 = 120;
/// Timeout for the repo-root probe.
pub const REPO_ROOT_TIMEOUT_SECS: u64 = 5;
/// Cleanup/removal timeout.
pub const CLEANUP_TIMEOUT_SECS: u64 = 15;
/// Pack count at which a background repack is triggered.
pub const PACK_SPRAWL_THRESHOLD: usize = 15;
/// Hard bound for one background repack.
pub const REPACK_TIMEOUT_SECS: u64 = 1800;
/// One repack attempt per clone per interval, box-wide.
pub const REPACK_MIN_INTERVAL_SECS: u64 = 6 * 3600;
/// Lock file name serializing repacks per clone.
pub const REPACK_LOCK: &str = "silver-repack.lock";
/// Branch prefix for worktrees created by worktree_create.
pub const WORKTREE_BRANCH_PREFIX: &str = "silver/";
/// Prefix for the random worktree name used when no name is supplied.
pub const WORKTREE_RANDOM_PREFIX: &str = "silver-";
/// Default worktree directory under the repo root (config.worktree_root()).
pub const DEFAULT_WORKTREE_DIR: &str = ".worktrees";
/// Retained git cherry verdict entries (~90 bytes each).
pub const MERGE_CACHE_MAX: usize = 1000;
/// Trees preserved past this age with real work are reported once per sweep.
pub const STALE_WORK_SECS: u64 = 7 * 24 * 3600;
/// Default startup-pruner age tier for random (silver-*) trees.
pub const DEFAULT_PRUNE_MAX_AGE_HOURS: u64 = 24;
/// Bounded cherry probe: a branch this far ahead is a stale-base lane.
pub const MAX_CHERRY_AHEAD: u64 = 50;
/// git cherry cap used by the startup pruner.
pub const PRUNE_CHERRY_MAX_AHEAD: u64 = 20;
/// working_diff._GIT_TIMEOUT.
pub const DIFF_TIMEOUT_SECS: u64 = 15;
/// Sanity cap on untracked files rendered one by one.
pub const MAX_UNTRACKED_FILES: usize = 50;
/// /diff body cap (_print_diff_body(limit=400)).
pub const DIFF_BODY_LINE_LIMIT: usize = 400;
/// Escalation notice when .worktrees/ holds this many trees.
pub const WORKTREE_NOTICE_COUNT: usize = 10;
/// Escalation notice when .worktrees/ holds this many MiB.
pub const WORKTREE_NOTICE_MIB: u64 = 5120;
/// Branches never considered for deletion, in any mode.
pub const PROTECTED_BRANCHES: [&str; 5] = ["main", "master", "develop", "dev", "trunk"];
/// Tree verdicts that are safe to reclaim.
pub const REAP_VERDICTS: [&str; 3] = ["reap", "reap-archive", "reap-keep-branch"];
/// Ref recording the session-start commit for diff(.., Session, ..).
pub const SESSION_BASELINE_REF: &str = "refs/silver/session-baseline";

const DEV_NULL: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Diff-rendering subcommands that accept --no-ext-diff --no-textconv.
const DIFF_RENDERING_SUBCOMMANDS: [&str; 4] = ["diff", "show", "log", "blame"];
/// Attribute-scoped drivers a hostile .gitattributes could otherwise run.
const NO_DRIVER_DIFF_FLAGS: [&str; 2] = ["--no-ext-diff", "--no-textconv"];
/// Global git options that consume the following token.
const GIT_VALUE_OPTS: [&str; 6] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
];

// ── Bounded process execution ────────────────────────────────────────────────

/// Result of one bounded subprocess run.
#[derive(Debug)]
struct CommandOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    spawn_failed: bool,
}

impl CommandOutput {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    fn success(&self) -> bool {
        self.status == Some(0) && !self.spawn_failed
    }

    fn failed_spawn(message: String) -> Self {
        Self {
            status: None,
            stdout: Vec::new(),
            stderr: message.into_bytes(),
            timed_out: false,
            spawn_failed: true,
        }
    }
}

/// Spawn cmd, capture stdout/stderr on reader threads and wait at most
/// timeout_secs. The child is killed on expiry (std-only: the direct child
/// only, best effort). Never panics; a spawn failure is reported in the result.
fn run_command(mut cmd: Command, timeout_secs: u64) -> CommandOutput {
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return CommandOutput::failed_spawn(err.to_string()),
    };
    let stdout_reader = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            drop(pipe.read_to_end(&mut buf));
            buf
        })
    });
    let stderr_reader = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            drop(pipe.read_to_end(&mut buf));
            buf
        })
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    drop(child.kill());
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                drop(child.kill());
                drop(child.wait());
                break None;
            }
        }
    };

    let stdout = stdout_reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    CommandOutput {
        status: status.and_then(|status| status.code()),
        stdout,
        stderr,
        timed_out,
        spawn_failed: false,
    }
}

/// The user's configured global/system safe.directory values, in git's own
/// effective order. Read once per process under the untouched environment.
fn safe_directories() -> &'static [(String, String)] {
    static CACHE: OnceLock<Vec<(String, String)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut values = Vec::new();
        for scope in ["--system", "--global"] {
            let mut cmd = Command::new("git");
            cmd.args(["config", scope, "-z", "--get-all", "safe.directory"]);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            cmd.env("GIT_TERMINAL_PROMPT", "0");
            for (key, _) in std::env::vars() {
                if key == "GIT_CONFIG_PARAMETERS"
                    || key == "GIT_CONFIG_COUNT"
                    || key.starts_with("GIT_CONFIG_KEY_")
                    || key.starts_with("GIT_CONFIG_VALUE_")
                {
                    cmd.env_remove(&key);
                }
            }
            let output = run_command(cmd, 5);
            if output.status == Some(0) {
                let text = output.stdout_text();
                let mut records: Vec<&str> = text.split('\0').collect();
                if records.last() == Some(&"") {
                    records.pop();
                }
                values.extend(
                    records
                        .into_iter()
                        .map(|value| ("safe.directory".to_string(), value.to_string())),
                );
            }
        }
        values
    })
}

/// Config overrides disabling credential helpers, fsmonitor, hooks, pagers,
/// editors, external diff and interactive ssh prompts.
const GIT_CONFIG_OVERRIDES: [(&str, &str); 10] = [
    ("credential.helper", ""),
    ("core.askPass", ""),
    ("core.fsmonitor", "false"),
    ("core.untrackedCache", "false"),
    ("core.hooksPath", DEV_NULL),
    ("core.pager", "cat"),
    ("core.editor", "true"),
    ("sequence.editor", "true"),
    ("diff.external", ""),
    ("core.sshCommand", "ssh -o BatchMode=yes"),
];

/// Apply the non-interactive git environment to cmd (Hermes
/// noninteractive_git_env): fail instead of prompting, no GCM dialog, and an
/// isolated config so a repo/global setting cannot hang or mutate plumbing.
fn apply_noninteractive_git_env(cmd: &mut Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GCM_INTERACTIVE", "Never");
    for (key, _) in std::env::vars() {
        if key == "GIT_CONFIG_PARAMETERS"
            || key == "GIT_CONFIG_COUNT"
            || key.starts_with("GIT_CONFIG_KEY_")
            || key.starts_with("GIT_CONFIG_VALUE_")
        {
            cmd.env_remove(&key);
        }
    }
    cmd.env("GIT_CONFIG_GLOBAL", DEV_NULL);
    cmd.env("GIT_CONFIG_SYSTEM", DEV_NULL);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("PAGER", "cat");
    cmd.env("GIT_EDITOR", "true");

    let mut overrides: Vec<(&str, &str)> = GIT_CONFIG_OVERRIDES.to_vec();
    overrides.extend(
        safe_directories()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
    for (index, (key, value)) in overrides.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{index}"), key);
        cmd.env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
}

/// Run git *args in cwd with a bounded timeout.
fn git_output(cwd: &Path, args: &[&str], timeout_secs: u64) -> CommandOutput {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    cmd.args(args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_noninteractive_git_env(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    run_command(cmd, timeout_secs)
}

/// git_output for owned argument vectors.
/// Stripped stdout, or None on a non-zero exit (Hermes _git_out).
fn git_out(cwd: &Path, args: &[&str], timeout_secs: u64) -> Option<String> {
    let output = git_output(cwd, args, timeout_secs);
    if output.success() {
        Some(output.stdout_text().trim().to_string())
    } else {
        None
    }
}

/// Fail-soft git (Hermes _git_quiet).
fn git_quiet(cwd: &Path, args: &[&str], timeout_secs: u64) {
    drop(git_output(cwd, args, timeout_secs));
}

/// Insert --no-ext-diff --no-textconv after a diff-rendering subcommand
/// (Hermes harden_git_argv). Input excludes the leading "git".
fn harden_git_argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut index = 0;
    while index < args.len() {
        let token = args[index];
        if GIT_VALUE_OPTS.contains(&token) {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        if DIFF_RENDERING_SUBCOMMANDS.contains(&token) {
            let mut out = args[..=index].to_vec();
            out.extend(NO_DRIVER_DIFF_FLAGS);
            out.extend_from_slice(&args[index + 1..]);
            return out;
        }
        return args.to_vec();
    }
    args.to_vec()
}

/// A diff-rendering git call: core.quotePath=false + hardened argv.
fn git_diff(cwd: &Path, args: &[&str], timeout_secs: u64) -> CommandOutput {
    let mut full = vec!["-c", "core.quotePath=false"];
    full.extend(harden_git_argv(args));
    git_output(cwd, &full, timeout_secs)
}

/// Whether git is on PATH (Hermes shutil.which("git")).
fn git_available() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths)
        .any(|dir| dir.join("git").is_file() || dir.join("git.exe").is_file())
}

// ── Small path/time helpers ──────────────────────────────────────────────────

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn modified_secs(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0),
    )
}

/// Resolve a path like Path.resolve(strict=False) where possible, falling back
/// to the lexical path when it does not exist.
fn resolve_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True when path (already resolved) stays within root.
fn path_is_within_root(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

/// A time+pid+counter hex id, used for random worktree names.
fn random_hex8() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed =
        nanos ^ ((std::process::id() as u64) << 24) ^ serial.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{mixed:016x}")[..8].to_string()
}

/// Translate a Git Bash path (/c/.., /cygdrive/c/.., /mnt/c/..) to C:\.. on
/// Windows; identity elsewhere.
fn normalize_git_bash_path(path: &str) -> String {
    if !cfg!(windows) {
        return path.to_string();
    }
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let rest = trimmed
        .strip_prefix("cygdrive/")
        .or_else(|| trimmed.strip_prefix("mnt/"))
        .unwrap_or(trimmed);
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'/' {
        format!(
            "{}:\\{}",
            (bytes[0] as char).to_ascii_uppercase(),
            rest[2..].replace('/', "\\")
        )
    } else {
        path.to_string()
    }
}

/// Whether a directory name is a kanban task tree (^t_[0-9a-f]+$).
fn is_kanban_tree(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("t_") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
}

/// The status of a worktree lock, mirroring _worktree_lock_is_live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    /// The lock names a running silver (or we could not tell).
    Live,
    /// The lock names a dead pid, or is a foreign leftover.
    Dead,
    /// No lock line for this tree.
    Unlocked,
}

/// Best-effort process liveness for the lock's recorded pid. Fails safe toward
/// "live" on platforms without /proc.
#[cfg(target_os = "linux")]
fn pid_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_exists(_pid: u32) -> bool {
    true
}

/// Extract the pid from a "silver pid=<pid>" lock reason.
fn parse_silver_pid(reason: &str) -> Option<u32> {
    let marker = "silver pid=";
    let start = reason.find(marker)? + marker.len();
    let digits: String = reason[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

// ── Basic git facts ──────────────────────────────────────────────────────────

/// Return the git repo root for cwd, or None when not inside a repo.
pub fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let root = git_out(
        cwd,
        &["rev-parse", "--show-toplevel"],
        REPO_ROOT_TIMEOUT_SECS,
    )?;
    if root.is_empty() {
        return None;
    }
    Some(PathBuf::from(normalize_git_bash_path(&root)))
}

/// Resolve the repo root for a workspace, reporting the offending path when the directory is not
/// a git checkout. "not inside a git repository" on its own leaves the user guessing which
/// directory was inspected, and every git-backed endpoint (worktrees, diffs, rollback) needs it.
fn require_repo_root(workspace_root: &Path) -> Result<PathBuf> {
    git_repo_root(workspace_root).ok_or_else(|| {
        anyhow::anyhow!(
            "workspace is not inside a git repository: {}",
            workspace_root.display()
        )
    })
}

/// The resolved worktrees directory: <repo_root>/<config.worktree_root()>.
pub fn worktrees_dir(config: &Config, repo_root: &Path) -> PathBuf {
    repo_root.join(config.worktree_root())
}

/// Whether repo_path is a shallow clone. Fails toward false on unknown.
fn repo_is_shallow(repo_path: &Path, timeout_secs: u64) -> bool {
    git_out(
        repo_path,
        &["rev-parse", "--is-shallow-repository"],
        timeout_secs,
    )
    .as_deref()
        == Some("true")
}

/// Blobless unshallow so history verdicts are correct. Returns whether the repo
/// is non-shallow afterwards.
fn deepen_shallow_repo(repo_root: &Path, timeout_secs: u64) -> bool {
    if !repo_is_shallow(repo_root, 5) {
        return true;
    }
    let Some(remotes) = git_out(repo_root, &["remote"], GIT_TIMEOUT_SECS) else {
        return false;
    };
    let names: Vec<String> = remotes
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    let Some(remote) = names
        .iter()
        .find(|name| name.as_str() == "origin")
        .cloned()
        .or_else(|| names.first().cloned())
    else {
        return false;
    };
    for extra in [vec!["--filter=blob:none"], vec![]] {
        let mut args = vec!["fetch", remote.as_str(), "--unshallow"];
        for flag in &extra {
            args.push(flag);
        }
        let result = git_output(repo_root, &args, timeout_secs);
        if result.timed_out {
            return false;
        }
        if result.success() {
            break;
        }
    }
    !repo_is_shallow(repo_root, 5)
}

/// Verify a ref resolves to a commit.
fn ref_exists(path: &Path, reference: &str, timeout_secs: u64) -> bool {
    git_out(
        path,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
        timeout_secs,
    )
    .is_some()
}

/// Local trunk of a repo with NO remote-tracking refs: main/master, else the
/// branch checked out in the main worktree. None = no baseline.
fn local_trunk(path: &Path, timeout_secs: u64) -> Option<String> {
    for name in ["main", "master"] {
        if ref_exists(path, &format!("refs/heads/{name}"), timeout_secs) {
            return Some(name.to_string());
        }
    }
    let porcelain = git_out(path, &["worktree", "list", "--porcelain"], timeout_secs)?;
    if let Some(first_block) = porcelain.split("\n\n").next() {
        for line in first_block.lines() {
            if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                let branch = branch.trim();
                if !branch.is_empty() {
                    return Some(branch.to_string());
                }
            }
        }
    }
    None
}

/// Ref merged work is judged against. None = nothing to compare against.
fn worktree_merge_base_ref(path: &Path, timeout_secs: u64) -> Option<String> {
    for candidate in ["origin/HEAD", "origin/main", "origin/master"] {
        if ref_exists(path, candidate, timeout_secs) {
            return Some(candidate.to_string());
        }
    }
    match git_out(
        path,
        &["for-each-ref", "--format=%(refname)", "refs/remotes"],
        timeout_secs,
    ) {
        Some(remotes) if remotes.is_empty() => local_trunk(path, timeout_secs),
        _ => None,
    }
}

/// Whether a worktree has commits unreachable from any remote branch. Fails SAFE
/// toward true.
fn worktree_has_unpushed_commits(path: &Path, timeout_secs: u64) -> bool {
    let Some(remote_refs) = git_out(
        path,
        &["for-each-ref", "--format=%(refname)", "refs/remotes"],
        timeout_secs,
    ) else {
        return true;
    };
    let baseline: Vec<String> = if remote_refs.is_empty() {
        match local_trunk(path, timeout_secs) {
            Some(trunk) => vec![trunk],
            None => return true,
        }
    } else {
        vec!["--remotes".to_string()]
    };
    let mut args: Vec<&str> = vec!["log", "--oneline", "HEAD", "--not"];
    for base in &baseline {
        args.push(base);
    }
    match git_out(path, &args, timeout_secs) {
        Some(unpushed) => !unpushed.is_empty(),
        None => true,
    }
}

/// Whether a worktree has staged/unstaged/untracked changes. Fails SAFE toward
/// true.
fn worktree_is_dirty(path: &Path, timeout_secs: u64) -> bool {
    match git_out(path, &["status", "--porcelain"], timeout_secs) {
        Some(status) => !status.is_empty(),
        None => true,
    }
}

/// Checked-out branch name, or None when detached/git fails.
fn worktree_current_branch(path: &Path, timeout_secs: u64) -> Option<String> {
    let branch = git_out(path, &["rev-parse", "--abbrev-ref", "HEAD"], timeout_secs)?;
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// (has_tracked_modifications, untracked_paths) — tracked = real work,
/// untracked = archivable (Hermes worktree_gc _dirty_split).
fn dirty_split(path: &Path) -> (bool, Vec<String>) {
    let Some(status) = git_out(path, &["status", "--porcelain"], 10) else {
        return (true, Vec::new());
    };
    let lines: Vec<&str> = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let untracked: Vec<String> = lines
        .iter()
        .filter(|line| line.starts_with("??"))
        .map(|line| line[3..].trim().to_string())
        .collect();
    (untracked.len() != lines.len(), untracked)
}

/// {branch: sha} for every branch on origin (one ls-remote), or None =
/// cannot verify, preserve.
fn fetch_remote_branch_heads(
    repo_root: &Path,
    timeout_secs: u64,
) -> Option<BTreeMap<String, String>> {
    let result = git_output(repo_root, &["ls-remote", "--heads", "origin"], timeout_secs);
    if !result.success() {
        return None;
    }
    let mut heads = BTreeMap::new();
    for line in result.stdout_text().lines() {
        if let Some((sha, reference)) = line.split_once('\t') {
            if let Some(branch) = reference.strip_prefix("refs/heads/") {
                heads.insert(branch.trim().to_string(), sha.trim().to_string());
            }
        }
    }
    Some(heads)
}

/// Whether the branch head is EXACTLY what origin holds. Fails SAFE toward false.
fn worktree_branch_pushed_exact(
    path: &Path,
    remote_heads: Option<&BTreeMap<String, String>>,
    timeout_secs: u64,
) -> bool {
    let Some(remote_heads) = remote_heads else {
        return false;
    };
    let Some(branch) = worktree_current_branch(path, timeout_secs) else {
        return false;
    };
    let Some(remote_sha) = remote_heads.get(&branch) else {
        return false;
    };
    let Some(head) = git_out(path, &["rev-parse", "HEAD"], timeout_secs) else {
        return false;
    };
    !remote_sha.is_empty() && remote_sha == &head
}

/// Whether every local-only commit is patch-equivalent (git cherry) to
/// upstream. Fails SAFE toward false.
fn worktree_commits_all_merged_upstream(
    path: &Path,
    timeout_secs: u64,
    max_ahead: u64,
    cache: &mut BTreeMap<String, bool>,
) -> bool {
    let Some(base) = worktree_merge_base_ref(path, timeout_secs) else {
        return false;
    };

    let mut cache_key: Option<String> = None;
    if let Some(revs) = git_out(
        path,
        &["rev-parse", &format!("{base}^{{commit}}"), "HEAD^{commit}"],
        timeout_secs,
    ) {
        let shas: Vec<&str> = revs.split_whitespace().collect();
        if shas.len() == 2 {
            let key = format!("{}..{}:{}", shas[0], shas[1], max_ahead);
            if let Some(verdict) = cache.get(&key) {
                return *verdict;
            }
            cache_key = Some(key);
        }
    }

    let Some(ahead) = git_out(
        path,
        &["rev-list", "--count", &format!("{base}..HEAD")],
        timeout_secs,
    ) else {
        return false;
    };
    let count: i64 = ahead.trim().parse().unwrap_or(0);
    if count == 0 {
        return memo_verdict(cache_key, cache, true);
    }
    if count > max_ahead as i64 {
        return memo_verdict(cache_key, cache, false);
    }

    let cherry = git_output(path, &["cherry", &base, "HEAD"], timeout_secs);
    if !cherry.success() {
        return false;
    }
    let cherry_text = cherry.stdout_text();
    let lines: Vec<&str> = cherry_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    // "-" = patch-equivalent upstream; "+" = unique local work
    let merged = !lines.is_empty() && lines.iter().all(|line| line.starts_with('-'));
    memo_verdict(cache_key, cache, merged)
}

fn memo_verdict(
    cache_key: Option<String>,
    cache: &mut BTreeMap<String, bool>,
    verdict: bool,
) -> bool {
    if let Some(key) = cache_key {
        cache.insert(key, verdict);
    }
    verdict
}

/// Whether the branch's PR is MERGED on GitHub (gh pr list). Fails SAFE toward
/// false. Only true is cached.
fn worktree_branch_pr_merged(
    path: &Path,
    timeout_secs: u64,
    cache: &mut BTreeMap<String, bool>,
) -> bool {
    let Some(branch) = worktree_current_branch(path, timeout_secs) else {
        return false;
    };
    let mut cache_key: Option<String> = None;
    if let Some(sha) = git_out(path, &["rev-parse", "HEAD"], timeout_secs) {
        let key = format!("pr-merged:{branch}:{sha}");
        if cache.get(&key) == Some(&true) {
            return true;
        }
        cache_key = Some(key);
    }
    let mut cmd = Command::new("gh");
    cmd.args([
        "pr", "list", "--head", &branch, "--state", "merged", "--json", "number", "--limit", "1",
    ]);
    cmd.current_dir(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let gh = run_command(cmd, timeout_secs);
    if !gh.success() {
        return false;
    }
    let parsed: Vec<serde_json::Value> = serde_json::from_slice(&gh.stdout).unwrap_or_default();
    let merged = !parsed.is_empty();
    if merged {
        if let Some(key) = cache_key {
            cache.insert(key, true);
        }
    }
    merged
}

/// Lock state for one worktree registration. Fails SAFE toward "live".
fn worktree_lock_is_live(repo_root: &Path, worktree_path: &Path, timeout_secs: u64) -> LockState {
    let listing = git_out(
        repo_root,
        &["worktree", "list", "--porcelain"],
        timeout_secs,
    );
    let Some(listing) = listing else {
        return LockState::Live;
    };
    let target = resolve_path(worktree_path);
    let mut current: Option<PathBuf> = None;
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current = Some(resolve_path(Path::new(rest)));
        } else if line == "locked" || line.starts_with("locked ") {
            if current.as_deref() != Some(target.as_path()) {
                continue;
            }
            let reason = line.strip_prefix("locked").unwrap_or("").trim();
            let Some(pid) = parse_silver_pid(reason) else {
                // A foreign lock here is a leftover; the age/dirty/unpushed gates
                // already passed.
                return LockState::Dead;
            };
            if pid == std::process::id() {
                return LockState::Live;
            }
            return if pid_exists(pid) {
                LockState::Live
            } else {
                LockState::Dead
            };
        }
    }
    LockState::Unlocked
}

// ── Worktree listing ─────────────────────────────────────────────────────────

/// One git worktree list --porcelain entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
}

/// Parse git worktree list --porcelain output.
pub fn parse_worktree_list(text: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(rest),
                ..WorktreeEntry::default()
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("HEAD ") {
                entry.head = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
                entry.branch = Some(rest.to_string());
            } else if line == "detached" {
                entry.detached = true;
            } else if line == "bare" {
                entry.bare = true;
            } else if line == "locked" {
                entry.locked = true;
            } else if let Some(rest) = line.strip_prefix("locked ") {
                entry.locked = true;
                entry.lock_reason = Some(rest.to_string());
            }
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    entries
}

/// List every worktree registered on the repo (raw porcelain, not filtered to
/// .worktrees/).
pub fn worktree_list(workspace_root: &Path) -> Result<Vec<WorktreeEntry>> {
    let repo_root = require_repo_root(workspace_root)?;
    let output = git_output(
        &repo_root,
        &["worktree", "list", "--porcelain"],
        GIT_TIMEOUT_SECS,
    );
    if !output.success() {
        anyhow::bail!("git worktree list failed: {}", output.stderr_text().trim());
    }
    Ok(parse_worktree_list(&output.stdout_text()))
}

// ── Name handling / .gitignore / .worktreeinclude ────────────────────────────

/// Sanitize a user-supplied worktree name exactly like the reference:
/// non-[A-Za-z0-9._-] runs become "-", then strip leading/trailing "-._",
/// then cap at 40 characters.
pub fn sanitize_worktree_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    replaced
        .trim_matches(|c| matches!(c, '-' | '.' | '_'))
        .chars()
        .take(40)
        .collect()
}

/// A random silver-<8 hex> worktree name.
pub fn random_worktree_name() -> String {
    format!("{WORKTREE_RANDOM_PREFIX}{}", random_hex8())
}

/// Append <root>/ to the repo's .gitignore when missing (fail-soft).
fn ensure_worktrees_gitignored(repo_root: &Path, config: &Config) {
    let root = config.worktree_root();
    let entry = if root.is_absolute() {
        format!("{DEFAULT_WORKTREE_DIR}/")
    } else {
        let trimmed = root
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        format!("{trimmed}/")
    };
    let gitignore = repo_root.join(".gitignore");
    // utf-8-sig: a Notepad BOM would glue to the first line and defeat the check.
    let existing = fs::read_to_string(&gitignore)
        .map(|text| text.trim_start_matches('\u{feff}').to_string())
        .unwrap_or_default();
    if existing.lines().any(|line| line == entry) {
        return;
    }
    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore)
    else {
        return;
    };
    let separator = if !existing.is_empty() && !existing.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    drop(file.write_all(format!("{separator}{entry}\n").as_bytes()));
}

/// Copy/symlink entries listed in .worktreeinclude (gitignored files the agent
/// needs), with a traversal/symlink-escape guard on both endpoints.
fn copy_worktree_includes(repo_root: &Path, wt_path: &Path) {
    let include_file = repo_root.join(".worktreeinclude");
    let Ok(contents) = fs::read_to_string(&include_file) else {
        return;
    };
    let contents = contents.trim_start_matches('\u{feff}');
    let repo_root_resolved = resolve_path(repo_root);
    let wt_path_resolved = resolve_path(wt_path);
    for raw in contents.lines() {
        let entry = raw.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let src = repo_root.join(entry);
        let dst = wt_path.join(entry);
        let src_resolved = resolve_path(&src);
        let dst_resolved = resolve_path(&dst);
        if !path_is_within_root(&src_resolved, &repo_root_resolved)
            || !path_is_within_root(&dst_resolved, &wt_path_resolved)
        {
            continue;
        }
        if src.is_file() {
            if let Some(parent) = dst.parent() {
                drop(fs::create_dir_all(parent));
            }
            drop(fs::copy(&src, &dst));
        } else if src.is_dir() && !dst.exists() {
            if let Some(parent) = dst.parent() {
                drop(fs::create_dir_all(parent));
            }
            #[cfg(unix)]
            {
                drop(std::os::unix::fs::symlink(&src_resolved, &dst));
            }
            #[cfg(not(unix))]
            {
                drop(copy_dir_recursive(&src_resolved, &dst));
            }
        }
    }
}

#[cfg(not(unix))]
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

// ── Worktree creation ────────────────────────────────────────────────────────

/// A worktree created by worktree_create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub repo_root: PathBuf,
    pub base: String,
    pub base_label: String,
}

/// Age of .git/FETCH_HEAD, in seconds.
fn fetch_head_age(repo_root: &Path) -> Option<f64> {
    let git_dir = git_out(repo_root, &["rev-parse", "--git-dir"], 20)?;
    let fetch_head = repo_root.join(git_dir).join("FETCH_HEAD");
    let modified = modified_secs(&fetch_head)?;
    Some((unix_now() - modified).max(0.0))
}

/// Resolve the freshest base ref to branch a new worktree from:
/// (base_ref, banner_label).
fn resolve_worktree_base(
    repo_root: &Path,
    fetch_timeout: f64,
    freshness_window: f64,
) -> (String, String) {
    let refresh = |remote: &str, branch: &str, reference: &str| -> (String, String) {
        if let Some(age) = fetch_head_age(repo_root) {
            if age < freshness_window && ref_exists(repo_root, reference, 20) {
                return (
                    reference.to_string(),
                    format!("{reference} (fetched {}s ago)", age as i64),
                );
            }
        }
        let result = git_output(repo_root, &["fetch", remote, branch], fetch_timeout as u64);
        if result.success() {
            return (reference.to_string(), format!("{reference} (fetched)"));
        }
        let reason = if result.timed_out {
            format!("fetch timed out after {fetch_timeout}s")
        } else {
            "fetch failed".to_string()
        };
        if ref_exists(repo_root, reference, 20) {
            return (
                reference.to_string(),
                format!("{reference} (cached — {reason})"),
            );
        }
        (
            "HEAD".to_string(),
            format!("HEAD (local — {reason}, no cached {reference})"),
        )
    };

    // 1. Current branch's upstream, if it tracks one.
    if let Some(upstream) = git_out(
        repo_root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        20,
    ) {
        if let Some((remote, branch)) = upstream.split_once('/') {
            if !remote.is_empty() && !branch.is_empty() {
                return refresh(remote, branch, &upstream);
            }
        }
    }

    // 2. Remote default branch (origin/HEAD).
    let mut default_ref = String::new();
    if let Some(head_ref) = git_out(
        repo_root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        20,
    ) {
        default_ref = head_ref
            .strip_prefix("refs/remotes/")
            .unwrap_or(&head_ref)
            .to_string();
    }
    if default_ref.is_empty() {
        let show = git_output(
            repo_root,
            &["remote", "show", "origin"],
            fetch_timeout.max(5.0) as u64,
        );
        if show.success() {
            for line in show.stdout_text().lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("HEAD branch:") {
                    let branch = rest.trim();
                    if !branch.is_empty() && branch != "(unknown)" {
                        default_ref = format!("origin/{branch}");
                    }
                    break;
                }
            }
        }
    }
    if let Some((remote, branch)) = default_ref.split_once('/') {
        if !remote.is_empty() && !branch.is_empty() {
            return refresh(remote, branch, &default_ref);
        }
    }

    // 3. Local HEAD (offline / no remote / detached).
    (
        "HEAD".to_string(),
        "HEAD (local — could not reach remote)".to_string(),
    )
}

/// Sweep the leftovers of a failed/timed-out git worktree add (fail-soft).
fn cleanup_failed_worktree_add(repo_root: &Path, wt_path: &Path, branch_name: &str) {
    let path = wt_path.to_string_lossy().to_string();
    // Unlock first: worktree remove --force refuses a locked tree.
    git_quiet(repo_root, &["worktree", "unlock", &path], 15);
    git_quiet(repo_root, &["worktree", "remove", "--force", &path], 15);
    if wt_path.exists() {
        drop(fs::remove_dir_all(wt_path));
    }
    git_quiet(repo_root, &["worktree", "prune"], 15);
    git_quiet(repo_root, &["branch", "-D", branch_name], 15);
}

/// git worktree add with a local-HEAD retry. Every failed attempt is swept.
fn worktree_add(
    repo_root: &Path,
    wt_path: &Path,
    branch_name: &str,
    base_ref: &str,
    base_label: &str,
) -> Result<(String, String)> {
    let path = wt_path.to_string_lossy();

    // checkout.workers parallelizes materialization; older git ignores unknown -c keys.
    let parallel = [
        "-c",
        "checkout.workers=8",
        "-c",
        "checkout.thresholdForParallelism=100",
        "worktree",
        "add",
        &path,
        "-b",
        branch_name,
        base_ref,
    ];
    let first = git_output(repo_root, &parallel, WORKTREE_ADD_TIMEOUT_SECS);
    if first.success() {
        return Ok((base_ref.to_string(), base_label.to_string()));
    }
    if base_ref != "HEAD" {
        // A partial fetch can leave the remote ref unusable; never hard-fail on a
        // sync hiccup.
        cleanup_failed_worktree_add(repo_root, wt_path, branch_name);
        let retry = ["worktree", "add", &path, "-b", branch_name, "HEAD"];
        let retry_result = git_output(repo_root, &retry, WORKTREE_ADD_TIMEOUT_SECS);
        if retry_result.success() {
            return Ok((
                "HEAD".to_string(),
                "HEAD (fallback — remote base failed)".to_string(),
            ));
        }
        cleanup_failed_worktree_add(repo_root, wt_path, branch_name);
        anyhow::bail!(
            "failed to create worktree: {}",
            retry_result.stderr_text().trim()
        );
    }
    cleanup_failed_worktree_add(repo_root, wt_path, branch_name);
    anyhow::bail!("failed to create worktree: {}", first.stderr_text().trim())
}

/// Create a worktree under config.worktree_root() on branch `silver/<name>` (a random
/// `silver-<id>` without a name), from the freshly fetched remote tip when `sync` is set.
pub fn worktree_create(
    config: &Config,
    workspace_root: &Path,
    name: Option<&str>,
    sync: bool,
) -> Result<WorktreeInfo> {
    let repo_root = git_repo_root(workspace_root)
        .ok_or_else(|| anyhow::anyhow!("--worktree requires being inside a git repository"))?;

    let wt_name = match name {
        Some(raw) if !sanitize_worktree_name(raw).is_empty() => sanitize_worktree_name(raw),
        _ => random_worktree_name(),
    };
    let branch_name = format!("{WORKTREE_BRANCH_PREFIX}{wt_name}");

    let worktrees_dir = worktrees_dir(config, &repo_root);
    fs::create_dir_all(&worktrees_dir)
        .with_context(|| format!("create worktrees directory {}", worktrees_dir.display()))?;
    let wt_path = worktrees_dir.join(&wt_name);
    if name.is_some() && wt_path.exists() {
        anyhow::bail!(
            "worktree already exists: {} (pick a different name, or remove it with git worktree remove)",
            wt_path.display()
        );
    }

    ensure_worktrees_gitignored(&repo_root, config);

    let (base_ref, base_label) = if sync {
        resolve_worktree_base(&repo_root, 5.0, 300.0)
    } else {
        (
            "HEAD".to_string(),
            "HEAD (local — worktree_sync disabled)".to_string(),
        )
    };
    let (base_ref, base_label) =
        worktree_add(&repo_root, &wt_path, &branch_name, &base_ref, &base_label)?;
    copy_worktree_includes(&repo_root, &wt_path);

    // Lock so other processes (and git worktree remove) see it is in use.
    let reason = format!("silver pid={}", std::process::id());
    let path = wt_path.to_string_lossy().to_string();
    let _ = git_output(
        &repo_root,
        &["worktree", "lock", "--reason", &reason, &path],
        GIT_TIMEOUT_SECS,
    );

    Ok(WorktreeInfo {
        path: wt_path,
        branch: branch_name,
        repo_root,
        base: base_ref,
        base_label,
    })
}

/// Validate a worktree name as a single path component.
fn validate_worktree_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("worktree name must not be empty");
    }
    let path = Path::new(trimmed);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(trimmed.to_string()),
        _ => anyhow::bail!("worktree name {trimmed:?} must be a single path component"),
    }
}

/// Outcome of worktree_remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRemove {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub removed: bool,
    pub forced: bool,
}

/// Remove the worktree named name under the repo's worktrees directory. When
/// force is true, passes --force; the branch is left intact.
pub fn worktree_remove(
    config: &Config,
    workspace_root: &Path,
    name: &str,
    force: bool,
) -> Result<WorktreeRemove> {
    let repo_root = require_repo_root(workspace_root)?;
    let safe_name = validate_worktree_name(name)?;
    let wt_path = worktrees_dir(config, &repo_root).join(&safe_name);
    if !wt_path.exists() {
        anyhow::bail!("worktree not found: {}", wt_path.display());
    }

    let target = resolve_path(&wt_path);
    let branch = worktree_list(&repo_root)
        .unwrap_or_default()
        .into_iter()
        .find(|entry| resolve_path(&entry.path) == target)
        .and_then(|entry| entry.branch);

    let path = wt_path.to_string_lossy().to_string();
    git_quiet(&repo_root, &["worktree", "unlock", &path], 10);
    let mut args: Vec<&str> = vec!["worktree", "remove", &path];
    if force {
        args.push("--force");
    }
    let result = git_output(&repo_root, &args, 30);
    if !result.success() {
        anyhow::bail!(
            "failed to remove worktree {}: {}",
            wt_path.display(),
            result.stderr_text().trim()
        );
    }
    Ok(WorktreeRemove {
        path: wt_path,
        branch,
        removed: true,
        forced: force,
    })
}

// ── Merge-verdict cache ──────────────────────────────────────────────────────

fn merge_cache_path(config: &Config) -> PathBuf {
    config
        .data_dir()
        .join("cache")
        .join("worktree_merge_verdicts.json")
}

/// Load the git cherry verdict cache. Missing/corrupt cache = empty.
fn load_merge_cache(config: &Config) -> BTreeMap<String, bool> {
    let Ok(text) = fs::read_to_string(merge_cache_path(config)) else {
        return BTreeMap::new();
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeMap::new();
    };
    let serde_json::Value::Object(entries) = value["verdicts"].take() else {
        return BTreeMap::new();
    };
    entries
        .into_iter()
        .filter_map(|(key, value)| value.as_bool().map(|flag| (key, flag)))
        .collect()
}

/// Atomically persist the newest MERGE_CACHE_MAX verdicts. Never raises.
fn save_merge_cache(config: &Config, cache: &BTreeMap<String, bool>) {
    let path = merge_cache_path(config);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let payload = serde_json::json!({ "version": 1, "verdicts": cache });
    let Ok(text) = serde_json::to_string(&payload) else {
        return;
    };
    let temp = path.with_extension("json.tmp");
    if fs::write(&temp, text).is_ok() {
        drop(fs::rename(&temp, &path));
    }
}

// ── Startup pruner / GC ──────────────────────────────────────────────────────

/// What the startup pruner did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    pub preserved: Vec<String>,
    pub kept_branches: Vec<String>,
    pub removed: Vec<String>,
    pub orphaned_branches: Vec<String>,
}

struct PruneCandidate {
    entry: PathBuf,
    mtime: f64,
}

struct PruneVerdict {
    entry: PathBuf,
    mtime: f64,
    verdict: &'static str,
    lock_state: LockState,
}

/// Phase 1, stat-only age filter. silver-* trees age on max_age_hours,
/// deliberately named trees at 3x.
fn prune_candidates(worktrees_dir: &Path, max_age_hours: u64, now: f64) -> Vec<PruneCandidate> {
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    paths.sort();
    let mut candidates = Vec::new();
    for entry in paths {
        let name = entry
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_kanban_tree(&name) {
            continue;
        }
        let Some(mtime) = modified_secs(&entry) else {
            continue;
        };
        let tier_hours = if name.starts_with(WORKTREE_RANDOM_PREFIX) {
            max_age_hours
        } else {
            max_age_hours.saturating_mul(3)
        };
        if mtime > now - (tier_hours as f64 * 3600.0) {
            continue; // Too recent — skip
        }
        candidates.push(PruneCandidate { entry, mtime });
    }
    candidates
}

/// Phase 2, read-only classification. Verdicts: dirty / unpushed /
/// locked-live / reap / reap-keep-branch.
fn classify_prune_candidates(
    config: &Config,
    repo_root: &Path,
    candidates: Vec<PruneCandidate>,
) -> Vec<PruneVerdict> {
    let mut merge_cache = load_merge_cache(config);
    let cache_size_before = merge_cache.len();
    let mut remote_heads: Option<Option<BTreeMap<String, String>>> = None;
    let mut verdicts = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let entry = candidate.entry;
        // Never delete real work regardless of age: only clean, merged/pushed
        // trees are reaped.
        if worktree_is_dirty(&entry, 5) {
            verdicts.push(PruneVerdict {
                entry,
                mtime: candidate.mtime,
                verdict: "dirty",
                lock_state: LockState::Unlocked,
            });
            continue;
        }
        let mut keep_branch = false;
        if worktree_has_unpushed_commits(&entry, 5) {
            // Squash-merge escape hatch: patch-equivalent commits are merged.
            let mut merged = worktree_commits_all_merged_upstream(
                &entry,
                30,
                PRUNE_CHERRY_MAX_AHEAD,
                &mut merge_cache,
            );
            if !merged {
                // Rebase-merge escape hatch: cherry misses changed patch-ids.
                merged = worktree_branch_pr_merged(&entry, 15, &mut merge_cache);
            }
            if !merged {
                if remote_heads.is_none() {
                    remote_heads = Some(fetch_remote_branch_heads(repo_root, 10));
                }
                let heads = remote_heads.as_ref().and_then(|heads| heads.as_ref());
                if !worktree_branch_pushed_exact(&entry, heads, 10) {
                    verdicts.push(PruneVerdict {
                        entry,
                        mtime: candidate.mtime,
                        verdict: "unpushed",
                        lock_state: LockState::Unlocked,
                    });
                    continue;
                }
            }
            keep_branch = !merged;
        }

        // Live lock = running silver; a dead lock is unlocked in phase 3.
        let lock_state = worktree_lock_is_live(repo_root, &entry, 5);
        if lock_state == LockState::Live {
            verdicts.push(PruneVerdict {
                entry,
                mtime: candidate.mtime,
                verdict: "locked-live",
                lock_state,
            });
            continue;
        }
        verdicts.push(PruneVerdict {
            entry,
            mtime: candidate.mtime,
            verdict: if keep_branch {
                "reap-keep-branch"
            } else {
                "reap"
            },
            lock_state,
        });
    }

    if merge_cache.len() != cache_size_before {
        save_merge_cache(config, &merge_cache);
    }
    verdicts
}

/// Phase 3, serial unlock / remove / branch -D.
fn reap_prune_verdicts(
    repo_root: &Path,
    verdicts: Vec<PruneVerdict>,
    stale_work_cutoff: f64,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut preserved_stale = Vec::new();
    let mut kept_branches = Vec::new();
    let mut removed = Vec::new();
    for verdict in verdicts {
        if verdict.verdict == "dirty" || verdict.verdict == "unpushed" {
            let reason = if verdict.verdict == "dirty" {
                "uncommitted changes"
            } else {
                "unpushed commits"
            };
            if verdict.mtime <= stale_work_cutoff {
                let name = verdict
                    .entry
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                preserved_stale.push(format!("{name} ({reason})"));
            }
            continue;
        }
        if verdict.verdict == "locked-live" {
            continue;
        }

        let path = verdict.entry.to_string_lossy().to_string();
        if verdict.lock_state == LockState::Dead {
            git_quiet(repo_root, &["worktree", "unlock", &path], 10);
        }
        let branch = git_output(&verdict.entry, &["branch", "--show-current"], 5)
            .stdout_text()
            .trim()
            .to_string();
        let remove_result = git_output(
            repo_root,
            &["worktree", "remove", &path, "--force"],
            CLEANUP_TIMEOUT_SECS,
        );
        if !remove_result.success() {
            continue;
        }
        let name = verdict
            .entry
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if !branch.is_empty() && verdict.verdict == "reap-keep-branch" {
            kept_branches.push(branch);
        } else if !branch.is_empty() {
            git_quiet(repo_root, &["branch", "-D", &branch], GIT_TIMEOUT_SECS);
        }
        removed.push(name);
    }
    (preserved_stale, kept_branches, removed)
}

/// Delete local silver/silver-* and pr-* branches with no worktree, except
/// those in protect.
fn prune_orphaned_branches(repo_root: &Path, protect: &[String]) -> Vec<String> {
    let Some(listing) = git_out(
        repo_root,
        &["branch", "--format=%(refname:short)"],
        GIT_TIMEOUT_SECS,
    ) else {
        return Vec::new();
    };

    let Some(worktree_output) = git_out(
        repo_root,
        &["worktree", "list", "--porcelain"],
        GIT_TIMEOUT_SECS,
    ) else {
        return Vec::new(); // can't determine active branches: bail
    };
    let mut active: Vec<String> = worktree_output
        .lines()
        .filter_map(|line| line.strip_prefix("branch refs/heads/"))
        .map(|branch| branch.trim().to_string())
        .collect();
    if let Some(current) = git_out(repo_root, &["branch", "--show-current"], 5) {
        if !current.is_empty() {
            active.push(current);
        }
    }
    active.push("main".to_string());

    let orphaned: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|branch| branch.starts_with("silver/silver-") || branch.starts_with("pr-"))
        .map(str::to_string)
        .filter(|branch| !active.contains(branch) && !protect.contains(branch))
        .collect();
    if orphaned.is_empty() {
        return Vec::new();
    }
    for chunk in orphaned.chunks(50) {
        let mut args = vec!["branch", "-D"];
        args.extend(chunk.iter().map(String::as_str));
        drop(git_output(repo_root, &args, 30));
    }
    orphaned
}

/// Remove stale worktrees and orphaned branches. A dirty tree is never removed, and unpushed
/// commits only when patch-equivalent to upstream, in a MERGED PR, or exactly at origin.
pub fn worktree_prune(
    config: &Config,
    workspace_root: &Path,
    max_age_hours: u64,
) -> Result<PruneOutcome> {
    let repo_root = require_repo_root(workspace_root)?;
    let worktrees_dir = worktrees_dir(config, &repo_root);
    if !worktrees_dir.exists() {
        return Ok(PruneOutcome {
            orphaned_branches: prune_orphaned_branches(&repo_root, &[]),
            ..PruneOutcome::default()
        });
    }
    // Shallow clones make every aged tree read as unpushed forever; deepen once.
    if repo_is_shallow(&repo_root, 5) {
        deepen_shallow_repo(&repo_root, 600);
    }

    let now = unix_now();
    let candidates = prune_candidates(&worktrees_dir, max_age_hours, now);
    if candidates.is_empty() {
        return Ok(PruneOutcome {
            orphaned_branches: prune_orphaned_branches(&repo_root, &[]),
            ..PruneOutcome::default()
        });
    }
    let verdicts = classify_prune_candidates(config, &repo_root, candidates);
    let (preserved, kept_branches, removed) =
        reap_prune_verdicts(&repo_root, verdicts, now - (STALE_WORK_SECS as f64));
    let orphaned_branches = prune_orphaned_branches(&repo_root, &kept_branches);
    Ok(PruneOutcome {
        preserved,
        kept_branches,
        removed,
        orphaned_branches,
    })
}

/// (tree_count, total_size_mb) for the escalation notice.
pub fn worktrees_summary(config: &Config, repo_root: &Path) -> (usize, Option<u64>) {
    let worktrees_dir = worktrees_dir(config, repo_root);
    if !worktrees_dir.is_dir() {
        return (0, None);
    }
    let count = fs::read_dir(&worktrees_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .count()
        })
        .unwrap_or(0);
    (count, tree_size_mb(&worktrees_dir, 20))
}

/// Cheap directory size via du -sm — best-effort, None on failure.
fn tree_size_mb(path: &Path, timeout_secs: u64) -> Option<u64> {
    let mut cmd = Command::new("du");
    cmd.arg("-sm").arg(path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = run_command(cmd, timeout_secs);
    if !output.success() {
        return None;
    }
    output.stdout_text().split_whitespace().next()?.parse().ok()
}

// ── worktree_gc audit / reclaim ──────────────────────────────────────────────

/// One classified tree under .worktrees/ (Hermes worktree_gc TreeRecord).
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRecord {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub age_days: f64,
    pub size_mb: Option<u64>,
    pub verdict: String,
    pub reason: String,
    pub untracked: Vec<String>,
}

/// Classify one tree under .worktrees/ without mutating anything.
fn classify_tree(
    repo_root: &Path,
    entry: &Path,
    merge_cache: &mut BTreeMap<String, bool>,
    remote_heads: Option<&BTreeMap<String, String>>,
) -> (String, String, Vec<String>) {
    let name = entry
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if is_kanban_tree(&name) {
        return (
            "keep".to_string(),
            "kanban task tree (owned by kanban gc)".to_string(),
            Vec::new(),
        );
    }
    if worktree_lock_is_live(repo_root, entry, 5) == LockState::Live {
        return (
            "keep".to_string(),
            "in use by a running hermes session".to_string(),
            Vec::new(),
        );
    }
    let (tracked_dirty, untracked) = dirty_split(entry);
    if tracked_dirty {
        return (
            "keep".to_string(),
            "uncommitted tracked changes (real work)".to_string(),
            Vec::new(),
        );
    }
    let archive_note = format!("{} untracked file(s) will be archived", untracked.len());
    let unmerged = worktree_has_unpushed_commits(entry, 5)
        && !worktree_commits_all_merged_upstream(entry, 30, MAX_CHERRY_AHEAD, merge_cache);
    if unmerged {
        if !worktree_branch_pushed_exact(entry, remote_heads, 10) {
            return (
                "keep".to_string(),
                "unpushed commits not found upstream".to_string(),
                Vec::new(),
            );
        }
        if untracked.is_empty() {
            return (
                "reap-keep-branch".to_string(),
                "pushed to origin (open-PR lane); branch kept".to_string(),
                Vec::new(),
            );
        }
        return (
            "reap-keep-branch".to_string(),
            format!("pushed to origin (open-PR lane); branch kept; {archive_note}"),
            untracked,
        );
    }
    if untracked.is_empty() {
        (
            "reap".to_string(),
            "clean and fully merged/pushed".to_string(),
            Vec::new(),
        )
    } else {
        (
            "reap-archive".to_string(),
            format!("merged/pushed; {archive_note}"),
            untracked,
        )
    }
}

/// Classify every tree under .worktrees/ without mutating anything.
/// older_than_days only RESTRICTS — it never widens eligibility.
pub fn audit_worktrees(
    config: &Config,
    workspace_root: &Path,
    with_sizes: bool,
    older_than_days: Option<f64>,
) -> Result<Vec<TreeRecord>> {
    let repo_root = require_repo_root(workspace_root)?;
    let worktrees_dir = worktrees_dir(config, &repo_root);
    if !worktrees_dir.is_dir() {
        return Ok(Vec::new());
    }
    if repo_is_shallow(&repo_root, 5) {
        deepen_shallow_repo(&repo_root, 600);
    }
    let mut merge_cache = load_merge_cache(config);
    let cache_size_before = merge_cache.len();
    let remote_heads = fetch_remote_branch_heads(&repo_root, 20);

    let mut paths: Vec<PathBuf> = fs::read_dir(&worktrees_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    paths.sort();

    let now = unix_now();
    let mut records = Vec::new();
    for entry in paths {
        let name = entry
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(mtime) = modified_secs(&entry) else {
            continue;
        };
        let age_days = (now - mtime) / 86400.0;
        let branch = git_output(&entry, &["branch", "--show-current"], 5)
            .stdout_text()
            .trim()
            .to_string();
        let (mut verdict, mut reason, untracked) =
            classify_tree(&repo_root, &entry, &mut merge_cache, remote_heads.as_ref());
        if let Some(threshold) = older_than_days {
            if REAP_VERDICTS.contains(&verdict.as_str()) && age_days < threshold {
                verdict = "keep".to_string();
                reason =
                    format!("reapable but only {age_days:.1} d old (--older-than {threshold})");
            }
        }
        records.push(TreeRecord {
            name,
            size_mb: if with_sizes {
                tree_size_mb(&entry, 30)
            } else {
                None
            },
            path: entry,
            branch,
            age_days,
            verdict,
            reason,
            untracked,
        });
    }
    if merge_cache.len() != cache_size_before {
        save_merge_cache(config, &merge_cache);
    }
    Ok(records)
}

/// Copy untracked files out of a doomed tree; None on any failure.
fn archive_untracked(config: &Config, tree: &Path, untracked: &[String]) -> Option<PathBuf> {
    let name = tree
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dest = config
        .data_dir()
        .join("archive")
        .join("worktree-prune")
        .join(format!("{name}-{stamp}"));
    for rel in untracked {
        let src = tree.join(rel);
        if !src.exists() || src.is_symlink() {
            continue;
        }
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            if fs::create_dir_all(parent).is_err() {
                return None;
            }
        }
        let copied = if src.is_dir() {
            copy_any_dir(&src, &target)
        } else {
            fs::copy(&src, &target).map(|_| ())
        };
        if copied.is_err() {
            return None;
        }
    }
    if dest.exists() {
        Some(dest)
    } else {
        None
    }
}

fn copy_any_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_any_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Remove every reap-verdict tree from a frozen audit list (never re-globs).
pub fn reclaim_worktrees(
    config: &Config,
    workspace_root: &Path,
    dry_run: bool,
    records: Option<&[TreeRecord]>,
) -> Result<Vec<String>> {
    let repo_root = require_repo_root(workspace_root)?;
    let owned;
    let records: &[TreeRecord] = match records {
        Some(records) => records,
        None => {
            owned = audit_worktrees(config, &repo_root, false, None)?;
            &owned
        }
    };
    let mut actions = Vec::new();
    for record in records {
        if !REAP_VERDICTS.contains(&record.verdict.as_str()) {
            continue;
        }
        if dry_run {
            actions.push(format!("would remove {} ({})", record.name, record.reason));
            continue;
        }
        if !record.untracked.is_empty() {
            match archive_untracked(config, &record.path, &record.untracked) {
                Some(archive) => actions.push(format!(
                    "archived {} untracked file(s) -> {}",
                    record.untracked.len(),
                    archive.display()
                )),
                None => {
                    actions.push(format!(
                        "kept {} (archive of untracked files failed)",
                        record.name
                    ));
                    continue;
                }
            }
        }
        // Dead-pid locks must be unlocked or remove --force refuses.
        let path = record.path.to_string_lossy().to_string();
        git_quiet(&repo_root, &["worktree", "unlock", &path], 10);
        let remove_result = git_output(&repo_root, &["worktree", "remove", &path, "--force"], 30);
        if !remove_result.success() {
            actions.push(format!(
                "failed to remove {}: {}",
                record.name,
                remove_result.stderr_text().trim()
            ));
            continue;
        }
        if record.verdict == "reap-keep-branch" {
            actions.push(format!(
                "removed {} (branch {} kept — pushed open-PR lane)",
                record.name, record.branch
            ));
            continue;
        }
        if !record.branch.is_empty() && !PROTECTED_BRANCHES.contains(&record.branch.as_str()) {
            git_quiet(&repo_root, &["branch", "-D", &record.branch], 10);
        }
        actions.push(format!("removed {}", record.name));
    }
    if !dry_run {
        git_quiet(&repo_root, &["worktree", "prune"], CLEANUP_TIMEOUT_SECS);
    }
    Ok(actions)
}

// ── Pack maintenance ─────────────────────────────────────────────────────────

/// Exactly one process per clone gets to repack per REPACK_MIN_INTERVAL_SECS.
fn claim_repack_slot(git_dir: &Path) -> bool {
    let lock = git_dir.join(REPACK_LOCK);
    if let Ok(metadata) = fs::metadata(&lock) {
        let fresh = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map(|age| age.as_secs() < REPACK_MIN_INTERVAL_SECS)
            .unwrap_or(false);
        if fresh {
            return false;
        }
        let stale = lock.with_extension("stale");
        if fs::rename(&lock, &stale).is_err() {
            return false;
        }
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(mut file) => {
            drop(writeln!(file, "{}", std::process::id()));
        }
        Err(_) => return false,
    }
    drop(fs::remove_file(lock.with_extension("stale")));
    true
}

/// Incremental geometric repack with a hard timeout (background).
fn run_bounded_repack(repo_root: &Path) {
    let mut args: Vec<String> = Vec::new();
    if cfg!(unix) {
        args.extend(["nice".into(), "-n".into(), "19".into()]);
    }
    args.extend([
        "git".into(),
        "repack".into(),
        "-d".into(),
        "--geometric=2".into(),
        "--write-midx".into(),
        "--quiet".into(),
    ]);
    let Some((program, rest)) = args.split_first() else {
        return;
    };
    let mut cmd = Command::new(program);
    cmd.args(rest);
    cmd.current_dir(repo_root);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    drop(run_command(cmd, REPACK_TIMEOUT_SECS));
}

/// Repack the object store when pack files sprawl (background thread, fail-soft).
pub fn maintain_pack_health(repo_root: &Path) {
    let pack_dir = repo_root.join(".git").join("objects").join("pack");
    if !pack_dir.is_dir() {
        return;
    }
    let packs = fs::read_dir(&pack_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .map(|extension| extension == "pack")
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    if packs < PACK_SPRAWL_THRESHOLD {
        return;
    }
    let git_dir = pack_dir
        .parent()
        .and_then(|parent| parent.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.join(".git"));
    if !claim_repack_slot(&git_dir) {
        return;
    }
    let repo_root = repo_root.to_path_buf();
    std::thread::spawn(move || run_bounded_repack(&repo_root));
}

// ── Working-tree diff ────────────────────────────────────────────────────────

/// Diff scope (/diff [working|staged|all|session]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffScope {
    /// Unstaged changes plus untracked files.
    Working,
    /// git diff --cached.
    Staged,
    /// Everything since HEAD plus untracked files.
    All,
    /// Everything since the recorded session baseline plus untracked files.
    Session,
}

impl DiffScope {
    /// Parse a /diff scope token (anything else is a path).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "working" => Some(Self::Working),
            "staged" | "--staged" | "cached" | "--cached" => Some(Self::Staged),
            "all" | "--all" | "head" => Some(Self::All),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Staged => "staged",
            Self::All => "all",
            Self::Session => "session",
        }
    }

    /// The heading Hermes prints for the stat block.
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "Unstaged",
            Self::Staged => "Staged",
            Self::All => "All (vs HEAD)",
            Self::Session => "Session",
        }
    }
}

/// Structured result of one diff collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffResult {
    pub success: bool,
    pub error: Option<String>,
    pub stat: String,
    pub diff: String,
    pub untracked: Vec<String>,
    pub empty: bool,
}

impl DiffResult {
    fn failure(message: &str) -> Self {
        Self {
            success: false,
            error: Some(message.to_string()),
            ..Self::default()
        }
    }
}

/// Resolve the recorded session baseline commit for a workspace.
pub fn session_baseline(workspace_root: &Path) -> Option<String> {
    let repo_root = git_repo_root(workspace_root)?;
    git_out(
        &repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{SESSION_BASELINE_REF}^{{commit}}"),
        ],
        5,
    )
}

/// Record the current HEAD as this workspace's session baseline.
pub fn set_session_baseline(workspace_root: &Path) -> Result<String> {
    let repo_root = require_repo_root(workspace_root)?;
    let head = git_out(&repo_root, &["rev-parse", "HEAD"], 5)
        .ok_or_else(|| anyhow::anyhow!("could not resolve HEAD"))?;
    let result = git_output(&repo_root, &["update-ref", SESSION_BASELINE_REF, &head], 5);
    if !result.success() {
        anyhow::bail!(
            "could not set session baseline: {}",
            result.stderr_text().trim()
        );
    }
    Ok(head)
}

/// Drop the recorded session baseline.
pub fn clear_session_baseline(workspace_root: &Path) -> Result<()> {
    let repo_root = require_repo_root(workspace_root)?;
    drop(git_output(
        &repo_root,
        &["update-ref", "-d", SESSION_BASELINE_REF],
        5,
    ));
    Ok(())
}

fn untracked_files(cwd: &Path) -> Vec<String> {
    let output = git_diff(
        cwd,
        &["ls-files", "--others", "--exclude-standard"],
        DIFF_TIMEOUT_SECS,
    );
    if !output.success() {
        return Vec::new();
    }
    output
        .stdout_text()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Render untracked files as new-file diffs via git diff --no-index.
fn untracked_diff_text(cwd: &Path, files: &[String]) -> String {
    let mut chunks: Vec<String> = Vec::new();
    for rel in files.iter().take(MAX_UNTRACKED_FILES) {
        let args = ["diff", "--no-index", "--", DEV_NULL, rel];
        // --no-index exits 1 when files differ — the success path, so the code is ignored.
        let output = git_diff(cwd, &args, DIFF_TIMEOUT_SECS);
        if output.timed_out {
            continue;
        }
        let text = output.stdout_text();
        if !text.trim().is_empty() {
            chunks.push(text.trim_end_matches('\n').to_string());
        }
    }
    if files.len() > MAX_UNTRACKED_FILES {
        chunks.push(format!(
            "... ({} more untracked files not shown)",
            files.len() - MAX_UNTRACKED_FILES
        ));
    }
    chunks.join("\n")
}

/// Collect a git diff of the working directory:
/// {success, stat, diff, untracked, empty} or a failure with error.
pub fn collect_diff(workspace_root: &Path, scope: DiffScope, paths: &[String]) -> DiffResult {
    if !git_available() {
        return DiffResult::failure("git is not installed or not on PATH.");
    }
    let check = git_output(workspace_root, &["rev-parse", "--is-inside-work-tree"], 5);
    if check.spawn_failed || check.timed_out {
        return DiffResult::failure(&format!("git failed: {}", check.stderr_text().trim()));
    }
    if !check.success() {
        return DiffResult::failure("Not a git repository.");
    }

    let session_base;
    let (base_args, include_untracked): (Vec<&str>, bool) = match scope {
        DiffScope::Working => (vec!["diff"], true),
        DiffScope::Staged => (vec!["diff", "--cached"], false),
        DiffScope::All => (vec!["diff", "HEAD"], true),
        DiffScope::Session => {
            session_base = session_baseline(workspace_root).unwrap_or_else(|| "HEAD".to_string());
            (vec!["diff", &session_base], true)
        }
    };
    let pathspec: Vec<&str> = if paths.is_empty() {
        Vec::new()
    } else {
        std::iter::once("--")
            .chain(paths.iter().map(String::as_str))
            .collect()
    };

    let stat_args = [base_args.as_slice(), &["--stat"], &pathspec].concat();
    let diff_args = [base_args, pathspec].concat();

    let stat_output = git_diff(workspace_root, &stat_args, DIFF_TIMEOUT_SECS);
    let diff_output = git_diff(workspace_root, &diff_args, DIFF_TIMEOUT_SECS * 2);
    if stat_output.timed_out || diff_output.timed_out {
        return DiffResult::failure("git diff timed out.");
    }

    let untracked = if include_untracked && paths.is_empty() {
        untracked_files(workspace_root)
    } else {
        Vec::new()
    };
    let untracked_diff = if untracked.is_empty() {
        String::new()
    } else {
        untracked_diff_text(workspace_root, &untracked)
    };

    let stat = stat_output.stdout_text().trim().to_string();
    let mut diff = diff_output.stdout_text().trim().to_string();
    if !untracked_diff.is_empty() {
        diff = format!("{diff}\n{untracked_diff}").trim().to_string();
    }
    let empty = stat.is_empty() && diff.is_empty() && untracked.is_empty();
    DiffResult {
        success: true,
        error: None,
        stat,
        diff,
        untracked,
        empty,
    }
}

/// Cap a diff body at limit lines with a pointer to the --stat form.
pub fn bound_diff_body(diff: &str, limit: usize) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() > limit {
        format!(
            "{}\n\n  ... ({} more lines — run /diff --stat for a summary)",
            lines[..limit].join("\n"),
            lines.len() - limit
        )
    } else {
        diff.to_string()
    }
}

/// The /diff body: the bounded unified diff text, or the --stat block when stat
/// is true. Failure/empty cases return a human-readable string.
pub fn diff(workspace_root: &Path, scope: DiffScope, stat: bool, paths: &[String]) -> String {
    let result = collect_diff(workspace_root, scope, paths);
    if !result.success {
        return result
            .error
            .unwrap_or_else(|| "Could not generate diff".to_string());
    }
    if result.empty {
        return "No changes.".to_string();
    }
    if stat {
        return result.stat;
    }
    bound_diff_body(&result.diff, DIFF_BODY_LINE_LIMIT)
}

/// Full CLI-shaped rendering: stat block, untracked list, then the bounded diff
/// (matching _handle_diff_command).
pub fn render_diff(result: &DiffResult, scope: DiffScope, stat_only: bool) -> String {
    if !result.success {
        return Option::clone(&result.error)
            .unwrap_or_else(|| "Could not generate diff".to_string());
    }
    if result.empty {
        return "No changes.".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if !result.stat.is_empty() {
        parts.push(format!("{}:\n{}", scope.label(), result.stat));
    }
    if !result.untracked.is_empty() && matches!(scope, DiffScope::Working | DiffScope::All) {
        let mut block = String::from("Untracked:");
        for rel in result.untracked.iter().take(20) {
            block.push_str(&format!("\n    + {rel}"));
        }
        if result.untracked.len() > 20 {
            block.push_str(&format!(
                "\n    ... and {} more",
                result.untracked.len() - 20
            ));
        }
        parts.push(block);
    }
    if !stat_only && !result.diff.is_empty() {
        parts.push(bound_diff_body(&result.diff, DIFF_BODY_LINE_LIMIT));
    }
    parts.join("\n")
}
