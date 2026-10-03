//! Tool contract, registry and approval policy.

use crate::context::RunContext;
use crate::error::CoreResult;
use crate::event::EventEmitter;
use crate::model::ToolSpec;
use silver_protocol::{RiskLevel, ToolCallId};
use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
    pub summary: Option<String>,
    /// The picture the model should see, as (media type, raw bytes). `content` stays a one-line
    /// receipt: the turn loop encodes this into an image part for the next request only, and the
    /// transcript records the receipt alone, so a run's messages never grow by megabytes.
    pub image: Option<(String, Vec<u8>)>,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            summary: None,
            image: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            summary: None,
            image: None,
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    pub fn with_image(mut self, media_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.image = Some((media_type.into(), bytes));
        self
    }
}

/// A run lends its emitter and the call id; a test helper owns them.
pub struct ToolContext<'a> {
    pub run: Arc<RunContext>,
    pub events: Cow<'a, EventEmitter>,
    pub cancel: CancellationToken,
    /// The call being executed, so a tool that runs work of its own can file what it emits
    /// under the call the user is looking at.
    pub call_id: Cow<'a, ToolCallId>,
    /// The run's approval gate. A tool that runs work of its own (delegation) passes it on,
    /// so what it does on the user's behalf is approved exactly like the run itself (INV-9).
    pub gate: Arc<dyn crate::agent::ApprovalGate>,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the tool arguments.
    fn schema(&self) -> serde_json::Value;
    /// Risk can depend on the concrete arguments.
    fn risk(&self, args: &serde_json::Value) -> RiskLevel;
    /// When true the tool is absent from the schema for workspace-less runs.
    fn requires_workspace(&self) -> bool {
        false
    }
    /// The tool's toolset; built-ins are classified by name, and MCP tools override it.
    fn toolset(&self) -> &'static str {
        crate::toolset::builtin_toolset_for(self.name())
    }
    /// A longer timeout this tool needs (delegation runs whole turns). The loop takes the larger of
    /// this and the configured timeout.
    fn timeout_hint(&self) -> Option<std::time::Duration> {
        None
    }
    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome>;
}

/// Registry of the tools available to the agent. Interior mutability lets MCP servers register
/// after it is shared with the running agent.
#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<Vec<Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(Vec::new()),
        }
    }

    /// Add a tool while the registry is being assembled. Safe to call before the
    /// agent runs.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools
            .get_mut()
            .expect("tool registry lock poisoned")
            .push(tool);
    }

    /// Add a tool to a registry that is already shared, for example an `Arc`
    /// held by the daemon while MCP servers connect at startup.
    pub fn register_shared(&self, tool: Arc<dyn Tool>) {
        self.tools
            .write()
            .expect("tool registry lock poisoned")
            .push(tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .expect("tool registry lock poisoned")
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    /// Every registered tool, including tools hidden from workspace-less runs.
    pub fn all(&self) -> RwLockReadGuard<'_, Vec<Arc<dyn Tool>>> {
        self.tools.read().expect("tool registry lock poisoned")
    }

    /// The distinct toolsets present in the registry, sorted and deduplicated.
    /// The daemon extends the built-in list with this when it resolves a
    /// selection, so dynamically registered MCP toolsets become selectable.
    pub fn toolsets(&self) -> Vec<String> {
        let mut toolsets: Vec<String> = self
            .tools
            .read()
            .expect("tool registry lock poisoned")
            .iter()
            .map(|tool| tool.toolset().to_string())
            .collect();
        toolsets.sort();
        toolsets.dedup();
        toolsets
    }

    /// Specs visible to the model for the current run (all toolsets).
    pub fn specs(&self, has_workspace: bool) -> Vec<ToolSpec> {
        self.specs_for(has_workspace, &crate::toolset::ToolsetSelection::all())
    }

    /// Specs the model may see for a run: workspace-visible and inside
    /// `selection`. The daemon filters the model-facing tools per run with this.
    pub fn specs_for(
        &self,
        has_workspace: bool,
        selection: &crate::toolset::ToolsetSelection,
    ) -> Vec<ToolSpec> {
        self.tools
            .read()
            .expect("tool registry lock poisoned")
            .iter()
            .filter(|tool| {
                (has_workspace || !tool.requires_workspace()) && selection.allows(tool.toolset())
            })
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }

    /// Names visible to the model for the current run (all toolsets).
    pub fn names(&self, has_workspace: bool) -> Vec<&'static str> {
        self.names_for(has_workspace, &crate::toolset::ToolsetSelection::all())
    }

    /// Names the model may see for a run, restricted to `selection`.
    pub fn names_for(
        &self,
        has_workspace: bool,
        selection: &crate::toolset::ToolsetSelection,
    ) -> Vec<&'static str> {
        self.tools
            .read()
            .expect("tool registry lock poisoned")
            .iter()
            .filter(|tool| {
                (has_workspace || !tool.requires_workspace()) && selection.allows(tool.toolset())
            })
            .map(|tool| tool.name())
            .collect()
    }
}

/// Registration on a registry already shared in an `Arc`, for late contributors such as MCP
/// servers; see [`ToolRegistry::register_shared`].
pub trait ToolRegistryExt {
    fn register(&self, tool: Arc<dyn Tool>);
}

impl ToolRegistryExt for Arc<ToolRegistry> {
    fn register(&self, tool: Arc<dyn Tool>) {
        self.register_shared(tool);
    }
}

/// A command-safety finding. `pattern_key` is persisted in allow/deny records and must stay stable;
/// `description` is only shown to people.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DangerFinding {
    pub pattern_key: &'static str,
    pub description: &'static str,
}

impl DangerFinding {
    const fn new(pattern_key: &'static str, description: &'static str) -> Self {
        Self {
            pattern_key,
            description,
        }
    }
}

impl fmt::Display for DangerFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description)
    }
}

// Hardline floor: refused before the approval gate, so an always-approve decision cannot reach
// them. Each of these is also reported by dangerous_command so callers that only consult the
// recoverable tier still see the finding.
const F_RM_ROOT: DangerFinding =
    DangerFinding::new("rm_root", "recursive delete of root filesystem");
const F_RM_SYSTEM: DangerFinding =
    DangerFinding::new("rm_system_dir", "recursive delete of a system directory");
const F_RM_HOME: DangerFinding =
    DangerFinding::new("rm_home", "recursive delete of the home directory");
const F_DD_BLOCK: DangerFinding = DangerFinding::new("dd_block_device", "dd to a raw block device");
const F_REDIRECT_BLOCK: DangerFinding =
    DangerFinding::new("redirect_block_device", "redirect to a raw block device");
const F_MKFS: DangerFinding = DangerFinding::new("mkfs", "format a filesystem (mkfs)");
const F_FORK_BOMB: DangerFinding = DangerFinding::new("fork_bomb", "fork bomb");
const F_KILL_ALL: DangerFinding =
    DangerFinding::new("kill_all_processes", "kill all processes (kill -1)");
const F_POWER: DangerFinding = DangerFinding::new("host_power", "system shutdown/reboot/poweroff");
const F_INIT: DangerFinding = DangerFinding::new("init_runlevel", "init 0/6 (shutdown/reboot)");
const F_SYSTEMCTL_POWER: DangerFinding =
    DangerFinding::new("systemctl_power", "systemctl poweroff/reboot/halt");
const F_TELINIT: DangerFinding =
    DangerFinding::new("telinit_runlevel", "telinit 0/6 (shutdown/reboot)");
const F_PIPE_SHELL: DangerFinding =
    DangerFinding::new("pipe_to_shell", "pipe a download into a shell");
const F_SUDO_STDIN: DangerFinding =
    DangerFinding::new("sudo_stdin", "sudo password guessing via stdin (sudo -S)");

// Recoverable-danger findings: a human may approve these, but they always reach the gate.
const D_CHMOD: DangerFinding = DangerFinding::new(
    "recursive_permissions_system",
    "recursive chmod/chown on a system path",
);
const D_SQL_DROP: DangerFinding = DangerFinding::new("sql_drop", "SQL DROP TABLE/DATABASE");
const D_SQL_DELETE: DangerFinding =
    DangerFinding::new("sql_delete_without_where", "SQL DELETE without WHERE");
const D_METADATA: DangerFinding = DangerFinding::new(
    "cloud_metadata",
    "cloud metadata endpoint access (instance credentials)",
);
const D_GIT_PUSH: DangerFinding =
    DangerFinding::new("git_force_push", "git force push (rewrites remote history)");
const D_GIT_RESET: DangerFinding = DangerFinding::new(
    "git_reset_hard",
    "git reset --hard (destroys uncommitted changes)",
);
const D_GIT_CLEAN: DangerFinding = DangerFinding::new(
    "git_clean_force",
    "git clean with force (deletes untracked files)",
);
const D_DOCKER: DangerFinding =
    DangerFinding::new("docker_host", "docker daemon host override (docker -H)");
const D_DAEMON_KILL: DangerFinding =
    DangerFinding::new("daemon_self_kill", "kill the running agent daemon");

/// Daemon-owned approval policy. Clients can never relax it per request (INV-9).
#[derive(Clone, Debug)]
pub struct ApprovalPolicy {
    pub write_requires_approval: bool,
    pub command_requires_approval: bool,
    /// Shell-command globs that are always refused before the approval gate. Operator
    /// configuration, but enforced like the hardline floor: no approval decision, and no
    /// disabled approval mode, can allow a matching command through.
    pub deny_commands: Vec<String>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            write_requires_approval: true,
            command_requires_approval: true,
            deny_commands: Vec::new(),
        }
    }
}

impl ApprovalPolicy {
    pub fn requires_approval(&self, risk: RiskLevel) -> bool {
        match risk {
            RiskLevel::Read | RiskLevel::Memory => false,
            RiskLevel::Write => self.write_requires_approval,
            RiskLevel::Process => self.command_requires_approval,
            RiskLevel::Destructive => true,
        }
    }

    /// A denial no approval can override: destructive shell commands refused before the approval
    /// gate, so "always approve" cannot reach them. execute_code runs another language and is not
    /// matched.
    pub fn hardline_denial(&self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        self.hardline_finding(tool_name, args)
            .map(|finding| finding.description.to_string())
    }

    /// The hardline finding, with its stable pattern key, for callers that persist decisions.
    pub fn hardline_finding(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<DangerFinding> {
        if !is_shell_tool(tool_name) {
            return None;
        }
        let command = command_text(args);
        if command.trim().is_empty() {
            return None;
        }
        detect_hardline(&command)
    }

    /// An operator deny-glob (`*`, `?`) matching the whole command or joined argv. Checked before
    /// the approval gate, so it holds even with approvals off; returns the pattern.
    pub fn denied_command(&self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        if !is_shell_tool(tool_name) || self.deny_commands.is_empty() {
            return None;
        }
        let command = command_text(args);
        let command = command.trim();
        if command.is_empty() {
            return None;
        }
        self.deny_commands
            .iter()
            .find(|pattern| glob_matches(pattern, command))
            .cloned()
    }

    /// A dangerous command a human may still approve; callers escalate its risk so it always
    /// reaches the gate. Covers recursive deletes, raw device writes, pipe-to-shell, destructive
    /// SQL and git, daemon self-kill and similar.
    pub fn dangerous_command(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<DangerFinding> {
        if !is_shell_tool(tool_name) {
            return None;
        }
        let command = command_text(args);
        if command.trim().is_empty() {
            return None;
        }
        detect_dangerous(&command)
    }

    /// Escalate a tool's declared risk when its arguments carry a dangerous command, so the
    /// approval gate always fires.
    pub fn escalated_risk(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        declared: RiskLevel,
    ) -> RiskLevel {
        if self.dangerous_command(tool_name, args).is_some() {
            RiskLevel::Destructive
        } else {
            declared
        }
    }

    /// Human-facing detail for a dangerous command, so the approval card can say why it is
    /// risky.
    pub fn approval_description(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        self.dangerous_command(tool_name, args)
            .map(|finding| finding.description.to_string())
    }
}

/// Tools whose arguments carry a shell command line.
pub(crate) fn is_shell_tool(tool_name: &str) -> bool {
    matches!(tool_name, "run_command" | "bash")
}

/// The shell command carried by a tool call, whether it is a string or an argv array.
pub(crate) fn command_text(args: &serde_json::Value) -> String {
    if let Some(command) = args.get("command").and_then(|value| value.as_str()) {
        return command.to_string();
    }
    if let Some(argv) = args.get("argv").and_then(|value| value.as_array()) {
        return argv
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
    }
    String::new()
}

/// Shell-style glob matching with '*' (any run, including empty) and '?' (exactly one
/// character). Every other character matches itself. Linear-time two-pointer scan.
pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // Position of the most recent '*' in the pattern and the text index it has consumed.
    let mut star: Option<usize> = None;
    let mut star_text = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_text = t;
            p += 1;
        } else if let Some(star_pattern) = star {
            // Backtrack: let the last '*' absorb one more character.
            star_text += 1;
            t = star_text;
            p = star_pattern + 1;
        } else {
            return false;
        }
    }
    // Any trailing '*' can match the empty string.
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// One shell word after quote/escape syntax has been removed.
#[derive(Clone, Debug)]
struct ShellWord {
    content: String,
}

/// One simple command (a separator-free run of words) plus its redirection.
#[derive(Clone, Debug, Default)]
struct SimpleCommand {
    words: Vec<ShellWord>,
    redirect_block_device: bool,
    /// The files `>` and `>>` write to, unquoted and unescaped. Empty for a descriptor or
    /// /dev/null, which write nothing a later check can see.
    redirects: Vec<String>,
}

#[derive(Default)]
struct LexState {
    words: Vec<ShellWord>,
    word: String,
    word_started: bool,
    commands: Vec<SimpleCommand>,
    redirect: bool,
    redirects: Vec<String>,
    /// A `>` was read, so the next word is the file it writes, not an argument.
    redirecting: bool,
}

impl LexState {
    fn flush_word(&mut self) {
        if self.word_started {
            let word = std::mem::take(&mut self.word);
            if std::mem::take(&mut self.redirecting) {
                self.redirect |= is_block_device_path(&word.to_ascii_lowercase());
                if !matches!(
                    word.as_str(),
                    "" | "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty"
                ) {
                    self.redirects.push(word);
                }
            } else {
                self.words.push(ShellWord { content: word });
            }
        }
        self.word.clear();
        self.word_started = false;
    }

    fn flush_command(&mut self) {
        self.flush_word();
        if !self.words.is_empty() || !self.redirects.is_empty() || self.redirect {
            self.commands.push(SimpleCommand {
                words: std::mem::take(&mut self.words),
                redirect_block_device: self.redirect,
                redirects: std::mem::take(&mut self.redirects),
            });
        }
        self.redirect = false;
        self.redirecting = false;
    }
}

/// Candidate spellings of a command worth scanning: the command itself, plus a backslash-flattened
/// variant when it contains a Windows drive/UNC path (whose backslashes read as shell escapes).
fn detection_variants(command: &str) -> Vec<String> {
    let mut variants = vec![command.to_string()];
    if looks_like_windows_path(command) {
        variants.push(command.replace('\\', "/"));
    }
    variants
}

fn looks_like_windows_path(command: &str) -> bool {
    command.contains("\\\\")
        || command
            .as_bytes()
            .windows(3)
            .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\')
}

/// Normalize shell splicing tricks before matching: NUL bytes, backslash-newline continuations and
/// the IFS variable (which the shell expands to whitespace, so a spaced spelling of rm -rf /
/// becomes the literal one).
fn normalize_command_for_detection(command: &str) -> String {
    let mut normalized = String::with_capacity(command.len());
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            i += 1;
            continue;
        }
        if b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            i += 2;
            continue;
        }
        if b == b'\\' && i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
            i += 3;
            continue;
        }
        if b == b'$' && bytes[i + 1..].starts_with(b"IFS") {
            i += 4;
            normalized.push(' ');
            continue;
        }
        if b == b'$' && bytes[i + 1..].starts_with(b"{IFS") {
            if let Some(close) = command[i + 2..].find('}') {
                i += 2 + close + 1;
                normalized.push(' ');
                continue;
            }
        }
        let ch = command[i..].chars().next().unwrap_or(' ');
        normalized.push(ch);
        i += ch.len_utf8();
    }
    normalized
}

/// Replace quoted string CONTENT with spaces for positionless matching. Quote characters stay, and
/// command-substitution or backtick bodies inside double quotes stay raw because the shell really
/// executes them.
fn mask_quoted_prose(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' => {
                out.push('\'');
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    out.push(' ');
                    i += 1;
                }
                if i < chars.len() {
                    out.push('\'');
                    i += 1;
                }
            }
            '"' => {
                out.push('"');
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(' ');
                        out.push(' ');
                        i += 2;
                    } else if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
                        if let Some(end) = matching_paren(&chars, i + 1) {
                            out.extend(chars[i..=end].iter().copied());
                            i = end + 1;
                        } else {
                            out.push(' ');
                            i += 1;
                        }
                    } else if chars[i] == '\u{60}' {
                        if let Some(end) = matching_backtick(&chars, i) {
                            out.extend(chars[i..=end].iter().copied());
                            i = end + 1;
                        } else {
                            out.push(' ');
                            i += 1;
                        }
                    } else {
                        out.push(' ');
                        i += 1;
                    }
                }
                if i < chars.len() {
                    out.push('"');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Index of the closing paren matching the open paren at open, honoring quotes and escapes.
fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == '\\' && q != '\'' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Index of the backtick closing the one at start; only a backslash escapes the next char.
fn matching_backtick(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '\u{60}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Split input into simple commands, recursing into command substitutions and shell-carrier
/// payloads (sh -c, bash -c, eval) because those quoted bodies are code, not prose.
#[expect(
    clippy::too_many_lines,
    reason = "recursive command parsing is inherently long"
)]
fn parse_commands(input: &str, out: &mut Vec<SimpleCommand>, depth: usize) {
    if depth > 6 {
        return;
    }
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut state = LexState::default();
    let mut substitutions: Vec<String> = Vec::new();
    // The delimiters of the heredocs opened on this line, and whether each was quoted.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut line_start = 0;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < len {
        let c = chars[i];
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    state.word.push(c);
                    state.word_started = true;
                }
                i += 1;
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                    i += 1;
                } else if c == '\\' && i + 1 < len {
                    state.word.push(chars[i + 1]);
                    state.word_started = true;
                    i += 2;
                } else if c == '$' && i + 1 < len && chars[i + 1] == '(' {
                    if let Some(end) = matching_paren(&chars, i + 1) {
                        substitutions.push(chars[i + 2..end].iter().copied().collect());
                        state.word_started = true;
                        i = end + 1;
                    } else {
                        state.word.push(c);
                        state.word_started = true;
                        i += 1;
                    }
                } else if c == '\u{60}' {
                    if let Some(end) = matching_backtick(&chars, i) {
                        substitutions.push(chars[i + 1..end].iter().copied().collect());
                        state.word_started = true;
                        i = end + 1;
                    } else {
                        state.word.push(c);
                        state.word_started = true;
                        i += 1;
                    }
                } else {
                    state.word.push(c);
                    state.word_started = true;
                    i += 1;
                }
            }
            _ => {
                if c == '\\' && i + 1 < len {
                    state.word.push(chars[i + 1]);
                    state.word_started = true;
                    i += 2;
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                    state.word_started = true;
                    i += 1;
                } else if c == '\u{60}' {
                    if let Some(end) = matching_backtick(&chars, i) {
                        substitutions.push(chars[i + 1..end].iter().copied().collect());
                        state.word_started = true;
                        i = end + 1;
                    } else {
                        state.word.push(c);
                        state.word_started = true;
                        i += 1;
                    }
                } else if c == '$' && i + 1 < len && chars[i + 1] == '(' {
                    if let Some(end) = matching_paren(&chars, i + 1) {
                        substitutions.push(chars[i + 2..end].iter().copied().collect());
                        state.word_started = true;
                        i = end + 1;
                    } else {
                        state.word.push(c);
                        state.word_started = true;
                        i += 1;
                    }
                } else if c == '>' {
                    // The digits of `2>` name a descriptor, not an argument.
                    if state.word.bytes().all(|b| b.is_ascii_digit()) {
                        state.word_started = false;
                    }
                    state.flush_word();
                    i += 1;
                    if chars.get(i) == Some(&'>') {
                        i += 1;
                    }
                    state.redirecting = true;
                    if chars.get(i) == Some(&'&') {
                        // `>&2` and `2>&1` point at a descriptor, which writes no file.
                        i += 1;
                        let start = i;
                        while chars
                            .get(i)
                            .is_some_and(|c| c.is_ascii_digit() || *c == '-')
                        {
                            i += 1;
                        }
                        state.redirecting = i == start;
                    }
                } else if chars[i..].starts_with(&['<', '<', '<']) {
                    state.word.push_str("<<<");
                    state.word_started = true;
                    i += 3;
                } else if chars[i..].starts_with(&['<', '<']) {
                    state.flush_word();
                    let (delimiter, quoted, end) = heredoc_delimiter(&chars, i + 2);
                    heredocs.push((delimiter, quoted));
                    i = end;
                } else if matches!(c, ';' | '&' | '|' | '\n' | '(' | ')') {
                    state.flush_command();
                    if (c == '&' || c == '|') && i + 1 < len && chars[i + 1] == c {
                        i += 1;
                    }
                    i += 1;
                    if c == '\n' {
                        // A heredoc body is data unless a shell on its line reads it.
                        let shell_reads = state.commands[line_start..].iter().any(|command| {
                            effective_command(command)
                                .is_some_and(|(word, _)| is_shell(&basename_lower(&word.content)))
                        });
                        for (delimiter, quoted) in std::mem::take(&mut heredocs) {
                            let Some((body, end)) = heredoc_body(&chars, i, &delimiter) else {
                                break;
                            };
                            i = end;
                            if shell_reads {
                                parse_commands(&body, out, depth + 1);
                            } else if !quoted {
                                heredoc_substitutions(&body, &mut substitutions);
                            }
                        }
                        line_start = state.commands.len();
                    }
                } else if c.is_whitespace() {
                    state.flush_word();
                    i += 1;
                } else {
                    state.word.push(c);
                    state.word_started = true;
                    i += 1;
                }
            }
        }
    }
    state.flush_command();

    for body in substitutions {
        parse_commands(&body, out, depth + 1);
    }
    for command in &state.commands {
        if let Some(payload) = shell_carrier_payload(command) {
            parse_commands(&payload, out, depth + 1);
        }
    }
    out.extend(state.commands);
}

/// The delimiter after `<<` or `<<-`, whether any of it is quoted (which keeps the body
/// literal), and where it ends.
fn heredoc_delimiter(chars: &[char], from: usize) -> (String, bool, usize) {
    let mut i = from + usize::from(chars.get(from) == Some(&'-'));
    while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
        i += 1;
    }
    let mut delimiter = String::new();
    let mut quoted = false;
    let mut quote: Option<char> = None;
    while let Some(&c) = chars.get(i) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => delimiter.push(c),
            None if matches!(c, '\'' | '"') => {
                quote = Some(c);
                quoted = true;
            }
            None if c == '\\' => quoted = true,
            None if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')') => {
                break
            }
            None => delimiter.push(c),
        }
        i += 1;
    }
    (delimiter, quoted, i)
}

/// The heredoc body starting at `from`, and where its closing line ends; None when no line
/// closes it, so the rest is still read as commands.
fn heredoc_body(chars: &[char], from: usize, delimiter: &str) -> Option<(String, usize)> {
    let mut start = from;
    while start < chars.len() {
        let end = chars[start..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |n| start + n);
        let line: String = chars[start..end].iter().collect();
        // `<<-` lets the closing line be indented with tabs.
        if line.trim_start_matches('\t') == delimiter {
            return Some((chars[from..start].iter().collect(), end + 1));
        }
        start = end + 1;
    }
    None
}

/// The command substitutions in an unquoted heredoc body: the only part of it the shell runs.
fn heredoc_substitutions(body: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let span = match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                matching_paren(&chars, i + 1).map(|end| (i + 2, end))
            }
            '\u{60}' => matching_backtick(&chars, i).map(|end| (i + 1, end)),
            _ => None,
        };
        match span {
            Some((start, end)) => {
                out.push(chars[start..end].iter().collect());
                i = end + 1;
            }
            None => i += 1,
        }
    }
}

fn is_shell(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "zsh" | "ksh" | "dash")
}

fn is_block_device_path(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("/dev/") else {
        return false;
    };
    ["sd", "nvme", "hd", "mmcblk", "vd", "xvd", "disk"]
        .iter()
        .any(|prefix| rest.starts_with(prefix))
}

fn basename_lower(word: &str) -> String {
    word.rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase()
}

fn is_env_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_command_wrapper(name: &str) -> bool {
    matches!(
        name,
        "sudo"
            | "env"
            | "exec"
            | "nohup"
            | "setsid"
            | "time"
            | "command"
            | "builtin"
            | "nice"
            | "timeout"
            | "stdbuf"
            | "ionice"
            | "chrt"
            | "taskset"
            | "chroot"
    )
}

fn skip_wrapper_options(words: &[ShellWord], mut i: usize) -> usize {
    while i < words.len() {
        let arg = words[i].content.as_str();
        if arg == "--" {
            return i + 1;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        let option = arg.split('=').next().unwrap_or(arg);
        let takes_operand = matches!(
            option,
            "-u" | "--user"
                | "-g"
                | "--group"
                | "-p"
                | "--prompt"
                | "-h"
                | "--host"
                | "-C"
                | "--close-from"
                | "-n"
                | "--adjustment"
                | "-s"
                | "--signal"
                | "-k"
                | "--kill-after"
                | "-a"
                | "--argv0"
                | "--chdir"
                | "-e"
                | "--error"
                | "-i"
                | "--input"
                | "-o"
                | "--output"
                | "--format"
                | "--split-string"
                | "-S"
        );
        i += 1;
        if takes_operand && !arg.contains('=') && i < words.len() {
            i += 1;
        }
    }
    i
}

/// The executable word of a simple command, skipping env assignments and command wrappers
/// (sudo, env, exec, ...) together with their options.
fn effective_command(command: &SimpleCommand) -> Option<(&ShellWord, usize)> {
    let mut i = 0;
    while i < command.words.len() {
        let word = &command.words[i];
        let name = basename_lower(&word.content);
        if is_env_assignment(&word.content) {
            i += 1;
            continue;
        }
        if is_command_wrapper(&name) {
            i += 1;
            i = skip_wrapper_options(&command.words, i);
            continue;
        }
        return Some((word, i));
    }
    None
}

/// One program a shell command line runs: its lowercased name, its arguments, and whether its
/// output is redirected into a file.
pub(crate) struct ShellProgram {
    pub name: String,
    pub args: Vec<String>,
    /// The files this command's `>` and `>>` write to.
    pub redirects: Vec<String>,
}

/// Every program a shell command line runs, those inside substitutions and `sh -c` payloads
/// included. A bare redirect such as `> file` has an empty name.
pub(crate) fn shell_programs(command: &str) -> Vec<ShellProgram> {
    let mut commands = Vec::new();
    parse_commands(&normalize_command_for_detection(command), &mut commands, 0);
    commands
        .into_iter()
        .map(|command| {
            let effective = effective_command(&command)
                .map(|(word, index)| (basename_lower(&word.content), index));
            let (name, args) = match effective {
                Some((name, index)) => (
                    name,
                    command
                        .words
                        .into_iter()
                        .skip(index + 1)
                        .map(|word| word.content)
                        .collect(),
                ),
                None => (String::new(), Vec::new()),
            };
            ShellProgram {
                name,
                args,
                redirects: command.redirects,
            }
        })
        .collect()
}

/// The payload a shell carrier will execute: sh -c, eval, source, or dot.
fn shell_carrier_payload(command: &SimpleCommand) -> Option<Cow<'_, str>> {
    let (name_word, index) = effective_command(command)?;
    let name = basename_lower(&name_word.content);
    let rest = &command.words[index + 1..];
    match name.as_str() {
        "eval" | "source" | "." => {
            let payload = rest
                .iter()
                .map(|word| word.content.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            (!payload.trim().is_empty()).then_some(Cow::Owned(payload))
        }
        shell if is_shell(shell) => {
            for (i, word) in rest.iter().enumerate() {
                let arg = word.content.as_str();
                if arg == "-c"
                    || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c'))
                {
                    return rest
                        .get(i + 1)
                        .map(|word| Cow::Borrowed(word.content.as_str()));
                }
            }
            None
        }
        _ => None,
    }
}

const SYSTEM_DIR_ROOTS: &[&str] = &[
    "/home",
    "/root",
    "/etc",
    "/usr",
    "/var",
    "/bin",
    "/sbin",
    "/boot",
    "/lib",
    "/system",
    "c:/windows",
];

/// True when a path argument collapses to the filesystem root: /, //, /., /.., /*, and friends.
fn is_root_path(arg: &str) -> bool {
    let mut path = arg.trim_matches(|c| c == '"' || c == '\'');
    while path.ends_with('*') {
        path = &path[..path.len() - 1];
    }
    if path == "/" {
        return true;
    }
    if !path.starts_with('/') {
        return false;
    }
    let mut depth = 0i32;
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => depth = (depth - 1).max(0),
            _ => depth += 1,
        }
    }
    depth == 0
}

fn is_home_path(arg: &str) -> bool {
    let path = arg.trim_matches(|c| c == '"' || c == '\'');
    let path = path.trim_end_matches('*');
    matches!(path, "~" | "~/" | "$home")
}

/// True when the argument names a system root exactly (/etc, /etc/, /etc/*).
fn is_system_dir_root(arg: &str) -> bool {
    let path = arg.trim_matches(|c| c == '"' || c == '\'');
    let path = path.trim_end_matches('*').trim_end_matches('/');
    SYSTEM_DIR_ROOTS.contains(&path)
}

/// True when the argument is a system root or anything under it (/etc/hosts, C:/Windows/System32).
fn is_within_system_dir(arg: &str) -> bool {
    let path = arg.trim_matches(|c| c == '"' || c == '\'');
    let path = path.trim_end_matches('*');
    SYSTEM_DIR_ROOTS.iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
    })
}

fn dd_writes_block_device(arg: &str) -> bool {
    let Some(index) = arg.find("of=") else {
        return false;
    };
    is_block_device_path(arg[index + 3..].trim_matches(|c| c == '"' || c == '\''))
}

/// The fork bomb is positionless (a function definition), so it is matched against quote-masked
/// text: echoing the text is data, not a definition.
fn is_fork_bomb(masked: &str) -> bool {
    let compact: String = masked
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    compact.contains(":(){:|:&};:")
}

fn has_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(bytes[abs - 1]);
        let after = abs + needle.len();
        let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// A download piped into a shell. Matched on quote-masked text so prose cannot trip it, while
/// command-substitution bodies (which run) stay visible.
fn pipe_to_shell(masked: &str) -> bool {
    let lower = masked.to_ascii_lowercase();
    if !has_word(&lower, "curl") && !has_word(&lower, "wget") {
        return false;
    }
    let mut search_from = 0;
    while let Some(pos) = lower[search_from..].find('|') {
        let after = search_from + pos + 1;
        let rest = lower[after..].trim_start();
        let token = rest.split_whitespace().next().unwrap_or("");
        if matches!(
            basename_lower(token).as_str(),
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "sudo"
        ) {
            return true;
        }
        search_from = after;
    }
    false
}

/// sudo -S / -s / --stdin / combined short flags: without a configured password this is stdin
/// password guessing.
fn sudo_stdin(commands: &[SimpleCommand]) -> bool {
    commands.iter().any(|command| {
        let Some(first) = command.words.first() else {
            return false;
        };
        if basename_lower(&first.content) != "sudo" {
            return false;
        }
        command.words[1..].iter().any(|word| {
            let arg = word.content.to_ascii_lowercase();
            arg == "--stdin"
                || arg == "-s"
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('s'))
        })
    })
}

fn hardline_command_match(name: &str, args: &[String]) -> Option<DangerFinding> {
    match name {
        "rm" => {
            for arg in args {
                if !arg.starts_with('-') && is_root_path(arg) {
                    return Some(F_RM_ROOT);
                }
            }
            for arg in args {
                if arg.starts_with('-') {
                    continue;
                }
                if is_home_path(arg) {
                    return Some(F_RM_HOME);
                }
                if is_system_dir_root(arg) {
                    return Some(F_RM_SYSTEM);
                }
            }
            None
        }
        "dd" => args
            .iter()
            .any(|arg| dd_writes_block_device(arg))
            .then_some(F_DD_BLOCK),
        name if name.starts_with("mkfs") => Some(F_MKFS),
        "kill" => args.iter().any(|arg| arg == "-1").then_some(F_KILL_ALL),
        "shutdown" | "reboot" | "halt" | "poweroff" => Some(F_POWER),
        "init" => args
            .iter()
            .any(|arg| arg == "0" || arg == "6")
            .then_some(F_INIT),
        "telinit" => args
            .iter()
            .any(|arg| arg == "0" || arg == "6")
            .then_some(F_TELINIT),
        "systemctl" => args
            .iter()
            .any(|arg| matches!(arg.as_str(), "poweroff" | "reboot" | "halt" | "kexec"))
            .then_some(F_SYSTEMCTL_POWER),
        _ => None,
    }
}

fn detect_hardline(command: &str) -> Option<DangerFinding> {
    for variant in detection_variants(command) {
        let normalized = normalize_command_for_detection(&variant);
        let masked = mask_quoted_prose(&normalized);
        if is_fork_bomb(&masked) {
            return Some(F_FORK_BOMB);
        }
        if pipe_to_shell(&masked) {
            return Some(F_PIPE_SHELL);
        }
        let mut commands = Vec::new();
        parse_commands(&normalized, &mut commands, 0);
        if sudo_stdin(&commands) {
            return Some(F_SUDO_STDIN);
        }
        for command in &commands {
            if command.redirect_block_device {
                return Some(F_REDIRECT_BLOCK);
            }
            let Some((word, index)) = effective_command(command) else {
                continue;
            };
            let name = basename_lower(&word.content);
            let args: Vec<String> = command.words[index + 1..]
                .iter()
                .map(|word| word.content.to_ascii_lowercase())
                .collect();
            if let Some(finding) = hardline_command_match(&name, &args) {
                return Some(finding);
            }
        }
    }
    None
}

fn detect_dangerous(command: &str) -> Option<DangerFinding> {
    // The hardline floor is dangerous too: a caller that only consults the recoverable tier still
    // sees the finding.
    if let Some(finding) = detect_hardline(command) {
        return Some(finding);
    }
    for variant in detection_variants(command) {
        let normalized = normalize_command_for_detection(&variant);
        let mut commands = Vec::new();
        parse_commands(&normalized, &mut commands, 0);
        for command in &commands {
            if let Some(finding) = recoverable_command_match(command) {
                return Some(finding);
            }
        }
    }
    None
}

fn recoverable_command_match(command: &SimpleCommand) -> Option<DangerFinding> {
    if let Some(finding) = sql_finding(command) {
        return Some(finding);
    }
    if let Some(finding) = cloud_metadata_finding(command) {
        return Some(finding);
    }
    let (word, index) = effective_command(command)?;
    let name = basename_lower(&word.content);
    let args: Vec<String> = command.words[index + 1..]
        .iter()
        .map(|word| word.content.to_ascii_lowercase())
        .collect();
    match name.as_str() {
        "chmod" | "chown" => {
            let recursive = args.iter().any(|arg| {
                arg == "--recursive"
                    || arg == "-r"
                    || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('r'))
            });
            let targets_system = args
                .iter()
                .any(|arg| is_root_path(arg) || is_within_system_dir(arg));
            (recursive && targets_system).then_some(D_CHMOD)
        }
        "git" => git_finding(&args),
        "docker" => docker_finding(&args),
        "pkill" | "killall" => daemon_kill_finding(&args),
        "systemctl" => {
            let names_daemon = daemon_kill_finding(&args).is_some();
            let destructive_verb = args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "stop" | "restart" | "kill" | "disable" | "mask"
                )
            });
            (names_daemon && destructive_verb).then_some(D_DAEMON_KILL)
        }
        _ => None,
    }
}

/// SQL statements are arguments (for example, psql -c "DROP TABLE users"), so the words are joined
/// and tokenized. echo/printf arguments are prose and are skipped.
fn sql_finding(command: &SimpleCommand) -> Option<DangerFinding> {
    let (word, index) = effective_command(command)?;
    let name = basename_lower(&word.content);
    if matches!(name.as_str(), "echo" | "printf") {
        return None;
    }
    let text = command.words[index..]
        .iter()
        .map(|word| word.content.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let tokens: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
        .collect();
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i] == "drop" && matches!(tokens[i + 1], "table" | "database") {
            return Some(D_SQL_DROP);
        }
        if tokens[i] == "delete" && tokens[i + 1] == "from" {
            let has_where = tokens[i + 2..].contains(&"where");
            if !has_where {
                return Some(D_SQL_DELETE);
            }
        }
        i += 1;
    }
    None
}

/// Cloud instance-metadata endpoints serve live credentials to any local process.
fn cloud_metadata_finding(command: &SimpleCommand) -> Option<DangerFinding> {
    let (word, index) = effective_command(command)?;
    let name = basename_lower(&word.content);
    if matches!(name.as_str(), "echo" | "printf") {
        return None;
    }
    let text = command.words[index..]
        .iter()
        .map(|word| word.content.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    contains_metadata_endpoint(&text).then_some(D_METADATA)
}

fn contains_metadata_endpoint(text: &str) -> bool {
    for ip in ["169.254.169.254", "100.100.100.200", "fd00:ec2::254"] {
        if let Some(pos) = text.find(ip) {
            let before = text[..pos].chars().last();
            let after = text[pos + ip.len()..].chars().next();
            if endpoint_boundary(before) && endpoint_boundary(after) {
                return true;
            }
        }
    }
    let host = "metadata.google.internal";
    if let Some(pos) = text.find(host) {
        let before = text[..pos].chars().last();
        let after = text[pos + host.len()..].chars().next();
        if endpoint_boundary(before) && endpoint_boundary(after) {
            return true;
        }
    }
    false
}

fn endpoint_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '.' && c != ':' && c != '-')
}

fn git_finding(args: &[String]) -> Option<DangerFinding> {
    let subcommand = args.iter().find(|arg| !arg.starts_with('-'))?;
    match subcommand.as_str() {
        "push" => args
            .iter()
            .any(|arg| arg == "-f" || arg.starts_with("--force"))
            .then_some(D_GIT_PUSH),
        "reset" => args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--hard" | "--h" | "--ha" | "--har"))
            .then_some(D_GIT_RESET),
        "clean" => args
            .iter()
            .any(|arg| {
                arg.starts_with("--force")
                    || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
            })
            .then_some(D_GIT_CLEAN),
        _ => None,
    }
}

fn docker_finding(args: &[String]) -> Option<DangerFinding> {
    args.iter()
        .any(|arg| {
            arg == "--host"
                || arg.starts_with("--host=")
                || arg == "-h"
                || (arg.starts_with("-h") && arg.len() > 2)
        })
        .then_some(D_DOCKER)
}

fn daemon_kill_finding(args: &[String]) -> Option<DangerFinding> {
    const DAEMON_NAMES: &[&str] = &["silver", "silverd", "hermes"];
    args.iter()
        .any(|arg| DAEMON_NAMES.iter().any(|name| arg.contains(name)))
        .then_some(D_DAEMON_KILL)
}
