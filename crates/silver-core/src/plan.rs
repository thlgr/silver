//! Plan mode: the model explores read-only and writes a plan file; nothing else changes until the
//! user approves it through `exit_plan_mode`. The prompts are opencode's, with silver tool names.

use crate::error::CoreResult;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use silver_protocol::{EventPayload, RiskLevel};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const EXIT_PLAN_MODE: &str = "exit_plan_mode";
pub const ASK_USER_QUESTION: &str = "ask_user_question";

/// Where a session stands with plan mode when a run starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlanMode {
    #[default]
    Off,
    /// Turned on since the last run, so a plan left from an earlier planning session gets
    /// the re-entry notice.
    Entered,
    Active,
    /// Left since the last run, so the model is told it may act again.
    Exited,
}

impl PlanMode {
    pub fn is_on(self) -> bool {
        matches!(self, PlanMode::Entered | PlanMode::Active)
    }
}

/// A run's plan mode and the session's plan file.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Where the session stood when the run started.
    pub mode: PlanMode,
    /// The session's plan file, in the daemon's data directory; empty when there is none.
    pub file: PathBuf,
    /// Whether plan mode holds right now: an approved plan ends it mid-run.
    on: Arc<AtomicBool>,
}

impl Plan {
    pub fn new(mode: PlanMode, file: PathBuf) -> Self {
        Self {
            mode,
            file,
            on: Arc::new(AtomicBool::new(mode.is_on())),
        }
    }

    /// Plan mode's refusals without plan mode's tools: a subagent of a planning run explores
    /// read-only, and can never put a plan to the user or leave the parent in plan mode.
    pub fn read_only(file: PathBuf) -> Self {
        Self {
            mode: PlanMode::Off,
            file,
            on: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Whether a tool's path argument names the plan file.
    pub fn is_file(&self, requested: &str) -> bool {
        !self.file.as_os_str().is_empty() && Path::new(requested) == self.file
    }

    /// Whether a call writes the plan file and nothing else. Which tool does it is the model's
    /// choice: a file tool naming that path, or a shell command whose only change is a `>`
    /// (or `tee`) to that same path, parsed out of the command line.
    pub fn is_written_by(&self, name: &str, args: &Value) -> bool {
        if matches!(name, "write_file" | "patch") {
            return args
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| self.is_file(path));
        }
        crate::tool::is_shell_tool(name)
            && self
                .shell_writes_plan(&crate::tool::command_text(args))
                .is_ok()
    }

    /// Ok when every `>` target, `tee` operand and file edited in place is the plan file and
    /// nothing else in the line changes; otherwise the first other file written, so a mistyped
    /// session id or a `$VAR` is refused by name.
    fn shell_writes_plan(&self, command: &str) -> Result<(), Option<String>> {
        let plan_file = self.file.to_string_lossy().into_owned();
        if plan_file.is_empty() {
            return Err(None);
        }
        let (mut writes_plan, mut changes_else) = (false, false);
        for program in crate::tool::shell_programs(command) {
            let args: Vec<&str> = program.args.iter().map(String::as_str).collect();
            let lower: Vec<String> = args.iter().map(|arg| arg.to_ascii_lowercase()).collect();
            let lowered: Vec<&str> = lower.iter().map(String::as_str).collect();
            let mut written: Vec<&str> = program.redirects.iter().map(String::as_str).collect();
            let operands: Vec<&str> = args
                .iter()
                .copied()
                .filter(|arg| !arg.starts_with('-'))
                .collect();
            // The daemon makes the plan's folder before the run, so a `mkdir -p` of it is a no-op.
            let makes_plan_dir = program.name == "mkdir"
                && operands
                    .iter()
                    .all(|dir| !dir.is_empty() && self.file.starts_with(dir));
            if program.name == "tee" {
                written.extend(operands);
            } else if edits_in_place(&program.name, &lowered) {
                // The first operand is the script, the rest are the files rewritten.
                written.extend(operands.iter().skip(1).copied());
            }
            if let Some(other) = written.iter().find(|target| **target != plan_file) {
                return Err(Some((*other).to_string()));
            }
            if !written.is_empty() {
                writes_plan = true;
            } else if changes(&program.name, &lowered) && !makes_plan_dir {
                changes_else = true;
            }
        }
        if writes_plan && !changes_else {
            Ok(())
        } else {
            Err(None)
        }
    }

    /// Why plan mode refuses a call, or None when it may run: reads, silver's own notes
    /// (todo_list, memory), the plan file itself, and shell commands that change nothing.
    pub fn refusal(&self, name: &str, args: &Value, risk: RiskLevel) -> Option<String> {
        if !self.is_on() || matches!(risk, RiskLevel::Read | RiskLevel::Memory) {
            return None;
        }
        let (what, hint) = if crate::tool::is_shell_tool(name) {
            let command = crate::tool::command_text(args);
            match self.shell_writes_plan(&command) {
                Ok(()) => return None,
                Err(Some(other)) => (
                    format!("writing `{other}`"),
                    "That is not the plan file, the only file you may write: copy its path exactly.",
                ),
                Err(None) => (
                    shell_change(&command)?,
                    "Commands that only read, such as ls, cat, grep, find and git log, still run.",
                ),
            }
        } else if self.is_written_by(name, args) {
            return None;
        } else {
            (name.to_string(), "Explore with read-only tools.")
        };
        Some(format!(
            "plan mode is active, so {what} can't run until the user approves your plan. {hint} Write the plan to {} (with a file tool, or `cat > {} <<'EOF'` in bash).",
            self.file.display(),
            self.file.display()
        ))
    }

    /// The system reminders the run's user message carries, with a label for the client.
    pub fn notice(&self) -> Option<(&'static str, String)> {
        let path = self.file.display().to_string();
        let exists = self.file.is_file();
        let write_only = crate::sandbox::write_boundary(self);
        let notice = match self.mode {
            PlanMode::Off => return None,
            PlanMode::Exited => {
                let reference = if exists {
                    format!(" The plan file is located at {path} if you need to reference it.")
                } else {
                    String::new()
                };
                return Some((
                    "Exited plan mode",
                    reminder(&format!(
                        "## Exited Plan Mode\n\nYou have exited plan mode. You can now make edits, run tools, and take actions.{reference}"
                    )),
                ));
            }
            PlanMode::Entered if exists => format!(
                "{}\n{}",
                reminder(&reentry_instructions(&path)),
                reminder(&instructions(&path, exists, write_only))
            ),
            _ => reminder(&instructions(&path, exists, write_only)),
        };
        Some(("Plan mode", notice))
    }

    /// The plan to put to the user, or why exit_plan_mode cannot be used now.
    pub fn for_review(&self) -> Result<String, String> {
        if !self.is_on() {
            return Err("You are not in plan mode. This tool is only for exiting plan mode after writing a plan.".into());
        }
        std::fs::read_to_string(&self.file)
            .ok()
            .filter(|plan| !plan.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "No plan file exists yet. Create the plan at {}.",
                    self.file.display()
                )
            })
    }
}

/// Programs that always change files or state.
const CHANGING_PROGRAMS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "ln", "mkdir", "touch", "tee", "truncate", "shred", "dd", "chmod",
    "chown", "chgrp", "install", "rsync", "unlink", "patch", "kill", "pkill", "killall", "wget",
];

/// The part of a shell command that changes files or state, or None when it only looks. A
/// blacklist: a program it does not know counts as looking, so exploring stays open.
pub fn shell_change(command: &str) -> Option<String> {
    crate::tool::shell_programs(command)
        .into_iter()
        .find_map(|program| {
            if !program.redirects.is_empty() {
                return Some("writing a file with `>`".into());
            }
            // Matched lowercased (-D deletes a branch like -d), quoted as written.
            let lower: Vec<String> = program
                .args
                .iter()
                .map(|arg| arg.to_ascii_lowercase())
                .collect();
            let args: Vec<&str> = lower.iter().map(String::as_str).collect();
            changes(&program.name, &args).then(|| {
                let part = format!("{} {}", program.name, program.args.join(" "));
                let cut: String = part.chars().take(60).collect();
                format!(
                    "`{}{}`",
                    cut.trim(),
                    if cut.len() < part.len() { "…" } else { "" }
                )
            })
        })
}

/// Whether a `sed` or `perl` rewrites its operands instead of printing: the `-i` flag, alone
/// (`-i.bak`) or in a cluster (`-pi`).
fn edits_in_place(name: &str, args: &[&str]) -> bool {
    matches!(name, "sed" | "perl")
        && args
            .iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('i'))
}

fn changes(name: &str, args: &[&str]) -> bool {
    if CHANGING_PROGRAMS.contains(&name) {
        return true;
    }
    // Formatters and linters rewrite files with these.
    if args
        .iter()
        .any(|arg| matches!(*arg, "--write" | "--fix") || arg.starts_with("--in-place"))
    {
        return true;
    }
    let first = args.iter().find(|arg| !arg.starts_with('-')).copied();
    match name {
        "sed" | "perl" => edits_in_place(name, args),
        "find" => args
            .iter()
            .any(|arg| *arg == "-delete" || CHANGING_PROGRAMS.contains(arg)),
        "xargs" => args.iter().any(|arg| CHANGING_PROGRAMS.contains(arg)),
        "curl" => args.iter().any(|arg| {
            matches!(*arg, "-o" | "--output" | "--remote-name") || arg.starts_with("--output=")
        }),
        "git" => git_changes(args),
        "npm" | "pnpm" | "yarn" | "bun" => first.is_some_and(|sub| {
            matches!(
                sub,
                "install"
                    | "i"
                    | "add"
                    | "remove"
                    | "rm"
                    | "uninstall"
                    | "update"
                    | "upgrade"
                    | "ci"
                    | "link"
                    | "unlink"
                    | "publish"
                    | "dedupe"
                    | "prune"
            )
        }),
        "pip" | "pip3" | "uv" | "poetry" | "pipx" | "gem" | "brew" => args.iter().any(|arg| {
            matches!(
                *arg,
                "install" | "uninstall" | "add" | "remove" | "sync" | "upgrade" | "update"
            )
        }),
        "cargo" => first.is_some_and(|sub| {
            matches!(
                sub,
                "add"
                    | "remove"
                    | "rm"
                    | "install"
                    | "uninstall"
                    | "update"
                    | "publish"
                    | "fmt"
                    | "fix"
                    | "new"
                    | "init"
            )
        }),
        "go" => first.is_some_and(|sub| matches!(sub, "get" | "install" | "fmt")),
        _ => false,
    }
}

/// Whether a git command changes the repository. Looking stays open: status, log, diff,
/// show, blame, grep, and listing branches, tags, stashes and worktrees.
fn git_changes(args: &[&str]) -> bool {
    // Global options come first; -C and -c take a value.
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        i += if matches!(args[i], "-c") { 2 } else { 1 };
    }
    let Some(sub) = args.get(i) else {
        return false;
    };
    let rest = &args[i + 1..];
    match *sub {
        "add" | "am" | "apply" | "checkout" | "cherry-pick" | "clean" | "clone" | "commit"
        | "gc" | "init" | "merge" | "mv" | "pull" | "push" | "rebase" | "reset" | "restore"
        | "revert" | "rm" | "switch" => true,
        // A name creates one; the delete, move, copy and force flags change one.
        "branch" | "tag" => rest.iter().any(|arg| {
            matches!(
                *arg,
                "-d" | "-m" | "-c" | "-f" | "--delete" | "--move" | "--copy" | "--force"
            ) || (!arg.starts_with('-') && !rest.contains(&"-l") && !rest.contains(&"--list"))
        }),
        "stash" | "worktree" | "submodule" => {
            !matches!(rest.first(), Some(&("list" | "show" | "status")))
        }
        _ => false,
    }
}

fn reminder(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// How the plan file must read. The model re-checks it before asking for approval.
const PLAN_STYLE: &str = "## Plan Style
- English, always, even when the user writes in another language.
- Terse: no preamble, no restating the request, no narration. One line per change.
- No pronouns (it, this, that, they, we): name the file, function, type or flag the line is about. Every English line needs that named subject.
- Every line points at the code it touches: path:line, symbol name, and the command that runs or tests it.";

/// The plan_mode instructions (the iterative planning workflow).
fn instructions(path: &str, exists: bool, write_only: bool) -> String {
    let read_only = if write_only {
        " In plan mode the rest of the filesystem is read-only, so a build or a test that writes cannot run."
    } else {
        ""
    };
    let file_info = if exists {
        format!("A plan file already exists at {path}. Read it, then update it in place.")
    } else {
        format!("No plan file exists yet. Create it at {path}.")
    };
    format!(
        "Plan mode is active. The user indicated that they do not want you to execute yet — you MUST NOT make any edits (with the exception of the plan file mentioned below), run any non-readonly tools, or otherwise make any changes to the system.

## Plan File Info:
{file_info} That path is the only file you may write, with whichever tool you have: `write_file` or `patch` on it, or `cat > {path} <<'EOF'` or `sed -i` or a script that writes it, in bash. They all work the same; choose the one your tool set gives you.{read_only}

{PLAN_STYLE}

## Iterative Planning Workflow

You are pair-planning with the user. Explore the code to build context, ask the user questions when you hit decisions you can't make alone, and write your findings into the plan file as you go. The plan file is the ONLY file you may edit — it starts as a rough skeleton and gradually becomes the final plan.

### The Loop
1. Explore — Use read-only tools to read code. Look for existing functions, utilities, and patterns to reuse.
2. Update the plan file — After each discovery, immediately capture what you learned.
3. Ask the user — When you hit an ambiguity or decision you can't resolve from code alone, use ask_user_question. Then go back to step 1.

### First Turn
Start by quickly scanning a few key files to form an initial understanding of the task scope. Then write a skeleton plan (headers and rough notes) and ask the user your first round of questions.

### Ending Your Turn
Your turn should only end by either:
- Using ask_user_question to gather more information
- Calling exit_plan_mode when the plan is ready for approval"
    )
}

/// The plan_mode_reentry instructions.
fn reentry_instructions(path: &str) -> String {
    format!(
        "## Re-entering Plan Mode

You are returning to plan mode after having previously exited it. A plan file exists at {path} from your previous planning session.

Before proceeding with any new planning, you should:
1. Read the existing plan file to understand what was previously planned
2. Evaluate the user's current request against that plan
3. Decide how to proceed:
   - Different task: If the user's request is for a different task—even if it's similar or related—start fresh by overwriting the existing plan
   - Same task, continuing: If this is explicitly a continuation or refinement of the exact same task, modify the existing plan while cleaning up outdated or irrelevant sections
4. Continue on with the plan process and most importantly you should always edit the plan file one way or the other before calling exit_plan_mode

Treat this as a fresh planning session. Do not assume the existing plan is relevant without evaluating it first."
    )
}

/// The question of an ask_user_question call, when it has one.
pub fn question(args: &Value) -> Option<&str> {
    args.get("question")
        .and_then(Value::as_str)
        .filter(|question| !question.trim().is_empty())
}

/// Turns a question's options into the strings its schema asks for. Models trained on richer
/// question tools send `{label, description}` objects, which a client would show as `[object Object]`.
pub fn options_as_text(args: &mut Value) {
    let Some(options) = args.get_mut("options").and_then(Value::as_array_mut) else {
        return;
    };
    for option in options.iter_mut().filter(|option| !option.is_string()) {
        let text = match &*option {
            Value::Object(fields) => fields
                .get("label")
                .into_iter()
                .chain(
                    fields
                        .iter()
                        .filter(|(key, _)| *key != "label")
                        .map(|(_, value)| value),
                )
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" — "),
            other => other.to_string(),
        };
        *option = Value::String(text);
    }
}

/// The result of an ask_user_question call the user answered.
pub fn answered(args: &Value, answer: &str) -> String {
    let question = question(args).unwrap_or_default();
    format!("User has answered your questions: \"{question}\"=\"{answer}\". You can now continue with the user's answers in mind.")
}

/// The tools only a plan-mode run has: none when the run starts outside plan mode, so they
/// cost nothing in any other prompt.
pub fn tools(on: bool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    if on {
        registry.register(Arc::new(AskUserQuestion));
        registry.register(Arc::new(ExitPlanMode));
    }
    registry
}

struct ExitPlanMode;

#[async_trait::async_trait]
impl Tool for ExitPlanMode {
    fn name(&self) -> &'static str {
        EXIT_PLAN_MODE
    }

    fn description(&self) -> &'static str {
        "Use this tool when you are in plan mode and have finished writing your plan to the plan file and are ready for user approval.

## How This Tool Works
- You should have already written your plan to the plan file specified in the plan mode system message
- This tool does NOT take the plan content as a parameter — it will read the plan from the file you wrote
- This tool simply signals that you're done planning and ready for the user to review and approve
- The user will see the contents of your plan file when they review it

## When to Use This Tool
IMPORTANT: Only use this tool when the task requires planning the implementation steps of a task that requires writing code. For research tasks where you're gathering information, searching files, reading files or in general trying to understand the codebase — do NOT use this tool.

## Before Using This Tool
Ensure your plan is complete and unambiguous:
- Re-read the plan file and fix every line that breaks the Plan Style section of the plan mode message: English only, terse, no pronouns, every line naming a path and a symbol
- If you have unresolved questions about requirements or approach, use ask_user_question first
- Once your plan is finalized, use THIS tool to request approval

Important: Do NOT use ask_user_question to ask \"Is this plan okay?\" or \"Should I proceed?\" — that's exactly what THIS tool does. exit_plan_mode inherently requests user approval of your plan."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    /// Runs once the user approved the plan; the turn loop asks them first.
    async fn execute(&self, ctx: &ToolContext<'_>, _args: Value) -> CoreResult<ToolOutcome> {
        let plan = &ctx.run.plan;
        let text = match plan.for_review() {
            Ok(text) => text,
            Err(reason) => return Ok(ToolOutcome::error(reason)),
        };
        plan.on.store(false, Ordering::Relaxed);
        ctx.events.emit(EventPayload::PlanModeExited);
        Ok(ToolOutcome::ok(format!(
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable

Your plan has been saved to: {}
You can refer back to it if needed during implementation.

## Approved Plan:
{text}",
            plan.file.display()
        ))
        .with_summary("Plan approved"))
    }
}

struct AskUserQuestion;

#[async_trait::async_trait]
impl Tool for AskUserQuestion {
    fn name(&self) -> &'static str {
        ASK_USER_QUESTION
    }

    fn description(&self) -> &'static str {
        "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- Users will always be able to select \"Other\" to provide custom text input
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label

Plan mode note: In plan mode, use this tool to clarify requirements or choose between approaches BEFORE finalizing your plan. Do NOT use this tool to ask \"Is my plan ready?\" or \"Should I proceed?\" - use exit_plan_mode for plan approval."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to ask the user." },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "2-4 short answers the user can pick from."
                }
            },
            "required": ["question"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    /// The turn loop answers this call with the user's reply; this runs only when there was
    /// nothing to ask or nobody to answer.
    async fn execute(&self, _ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        Ok(ToolOutcome::error(match question(&args) {
            None => "ask_user_question needs a 'question' string.",
            Some(_) => "Nobody answered the question. Use your best judgment and continue.",
        }))
    }
}
