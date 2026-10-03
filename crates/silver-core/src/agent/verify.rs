//! Verify-on-stop: recognises shell commands that verify a change (test, lint, build, typecheck,
//! format) and tracks whether each edit was followed by a fresh passing one.

use serde_json::Value;

/// Category of verification command a shell invocation performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyKind {
    Test,
    Lint,
    Build,
    Typecheck,
    Format,
}

impl VerifyKind {
    /// Short lowercase label for logs and nudges.
    pub fn label(self) -> &'static str {
        match self {
            VerifyKind::Test => "test",
            VerifyKind::Lint => "lint",
            VerifyKind::Build => "build",
            VerifyKind::Typecheck => "typecheck",
            VerifyKind::Format => "format",
        }
    }
}

/// Keyword tokens that classify a command, checked in this order.
const VERIFY_KEYWORDS: [(VerifyKind, &[&str]); 5] = [
    (
        VerifyKind::Test,
        &[
            "test", "pytest", "jest", "vitest", "mocha", "rspec", "phpunit", "unittest",
        ],
    ),
    (
        VerifyKind::Lint,
        &[
            "lint",
            "clippy",
            "eslint",
            "ruff",
            "pylint",
            "flake8",
            "rubocop",
            "stylelint",
            "shellcheck",
        ],
    ),
    (
        VerifyKind::Build,
        &[
            "build", "compile", "webpack", "rollup", "vite", "cmake", "make", "msbuild",
        ],
    ),
    (
        VerifyKind::Typecheck,
        &[
            "typecheck",
            "type-check",
            "type_check",
            "check-types",
            "mypy",
            "pyright",
            "tsc",
            "flow",
        ],
    ),
    (
        VerifyKind::Format,
        &[
            "format",
            "fmt",
            "prettier",
            "rustfmt",
            "black",
            "gofmt",
            "gofumpt",
            "clang-format",
        ],
    ),
];

/// Number of changed paths listed in a nudge before a summary ellipsis.
const MAX_PATHS_IN_NUDGE: usize = 8;

/// Classify a bash or run_command call as a verification command, or None.
pub fn classify_verify_command(tool_name: &str, args: &Value) -> Option<VerifyKind> {
    let command = match tool_name {
        "bash" => args.get("command").and_then(Value::as_str)?.to_string(),
        "run_command" => {
            let argv = args.get("argv")?.as_array()?;
            argv.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => return None,
    };
    if command.trim().is_empty() {
        return None;
    }
    let tokens = tokenize(&command);
    VERIFY_KEYWORDS.iter().find_map(|(kind, keywords)| {
        keywords
            .iter()
            .any(|keyword| tokens.iter().any(|token| token == keyword))
            .then_some(*kind)
    })
}

/// Split a shell command into lowercase alphanumeric/hyphen/underscore tokens.
fn tokenize(command: &str) -> Vec<String> {
    command
        .to_ascii_lowercase()
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// Per-run verification evidence for one agent turn.
#[derive(Clone, Debug, Default)]
pub struct VerificationTracker {
    /// A write_file or patch succeeded at least once.
    edited: bool,
    /// A verification command passed after the most recent edit.
    verified_after_edit: bool,
    /// Bounded follow-up nudges already injected.
    nudges: u32,
    /// Paths touched by successful edits, in first-seen order.
    changed_paths: Vec<String>,
    /// A nudge was sent and the model has not called a tool since. A text-only answer to
    /// a nudge explains why it cannot verify; nudging again would only repeat it.
    nudge_unanswered: bool,
}

impl VerificationTracker {
    /// Build an empty tracker for one run.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the outcome of one executed tool call.
    pub fn record_tool_result(&mut self, tool_name: &str, args: &Value, is_error: bool) {
        self.nudge_unanswered = false;
        match tool_name {
            "write_file" | "patch" => {
                // Prose has nothing to test, lint or build.
                if !is_error && edited_path(args).is_none_or(|path| !is_prose(&path)) {
                    self.edited = true;
                    self.verified_after_edit = false;
                    if let Some(path) = edited_path(args) {
                        if !self.changed_paths.iter().any(|known| known == &path) {
                            self.changed_paths.push(path);
                        }
                    }
                }
            }
            "bash" | "run_command"
                if !is_error
                    && self.edited
                    && (classify_verify_command(tool_name, args).is_some()
                        || self.runs_a_changed_file(tool_name, args)) =>
            {
                self.verified_after_edit = true;
            }
            _ => {}
        }
    }

    /// Whether the model should be nudged to verify before finishing.
    pub fn needs_verification(&self) -> bool {
        self.edited && !self.verified_after_edit && !self.nudge_unanswered
    }

    /// Whether the command runs a file this run edited (`python3 calc.py`, `./build.sh`):
    /// running the changed program is the verification a script without tests gets.
    /// Commands that only read files do not count.
    fn runs_a_changed_file(&self, tool_name: &str, args: &Value) -> bool {
        const READERS: [&str; 12] = [
            "cat", "head", "tail", "less", "more", "grep", "rg", "wc", "ls", "stat", "diff", "git",
        ];
        let command = match tool_name {
            "bash" => args
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => args.get("argv").and_then(Value::as_array).map(|argv| {
                argv.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        }
        .unwrap_or_default();
        let words: Vec<&str> = command
            .split_whitespace()
            .map(|word| word.trim_matches(|c: char| c == '"' || c == '\''))
            .map(|word| word.rsplit('/').next().unwrap_or(word))
            .collect();
        if words.first().is_none_or(|first| READERS.contains(first)) {
            return false;
        }
        // The stem also counts, so `python3 -c "from stats import mean"` runs stats.py.
        let tokens = tokenize(&command);
        self.changed_paths.iter().any(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
            words.contains(&name) || tokens.contains(&stem)
        })
    }

    /// Number of nudges already injected this run.
    pub fn nudges_sent(&self) -> u32 {
        self.nudges
    }

    /// Count one injected nudge.
    pub fn record_nudge(&mut self) {
        self.nudge_unanswered = true;
        self.nudges += 1;
    }

    /// Paths touched by successful edits.
    pub fn changed_paths(&self) -> &[String] {
        &self.changed_paths
    }

    /// Build the bounded follow-up message that asks for verification.
    pub fn build_nudge(&self) -> String {
        let mut paths = String::new();
        for path in self.changed_paths.iter().take(MAX_PATHS_IN_NUDGE) {
            paths.push_str("\n- ");
            paths.push_str(path);
        }
        if self.changed_paths.len() > MAX_PATHS_IN_NUDGE {
            paths.push_str(&format!(
                "\n- ... and {} more",
                self.changed_paths.len() - MAX_PATHS_IN_NUDGE
            ));
        }
        format!(
            "[System: You edited files in this turn, but no passing verification command has \
             run since the last edit. Run the relevant test, lint, typecheck, build or format \
             command now, read any failure, repair the code, and summarize what passed. If \
             verification is genuinely not possible, explain the concrete blocker instead of \
             claiming the work is verified.{paths}]"
        )
    }
}

/// The path argument of an edit tool, when present.
fn is_prose(path: &str) -> bool {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    matches!(extension.as_deref(), Some("md" | "txt" | "rst"))
}

fn edited_path(args: &Value) -> Option<String> {
    args.get("path")
        .or_else(|| args.get("file_path"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
