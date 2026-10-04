//! System prompt assembly in three tiers, joined stable (identity, guidance) -> context (project,
//! workspace) -> volatile (skills, memory, timestamp), so a prefix cache reuses the scaffold.

use crate::context::gate_context_text;
use crate::services::SkillSummary;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// Identity plus tool and model guidance. Always the first tier.
pub const SILVER_HELP_GUIDANCE: &str = "For help with agent itself: config, setup, usage, extending, troubleshooting: or to understand your own tools and capabilities, read the project docs and source before guessing.";

/// S3. Universal finish-the-job guidance.
pub const TASK_COMPLETION_GUIDANCE: &str = r#"# Finish the job

Build/run/verify means a working artifact backed by real tool output, not a description. Do not stop at a stub, a plan, or a single command: exercise the code, produce the result, then report what real execution returned.
If a tool, install, or network failure blocks the real path, say so and try an alternative (different package manager, different approach, ask the user). Never fabricate output (made-up data, invented file contents, synthesised responses) for results you couldn't produce. An honest blocker beats an invented result."#;

/// S4. Universal parallel-tool-call guidance.
pub const PARALLEL_TOOL_CALL_GUIDANCE: &str = r#"# Parallel tool calls

Need several independent pieces of info? Request them together in one response. Batch independent reads, searches, fetches, and read-only commands into the same turn: the runtime runs them concurrently and you avoid resending the conversation each round-trip.
Serialize only when a later call depends on an earlier result (read a file before patching it). When in doubt and the calls are independent, batch them."#;

/// S5. Session-history recall guidance.
pub const SESSION_SEARCH_GUIDANCE: &str = "When the user references a past conversation or relevant cross-session context likely exists, use session_search before asking them to repeat.";

/// S5. Image and document guidance. Without it a small model reads a screenshot's filename and
/// answers from imagination, or greps a PDF's bytes instead of extracting them.
pub const MEDIA_GUIDANCE: &str = "You can see a picture and search a document. Use view_image with a path and a question when the answer is in an image (a screenshot, a diagram, a photo); it is the only way to look at one. Use search_documents for a document you were not given the text of: pass its path to index it, or search what is already indexed. read_file extracts a PDF's text, so a document can be read directly when you know the page. A file the user attached is already in the workspace: open the path in the prompt, do not ask where it is.";

/// S5. Memory guidance. A 4B model reads hedged advice about what does not belong in memory
/// as "don't save", and often says it will remember without calling the tool.
pub const MEMORY_GUIDANCE: &str = "You have persistent memory: its entries load into every future session. Save with the memory tool before you reply whenever the user asks you to remember something, tells you about themselves (name, language, preferences, corrections to how you work), or states a lasting fact about this project (how to run it, conventions). Saying you will remember is not saving. When a saved fact changes, replace it; when the user asks you to forget it, remove it. Don't save details of the current task. Write short facts, not orders: 'User prefers <language>', not 'Always answer in <language>'.";

/// Sudo guard when the daemon runs without elevated privileges.
pub const NON_ROOT_SUDO_TIP: &str = "You have no sudo, but the user does. Never install anything (pacman, apt, dnf, gem, pip or similar), not even to try, and no workarounds such as downloading binaries. If something is missing, stop and give the user the exact commands to run themselves, with sudo where needed, for this OS:";

/// S5. Skill-writing guidance, including the compaction safety rule.
pub const SKILLS_GUIDANCE: &str = r#"Record non-trivial workflows with skill_manage for reuse.

## Skill Safety Rule
A skill placeholder with `[SKILL_PRUNED]` lost its content to compaction: reload it with skill_view(name='...') before acting on anything that depends on it. After reloading, ignore any remaining `[SKILL_PRUNED]` markers for that skill; they are historical."#;

/// S7. Tool-use enforcement for models that answer in prose without acting.
pub const TOOL_USE_ENFORCEMENT_GUIDANCE: &str = r#"# Tool-use enforcement
Use tools to act: never just describe or plan without doing. When you say you will do something ('run the tests', 'check the file', 'create the project'), make the tool call in the same response. Never end a turn with a promise of future action: execute now. Keep working until the task is complete, not until you've summarised a plan. Every response either makes progress via tool calls or delivers the final result; intention-only responses are not acceptable."#;

/// S8. Gemini/Gemma operational directives.
pub const GOOGLE_MODEL_OPERATIONAL_GUIDANCE: &str = r#"# Google model operational directives
- Absolute paths: build absolute paths for all file ops (root + relative).
- Verify first: read_file/search_files before changing; never guess contents.
- Dependencies: check the manifest (package.json, requirements.txt, Cargo.toml) before importing; never assume a library is present.
- Concise: a few sentences, actions over narration.
- Non-interactive: pass -y/--yes/--non-interactive so CLIs don't hang.
- Keep going: work autonomously until resolved; execute, don't just plan."#;

/// S9. Execution discipline for models that stop early or answer from memory.
pub const OPENAI_MODEL_EXECUTION_GUIDANCE: &str = r#"# Execution discipline
<tool_persistence>
Use tools whenever they improve correctness or grounding. Don't stop early when another call would materially help. On empty/partial/narrow results, retry with a broader query before concluding. Keep going until the task is complete AND verified.
</tool_persistence>

<mandatory_tool_use>
NEVER answer from memory: always use a tool for: arithmetic/math (bash or execute_code); hashes/encodings/checksums (bash, e.g. sha256sum, base64); current time/date/timezone (bash date); system state: OS, CPU, memory, disk, ports, processes (bash); file contents/sizes/line counts (read_file, search_files, bash); git history/branches/diffs (bash); current facts: weather, news, versions (a permitted retrieval/search tool).
Your memory and user profile describe the USER, not the system you run on.
</mandatory_tool_use>

<act_dont_ask>
On an obvious default interpretation, act instead of asking. 'Is port 443 open?' → check THIS machine. 'What OS am I running?' → the live system, not the user profile. 'What time is it?' → run `date`. Also: resolve prerequisite lookups first: don't skip them because the final action seems obvious. Ask only when the ambiguity changes which tool you'd call.
</act_dont_ask>

<verification>
Before finalizing: correctness (every stated requirement met?); grounding (claims backed by tool output?); formatting (matches requested schema?); safety (confirm scope before side effects). 'Done' means every named acceptance criterion is verified, not a plausible subset: completing your plan is not itself the answer; the requested output must appear in your response.
</verification>

<external_state_verification>
After a state-changing write to an external system (API call, message post, record update), read back the exact target before claiming success: a successful call is not a successful task; don't re-verify internal file edits a tool already confirmed. Declared totals (total, reply_count, has_more, '...N more') are hard assertions: if your enumerated count disagrees, re-fetch, never 'go with what I have'. Set write-payload fields explicitly, not by provider default.
</external_state_verification>

<literal_preservation>
Preserve identifiers, commands, and values exactly: never 'repair' a token that fails a stated format. A successful lookup does not validate a malformed token; validate format first, then look up.
</literal_preservation>

<missing_context>
If required context is missing, don't guess: use a lookup tool (search_files, read_file, or a retrieval/search tool). Ask only when tools can't retrieve it. If you must proceed incomplete, label assumptions explicitly.
</missing_context>"#;

/// S12. Coding operating brief.
pub const CODING_AGENT_GUIDANCE: &str = r#"You are a coding agent pairing with the user in their codebase. Work like a careful senior engineer.

Gather context first:
- Read relevant files with `read_file` and locate code with `search_files` before changing anything. Trace a symbol to its definition and uses rather than guessing its shape.
- Never invent files, symbols, APIs, or imports. Not seen in the repo? Go look. Don't assume a library is available: check the manifest (pyproject.toml / package.json / Cargo.toml / go.mod) and how neighbouring files import it.

Change through tools, not chat:
- Edit with `patch`: `path` plus the exact `old_string` copied verbatim from `read_file` (enough surrounding lines to be unique) and its `new_string`. Use `write_file` only for new files or to replace a whole small file. Do NOT print code blocks as a substitute for editing: apply the change, then summarise. Show code only when the user asks.
- Match the project's existing style; AGENTS.md / CLAUDE.md / .cursorrules in context win over your defaults. Touch only what the task needs: no drive-by refactors, renames, or reformatting: and add any imports/deps your code requires.
- If `patch` reports no match, re-read the file and copy the current text exactly into `old_string`: never resend the same old_string. If the same region misses twice, widen `old_string` to the whole enclosing function; do not rewrite a large file with `write_file` to get around a miss.

Verify and stop:
- Use `bash` for git, builds, tests, and inspection. Run the relevant tests/linter/build and confirm they pass before claiming done.
- Fix root causes, not symptoms: check sibling call paths for the same flaw and fix the class, not just the reported site.
- On linter/type errors, stop after about three attempts on the same file and ask the user rather than looping.
- Track multi-step work with `todo_list`. Reference code as `path:line` instead of pasting whole files.

Respect the repo: don't commit, push, or rewrite history unless asked; never read, print, or commit secrets: leave `.env` and credential files alone unless the user asks. The Workspace block below is a session-start snapshot: re-run `git status`/`git branch` before relying on it. Be concise: lead with the change or answer, not a preamble."#;

/// Sentence in the coding brief that assumes a multi-step todo tool.
pub const TODO_SENTENCE: &str = "- Track multi-step work with `todo_list`. Reference code as `path:line` instead of pasting whole files.";
/// Coding-brief sentence when no todo tool is registered.
pub const NO_TODO_SENTENCE: &str =
    "- Reference code as `path:line` instead of pasting whole files.";

/// Whole lines copied verbatim from `CODING_AGENT_GUIDANCE` (the `TODO_SENTENCE` pattern).
pub const READ_LINE: &str = "- Read relevant files with `read_file` and locate code with `search_files` before changing anything. Trace a symbol to its definition and uses rather than guessing its shape.";
pub const EDIT_LINE: &str = "- Edit with `patch`: `path` plus the exact `old_string` copied verbatim from `read_file` (enough surrounding lines to be unique) and its `new_string`. Use `write_file` only for new files or to replace a whole small file. Do NOT print code blocks as a substitute for editing: apply the change, then summarise. Show code only when the user asks.";
pub const PATCH_MISS_LINE: &str = "- If `patch` reports no match, re-read the file and copy the current text exactly into `old_string`: never resend the same old_string. If the same region misses twice, widen `old_string` to the whole enclosing function; do not rewrite a large file with `write_file` to get around a miss.";
pub const BASH_LINE: &str = "- Use `bash` for git, builds, tests, and inspection. Run the relevant tests/linter/build and confirm they pass before claiming done.";

/// Shell-only replacements, short and literal for 4B models.
/// `read_file` without `search_files`: keep the reading rule without naming the missing tool.
pub const READ_FILE_LINE: &str = "- Read relevant files with `read_file` before changing anything. Trace a symbol to its definition and uses rather than guessing its shape.";
pub const SHELL_READ_LINE: &str = "- Read files with `cat` or `sed -n 'START,ENDp' FILE` and locate code with `rg -n PATTERN` (or `grep -rn`) through `bash` before changing anything. Trace a symbol to its definition and uses rather than guessing its shape.";
pub const SHELL_EDIT_LINE: &str = "- Edit files through `bash`: write a new file with a quoted heredoc (`cat > FILE <<'EOF'`), and change an existing file by replacing one exact snippet, for example with a short `python3` script that fails unless the old text occurs exactly once. Re-read the changed lines to confirm. Do NOT print code blocks as a substitute for editing: apply the change, then summarise. Show code only when the user asks.";
pub const NO_FILE_TOOLS_NOTE: &str = "This chat's preset gives you no file or shell tools, so you cannot see or change files or run commands. If the user asks about files or code on disk, say so and suggest a preset with file tools; never guess their contents.";

/// Models that receive tool-use enforcement under the automatic gate.
pub const TOOL_USE_ENFORCEMENT_MODELS: &[&str] = &[
    "gpt", "codex", "gemini", "gemma", "grok", "glm", "qwen", "deepseek", "muse",
];

/// Models that receive execution discipline under the automatic gate.
pub const EXECUTION_GUIDANCE_MODELS: &[&str] = &[
    "gpt", "codex", "grok", "deepseek", "kimi", "qwen", "glm", "minimax", "mimo", "mistral", "muse",
];

/// First line of the runtime environment block.
pub const RUNTIME_ENVIRONMENT_HEADING: &str = "# Runtime environment";

/// Header of the built-in memory snapshot.
pub const MEMORY_BLOCK_HEADER: &str = "MEMORY (your personal notes)";
/// Header of the user-profile snapshot.
pub const USER_BLOCK_HEADER: &str = "USER PROFILE (who the user is)";
/// Character budget reported for the memory snapshot.
pub const MEMORY_CHAR_LIMIT: usize = 2_200;
/// Character budget reported for the user-profile snapshot.
pub const USER_CHAR_LIMIT: usize = 1_375;
/// Separator drawn above and below each memory snapshot header.
const MEMORY_SEPARATOR: char = '\u{2550}';

/// Gate for a model-conditioned guidance block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Match the model id against the block's default substring list.
    Auto,
    /// Always include the block.
    On,
    /// Never include the block.
    Off,
}

/// Everything the assembler reads. The caller owns the context, memory and skills data.
#[derive(Clone, Debug)]
pub struct PromptInputs<'a> {
    /// Identity slot, normally the daemon-supplied base system prompt.
    pub identity: &'a str,
    /// Names visible to the model for this run.
    pub tool_names: &'a [&'a str],
    /// Whether the skill_manage tool is available.
    pub has_skill_manage: bool,
    /// Pre-rendered skills index, or None when no skills are installed.
    pub skills_index: Option<&'a str>,
    /// Pre-rendered subagent catalogue, or None when delegation is off.
    pub agents_index: Option<&'a str>,
    /// Raw MEMORY.md snapshot content.
    pub memory: &'a str,
    /// Raw USER.md snapshot content.
    pub user: &'a str,
    /// Project context files, already labelled and capped by the caller.
    pub project_context: String,
    /// Ambient application-provided context (the AG-UI `context` and `forwardedProps`),
    /// already rendered by the caller. Absent when the client sent none.
    pub external_context: Option<&'a str>,
    /// Model id used by every model gate.
    pub model: &'a str,
    /// Provider id used by the provider gate.
    pub provider: &'a str,
    /// False when the daemon runs without elevated privileges, so the model
    /// does not reach for sudo.
    pub is_root: bool,
    /// Operating system name, so install commands shown to the user fit it.
    pub os: String,
    /// Session platform, for example 'cli' or 'tui'.
    pub platform: &'a str,
    /// Session id shown in the timestamp trailer.
    pub session_id: String,
    /// Current working directory, when the run has a workspace.
    pub cwd: Option<String>,
    /// Rendered workspace snapshot, when the run has a workspace.
    pub workspace: Option<String>,
    /// Conversation start, used for the timestamp line.
    pub started_at: DateTime<Utc>,
}

/// The three cache tiers, before the final join.
#[derive(Clone, Debug, Default)]
pub struct PromptTiers {
    pub stable: String,
    pub context: String,
    pub volatile: String,
}

impl PromptTiers {
    /// Drop blank tiers and join the rest in stable, context, volatile order.
    pub fn join(&self) -> String {
        [
            self.stable.as_str(),
            self.context.as_str(),
            self.volatile.as_str(),
        ]
        .iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim())
        .collect::<Vec<_>>()
        .join("\n\n")
    }
}

/// Longest skill description the index shows. Descriptions written to route other agents run
/// to a thousand characters on every request; picking a skill needs far less, and skill_view
/// loads the whole skill.
const SKILL_DESCRIPTION_CHARS: usize = 200;

/// The skills index grouped by category, sorted, descriptions capped at SKILL_DESCRIPTION_CHARS,
/// first description winning for a duplicate name. None when empty.
pub fn render_skills_index(skills: &[SkillSummary]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut by_category: BTreeMap<&str, BTreeMap<&str, String>> = BTreeMap::new();
    for skill in skills {
        let category = skill.category.as_deref().unwrap_or("general");
        let description = skill.description.trim();
        let description = match description.char_indices().nth(SKILL_DESCRIPTION_CHARS) {
            Some((cut, _)) => format!("{}…", description[..cut].trim_end()),
            None => description.to_string(),
        };
        by_category
            .entry(category)
            .or_default()
            .entry(skill.name.as_str())
            .or_insert(description);
    }
    let mut body = String::new();
    for (category, entries) in &by_category {
        body.push_str(&format!("  {category}:\n"));
        for (name, description) in entries {
            if description.is_empty() {
                body.push_str(&format!("    - {name}\n"));
            } else {
                body.push_str(&format!("    - {name}: {description}\n"));
            }
        }
    }
    // Load only on a clear match: an "err toward loading" rule sends small models into
    // skills for tasks that need none, and into guessing names that do not exist.
    Some(format!(
        "## Skills\nWhen the task clearly matches a skill's description, load it with skill_view(name) and follow it. Otherwise load none: most tasks need no skill, and only the names below exist.\n\n<available_skills>\n{body}</available_skills>"
    ))
}

/// Longest agent description the index shows, for the same reason skills cap theirs: the
/// description routes the choice, and `skill_view`/`read_file` loads the rest.
const AGENT_DESCRIPTION_CHARS: usize = 220;

/// The subagent catalogue for `delegate_task`, kept in the prompt rather than the tool description
/// like the skills index, so the cached prefix survives a new definition.
pub fn render_agents_index(agents: &[crate::subagent::AgentSummary]) -> Option<String> {
    if agents.is_empty() {
        return None;
    }
    let mut body = String::new();
    for agent in agents {
        let description = agent.description.trim();
        let description = match description.char_indices().nth(AGENT_DESCRIPTION_CHARS) {
            Some((cut, _)) => format!("{}…", description[..cut].trim_end()),
            None => description.to_string(),
        };
        body.push_str(&format!(
            "  - {}: {} (Tools: {})",
            agent.name,
            description,
            agent.tools_description()
        ));
        if let Some(model) = &agent.model {
            body.push_str(&format!(", model {model}"));
        }
        body.push('\n');
    }
    Some(format!(
        "## Subagents\n\
         Delegate with delegate_task: one call, a `tasks` list, and each task runs on its own and \
         reports back. Independent tasks go in the same call and run in parallel; the subagents \
         see only their own prompt, so write it as a full brief, and they cannot ask the user \
         anything. A subagent's report is not shown to the user, so relay what matters. Omit \
         `agent` for the general-purpose agent.\n\n<available_agents>\n{body}</available_agents>"
    ))
}

/// Without a workspace there are no file or shell tools; small models otherwise invent a
/// directory listing when asked about files.
const NO_WORKSPACE_NOTE: &str = "No workspace is open, so you cannot see or change files or run commands. If the user asks about files or code on disk, say so and ask them to open the folder as a workspace; never guess its contents.";

/// Assemble the three prompt tiers in the documented order.
pub fn build_system_prompt_parts(input: &PromptInputs) -> PromptTiers {
    let has_tools = !input.tool_names.is_empty();
    let has_workspace = input
        .workspace
        .as_deref()
        .map(|w| !w.trim().is_empty())
        .unwrap_or(false);

    let mut stable: Vec<Option<String>> = Vec::new();
    stable.push(non_empty(input.identity));
    if has_tools {
        stable.push(Some(SILVER_HELP_GUIDANCE.to_string()));
    }
    if has_tools {
        stable.push(task_completion(has_tools));
        stable.push(parallel_tool_calls(has_tools));
    }
    stable.push(tool_guidance_block(input));
    stable.push(sudo_tip(input));
    stable.push(tool_use_enforcement(input, has_tools));
    stable.push(google_operational(input, has_tools));
    stable.push(execution_discipline(input, has_tools));
    stable.push(alibaba_identity(input));
    if has_workspace {
        if has_file_or_shell_tool(input) {
            stable.push(Some(coding_brief(input)));
        } else {
            stable.push(Some(NO_FILE_TOOLS_NOTE.to_string()));
        }
    } else {
        stable.push(Some(NO_WORKSPACE_NOTE.to_string()));
    }

    let mut context: Vec<Option<String>> = Vec::new();
    context.push(project_context(input));
    context.push(application_context(input));
    if has_workspace {
        context.push(workspace_snapshot(input));
    }

    let mut volatile: Vec<Option<String>> = Vec::new();
    volatile.push(skills_index(input));
    volatile.push(agents_index(input));
    volatile.extend(memory_blocks(input).into_iter().map(Some));
    volatile.push(Some(timestamp_line(input)));
    volatile.push(runtime_environment(input));

    PromptTiers {
        stable: join_tier(&stable),
        context: join_tier(&context),
        volatile: join_tier(&volatile),
    }
}

/// Assemble and join the full system prompt.
pub fn build_system_prompt(input: &PromptInputs) -> String {
    build_system_prompt_parts(input).join()
}

/// Model gate for a guidance block.
pub fn model_gate(gate: Gate, model: &str, defaults: &[&str]) -> bool {
    match gate {
        Gate::On => true,
        Gate::Off => false,
        Gate::Auto => {
            let lower = model.to_ascii_lowercase();
            defaults.iter().any(|needle| lower.contains(needle))
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn join_tier(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .filter_map(|part| part.as_deref())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn task_completion(has_tools: bool) -> Option<String> {
    has_tools.then(|| TASK_COMPLETION_GUIDANCE.to_string())
}

fn parallel_tool_calls(has_tools: bool) -> Option<String> {
    has_tools.then(|| PARALLEL_TOOL_CALL_GUIDANCE.to_string())
}

fn tool_guidance_block(input: &PromptInputs) -> Option<String> {
    let has = |name: &str| input.tool_names.contains(&name);
    let mut parts: Vec<String> = Vec::new();
    if has("memory") {
        parts.push(MEMORY_GUIDANCE.to_string());
    }
    if has("session_search") {
        parts.push(SESSION_SEARCH_GUIDANCE.to_string());
    }
    if has("view_image") || has("search_documents") {
        parts.push(MEDIA_GUIDANCE.to_string());
    }
    if input.has_skill_manage {
        parts.push(SKILLS_GUIDANCE.to_string());
    }
    let joined = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Sudo guard: without root and with a shell, tell the model to leave
/// elevated commands to the user instead of reaching for sudo.
fn sudo_tip(input: &PromptInputs) -> Option<String> {
    if input.is_root {
        return None;
    }
    input
        .tool_names
        .contains(&"bash")
        .then(|| format!("{NON_ROOT_SUDO_TIP} {}.", input.os))
}

fn tool_use_enforcement(input: &PromptInputs, has_tools: bool) -> Option<String> {
    if !has_tools {
        return None;
    }
    model_gate(Gate::Auto, input.model, TOOL_USE_ENFORCEMENT_MODELS)
        .then(|| TOOL_USE_ENFORCEMENT_GUIDANCE.to_string())
}

fn google_operational(input: &PromptInputs, has_tools: bool) -> Option<String> {
    if !has_tools {
        return None;
    }
    if !model_gate(Gate::Auto, input.model, TOOL_USE_ENFORCEMENT_MODELS) {
        return None;
    }
    let lower = input.model.to_ascii_lowercase();
    (lower.contains("gemini") || lower.contains("gemma"))
        .then(|| GOOGLE_MODEL_OPERATIONAL_GUIDANCE.to_string())
}

fn execution_discipline(input: &PromptInputs, has_tools: bool) -> Option<String> {
    if !has_tools {
        return None;
    }
    model_gate(Gate::Auto, input.model, EXECUTION_GUIDANCE_MODELS)
        .then(|| OPENAI_MODEL_EXECUTION_GUIDANCE.to_string())
}

fn alibaba_identity(input: &PromptInputs) -> Option<String> {
    if input.provider != "alibaba" {
        return None;
    }
    let model = input.model;
    let model_short = model.rsplit('/').next().unwrap_or(model);
    Some(format!(
        "You are powered by the model named {model_short}. The exact model ID is {model}. When asked what model you are, always answer based on this information, not on any model name returned by the API."
    ))
}

/// Whether the run can see or change files: a name whose built-in toolset is
/// files or shell.
fn has_file_or_shell_tool(input: &PromptInputs) -> bool {
    input.tool_names.iter().any(|name| {
        matches!(
            crate::toolset::builtin_toolset_for(name),
            crate::toolset::FILES | crate::toolset::TERMINAL
        )
    })
}

fn coding_brief(input: &PromptInputs) -> String {
    let has = |name: &str| input.tool_names.contains(&name);
    let has_patch = has("patch");
    let has_bash = has("bash");
    CODING_AGENT_GUIDANCE
        .lines()
        .filter_map(|line| {
            if line == READ_LINE {
                match (has("read_file"), has("search_files")) {
                    (true, true) => Some(line.to_string()),
                    (true, false) => Some(READ_FILE_LINE.to_string()),
                    _ if has_bash => Some(SHELL_READ_LINE.to_string()),
                    _ => None,
                }
            } else if line == EDIT_LINE {
                if has_patch {
                    Some(line.to_string())
                } else if has_bash {
                    Some(SHELL_EDIT_LINE.to_string())
                } else {
                    None
                }
            } else if line == PATCH_MISS_LINE {
                has_patch.then(|| line.to_string())
            } else if line == BASH_LINE {
                has_bash.then(|| line.to_string())
            } else if line == TODO_SENTENCE {
                Some(if has("todo_list") {
                    TODO_SENTENCE.to_string()
                } else {
                    NO_TODO_SENTENCE.to_string()
                })
            } else {
                Some(line.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn project_context(input: &PromptInputs) -> Option<String> {
    let body = input.project_context.trim();
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "# Project Context\n\nThe following project context files have been loaded and should be followed:\n\n{body}"
    ))
}

/// Ambient context the application handed to the run (the AG-UI `context` and `forwardedProps`).
fn application_context(input: &PromptInputs) -> Option<String> {
    let body = input.external_context?.trim();
    if body.is_empty() {
        return None;
    }
    Some(format!("# Application Context\n\n{body}"))
}

fn workspace_snapshot(input: &PromptInputs) -> Option<String> {
    let workspace = input.workspace.as_deref()?.trim();
    if workspace.is_empty() {
        None
    } else {
        Some(workspace.to_string())
    }
}

fn skills_index(input: &PromptInputs) -> Option<String> {
    let index = input.skills_index?.trim();
    if index.is_empty() {
        return None;
    }
    let has_skill_tool = input
        .tool_names
        .iter()
        .any(|tool| matches!(*tool, "skills_list" | "skill_view" | "skill_manage"));
    if !has_skill_tool {
        return None;
    }
    Some(index.to_string())
}

/// The subagent catalogue, shown only when the run can actually delegate.
fn agents_index(input: &PromptInputs) -> Option<String> {
    let index = input.agents_index?.trim();
    if index.is_empty() || !input.tool_names.contains(&"delegate_task") {
        return None;
    }
    Some(index.to_string())
}

fn memory_blocks(input: &PromptInputs) -> Vec<String> {
    if !input.tool_names.contains(&"memory") {
        return Vec::new();
    }
    let mut parts = Vec::new();
    if let Some(block) = memory_block(MEMORY_BLOCK_HEADER, input.memory, MEMORY_CHAR_LIMIT) {
        parts.push(block);
    }
    if let Some(block) = memory_block(USER_BLOCK_HEADER, input.user, USER_CHAR_LIMIT) {
        parts.push(block);
    }
    parts
}

fn memory_block(header: &str, content: &str, limit: usize) -> Option<String> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    // Memory is untrusted context: a file that looks like an injection or exfiltration
    // payload is replaced by a visible fail-closed marker instead of entering the prompt.
    let content = match gate_context_text(content, header) {
        Ok(clean) => clean,
        Err(marker) => marker,
    };
    let current = content.chars().count();
    let pct = (current * 100)
        .checked_div(limit)
        .map(|value| value.min(100))
        .unwrap_or(0);
    let separator = MEMORY_SEPARATOR.to_string().repeat(46);
    Some(format!(
        "{separator}\n{header} [{pct}%:  {}/{} chars]\n{separator}\n{content}",
        with_commas(current),
        with_commas(limit)
    ))
}

fn timestamp_line(input: &PromptInputs) -> String {
    let date = input.started_at.format("%A, %B %d, %Y");
    let mut line = format!("Conversation started: {date}");
    if !input.session_id.trim().is_empty() {
        line.push_str(&format!("\nSession ID: {}", input.session_id.trim()));
    }
    if !input.model.trim().is_empty() {
        line.push_str(&format!("\nModel: {}", input.model.trim()));
    }
    if !input.provider.trim().is_empty() {
        line.push_str(&format!("\nProvider: {}", input.provider.trim()));
    }
    if !input.platform.trim().is_empty() {
        line.push_str(&format!("\nPlatform: {}", input.platform.trim()));
    }
    line
}

fn runtime_environment(input: &PromptInputs) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(cwd) = input
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    {
        lines.push(format!("Current working directory: {cwd}"));
    }
    if !input.platform.trim().is_empty() {
        lines.push(format!("Platform: {}", input.platform.trim()));
    }
    if !input.os.trim().is_empty() {
        lines.push(format!("OS: {}", input.os.trim()));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "{RUNTIME_ENVIRONMENT_HEADING}\n\n{}",
        lines.join("\n")
    ))
}

fn with_commas(value: usize) -> String {
    let digits = value.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }
    out
}
