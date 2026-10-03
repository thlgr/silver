//! Subagents: the definitions a run may delegate to, and the contract for running one. A subagent
//! is an `Agent::run_turn` with its own prompt, tools and budget, no view of the parent's
//! conversation, and a single tool result as its report.

use crate::agent::ApprovalGate;
use crate::context::RunContext;
use crate::error::CoreResult;
use crate::event::EventEmitter;
use async_trait::async_trait;
use silver_protocol::{EventPayload, RunEvent, ToolCallId, ToolStatus};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The delegation tool. A subagent never gets one, so delegation cannot nest.
pub const DELEGATE_TASK: &str = "delegate_task";

/// Tools every definition is denied, whatever it asks for.
const ALWAYS_DENIED: &[&str] = &[DELEGATE_TASK];

/// Where a definition came from, in increasing precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefinitionSource {
    /// Compiled into the daemon.
    BuiltIn,
    /// `~/.silver/agents`.
    Global,
    /// `<workspace>/.silver/agents`.
    Project,
}

impl DefinitionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DefinitionSource::BuiltIn => "built-in",
            DefinitionSource::Global => "global",
            DefinitionSource::Project => "project",
        }
    }

    /// Whether a client may edit or delete the definition.
    pub fn editable(self) -> bool {
        !matches!(self, DefinitionSource::BuiltIn)
    }
}

/// How a subagent's file changes are kept away from the working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// A temporary git worktree, removed again when the subagent left it clean.
    Worktree,
}

impl Isolation {
    pub fn as_str(self) -> &'static str {
        match self {
            Isolation::Worktree => "worktree",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "worktree" => Some(Isolation::Worktree),
            _ => None,
        }
    }
}

/// One subagent the model may delegate to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    /// The name a task passes as its `agent`; also the file stem for a custom definition.
    pub name: String,
    /// When to use this subagent, as shown to the model.
    pub description: String,
    /// The subagent's system prompt.
    pub body: String,
    /// Allow-list of tool names; None means every tool the parent run may call.
    pub tools: Option<Vec<String>>,
    /// Deny-list, applied after the allow-list.
    pub disallowed_tools: Vec<String>,
    /// Model id for this subagent; None inherits the parent's.
    pub model: Option<String>,
    /// Iteration budget for this subagent; None inherits the delegation default.
    pub max_turns: Option<u32>,
    /// Whether to run in a worktree by default; a task may override it.
    pub isolation: Option<Isolation>,
    pub source: DefinitionSource,
    /// Where a custom definition lives; None for the built-ins.
    pub path: Option<PathBuf>,
}

impl AgentDefinition {
    /// The tools a task runs with: the allow-list within the parent's tools, minus the deny-list
    /// and the delegation tool (a subagent has no subagents). Err names what emptied it.
    pub fn resolve_tools(&self, parent_tools: &[String]) -> Result<Vec<String>, String> {
        let mut resolved: Vec<String> = match &self.tools {
            Some(allowed) => allowed
                .iter()
                .filter(|name| parent_tools.contains(name))
                .cloned()
                .collect(),
            None => parent_tools.to_vec(),
        };
        resolved.retain(|name| !self.disallowed_tools.contains(name));
        resolved.retain(|name| !ALWAYS_DENIED.contains(&name.as_str()));
        let mut unique: Vec<String> = Vec::with_capacity(resolved.len());
        for name in resolved {
            if !unique.contains(&name) {
                unique.push(name);
            }
        }
        let resolved = unique;
        if resolved.is_empty() {
            return Err(format!(
                "agent '{}' has no tool left: it asked for {:?}, and the run cannot call {}",
                self.name,
                self.tools.as_deref().unwrap_or_default(),
                self.disallowed_tools.join(", ")
            ));
        }
        Ok(resolved)
    }

    /// The one-line catalogue entry the model reads in the system prompt.
    pub fn summary(self) -> AgentSummary {
        AgentSummary {
            name: self.name,
            description: self.description,
            tools: self.tools,
            disallowed_tools: self.disallowed_tools,
            model: self.model,
            max_turns: self.max_turns,
            isolation: self.isolation,
            source: self.source,
            path: self.path.as_ref().map(|path| path.display().to_string()),
        }
    }
}

/// A definition as the daemon's listing and the model's prompt need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSummary {
    pub name: String,
    pub description: String,
    /// None means every tool the run can call.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub isolation: Option<Isolation>,
    pub source: DefinitionSource,
    pub path: Option<String>,
}

impl AgentSummary {
    /// "all tools", "read_file, search_files", or "all tools except write_file", for the prompt.
    pub fn tools_description(&self) -> String {
        match (&self.tools, self.disallowed_tools.is_empty()) {
            (None, true) => "all tools".to_string(),
            (None, false) => format!("all tools except {}", self.disallowed_tools.join(", ")),
            (Some(tools), _) => tools.join(", "),
        }
    }
}

/// One task of a `delegate_task` batch, as the model wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentTask {
    /// The agent to run; None or empty means `general-purpose`.
    pub agent: Option<String>,
    /// The model's short label for the task, shown in the transcript.
    pub description: String,
    /// The full brief. The subagent sees nothing else.
    pub prompt: String,
    /// Overrides the definition's isolation.
    pub isolation: Option<Isolation>,
}

/// What one task of a batch produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentOutcome {
    pub index: u32,
    pub agent: String,
    pub description: String,
    pub status: ToolStatus,
    /// The report for the parent model, or the failure that replaced it.
    pub text: String,
    pub tool_uses: u32,
    pub duration_ms: u64,
    /// Set when a worktree was kept because the subagent changed it.
    pub worktree: Option<String>,
}

impl SubagentOutcome {
    pub fn is_error(&self) -> bool {
        self.status == ToolStatus::Failed
    }
}

/// What a subagent run needs from the tool that started it.
#[derive(Clone, Copy)]
pub struct SubagentRequest<'a> {
    /// The parent run's context, inherited by every subagent.
    pub run: &'a Arc<RunContext>,
    /// The delegation call being answered: every nested event is filed under it.
    pub call_id: &'a ToolCallId,
    /// The parent run's emitter; a subagent's events go out on it.
    pub events: &'a EventEmitter,
    /// Cancelled when the parent stops, so a subagent never outlives its run.
    pub cancel: &'a CancellationToken,
    /// The parent run's approval gate: a subagent asks the user for the same approvals.
    pub gate: &'a Arc<dyn ApprovalGate>,
}

/// The daemon side of running a batch. The catalogue itself is the store's, behind
/// `GET /v1/agents`; what a run needs from here is which of it that run may use, and the run.
#[async_trait]
pub trait Subagents: Send + Sync {
    /// The catalogue for a run's system prompt, kept to the definitions that run can actually
    /// use: a definition whose tools are all absent from `visible` is not worth the model's
    /// tokens. None when there is nothing to delegate to.
    fn prompt_index(&self, project_root: Option<&Path>, visible: &[String]) -> Option<String>;

    /// Run a batch. Implementations stream progress as `subagent.*` events on
    /// `req.events` and return one outcome per task, in the order the tasks were given.
    async fn run(
        &self,
        tasks: Vec<SubagentTask>,
        req: SubagentRequest<'_>,
    ) -> CoreResult<Vec<SubagentOutcome>>;
}

/// The catalogue with no filesystem behind it: the built-ins only.
pub fn builtin_agents() -> Vec<AgentDefinition> {
    vec![general_purpose()]
}

/// The definition a task runs when it names no agent.
pub const DEFAULT_AGENT: &str = "general-purpose";

const GENERAL_PURPOSE_DESCRIPTION: &str = "Multi-step task, solo: trace question across files, \
investigate, implement change end to end. All tools except delegation. Own context. Brief must \
carry full task: goal, reason, known facts, research-only or change-allowed.";

const GENERAL_PURPOSE_BODY: &str = "\
Subagent. Parent conversation invisible. Brief = full input. User unreachable: no questions.

Task loop:
1. Read brief. Extract goal, constraints, named paths.
2. Locate code. Path unknown: search_files (content) or list_files (names). Path known: read_file.
3. Search 2+ spots. Retry alternate names: snake_case, camelCase, plural, abbreviation, old name.
4. Change files only when brief demands change. Existing file: patch (old_string copied exact from read_file). New file: write_file, only when brief names file path.
5. After change: run project build, tests, lint through bash. Read command output. Failing command: fix, rerun. Unfixable: report command + output.
6. Docs, README, comments: write only when brief demands.
7. Delegation unavailable: no subagents exist here.

Report (parent relays to user), in order:
- Done: actions taken, 1 line per action.
- Found: facts, 1 line per fact.
- Paths: absolute only.
- Snippet: only when exact text decides outcome.
- Open: unfinished work, blocker, failing command + output copied.
Empty field: omit.";

fn general_purpose() -> AgentDefinition {
    AgentDefinition {
        name: DEFAULT_AGENT.to_string(),
        description: GENERAL_PURPOSE_DESCRIPTION.to_string(),
        body: GENERAL_PURPOSE_BODY.to_string(),
        tools: None,
        disallowed_tools: Vec::new(),
        model: None,
        max_turns: None,
        isolation: None,
        source: DefinitionSource::BuiltIn,
        path: None,
    }
}

/// A subagent's event as the parent run publishes it, or None to drop it. Its tool calls, context
/// and approvals survive; deltas, its own run boundaries and plan exit do not, and its reply comes
/// with `subagent.completed` rather than a second `text.completed`.
pub fn nested_event(call: &ToolCallId, index: u32, event: RunEvent) -> Option<EventPayload> {
    let keep = matches!(
        event.payload,
        EventPayload::ToolStarted { .. }
            | EventPayload::ToolCompleted { .. }
            | EventPayload::ContextInjected { .. }
            | EventPayload::ApprovalRequired { .. }
            | EventPayload::ApprovalResolved { .. }
    );
    keep.then(|| EventPayload::SubagentStep {
        tool_call_id: ToolCallId::clone(call),
        index,
        event: Box::new(event.payload),
    })
}

/// A definition file that could not be used, named so the author can fix it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionError {
    pub path: PathBuf,
    pub message: String,
}

impl DefinitionError {
    pub fn describe(&self) -> String {
        format!("{}: {}", self.path.display(), self.message)
    }
}

/// Read one definition: `name` and `description` required, plus `tools`, `disallowed_tools`,
/// `model`, `max_turns` and `isolation`. The body passes the injection scan before it is a prompt.
pub fn parse_definition(path: &Path, content: &str) -> Result<AgentDefinition, DefinitionError> {
    let fail = |message: String| DefinitionError {
        path: path.to_path_buf(),
        message,
    };
    let (fields, body) = split_frontmatter(content).map_err(fail)?;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;
    let mut disallowed_tools: Vec<String> = Vec::new();
    let mut model = None;
    let mut max_turns = None;
    let mut isolation = None;
    for (key, value) in fields {
        match key {
            "name" | "description" | "tools" | "disallowed_tools" | "model" | "max_turns"
            | "isolation" => {}
            other => return Err(fail(format!("unknown frontmatter key '{other}'"))),
        }
        if value.is_empty() {
            continue; // a list key opening its items
        }
        // A list key arrives once per item (inline or one per line), so extend, never replace.
        let mut push = |items: Vec<String>| match key {
            "tools" => tools.get_or_insert_with(Vec::new).extend(items),
            "disallowed_tools" => disallowed_tools.extend(items),
            _ => {}
        };
        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "tools" | "disallowed_tools" => push(string_list(&value)),
            "model" => model = Some(value),
            "max_turns" => {
                max_turns = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|turns| *turns > 0)
                        .ok_or_else(|| {
                            fail(format!(
                                "max_turns must be a positive number, got {value:?}"
                            ))
                        })?,
                )
            }
            "isolation" => {
                isolation = Some(
                    Isolation::parse(&value)
                        .ok_or_else(|| fail(format!("unknown isolation mode {value:?}")))?,
                )
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| fail("a definition needs a name".into()))?;
    let description = description.ok_or_else(|| fail("a definition needs a description".into()))?;
    let body = body.trim();
    if body.is_empty() {
        return Err(fail(
            "a definition needs a body: it is the subagent's system prompt".into(),
        ));
    }
    let body = crate::context::gate_context_text(body, &format!("agent {}", name))
        .unwrap_or_else(|blocked| blocked);
    Ok(AgentDefinition {
        name,
        description,
        body,
        tools,
        disallowed_tools,
        model,
        max_turns,
        isolation,
        source: DefinitionSource::Project,
        path: Some(path.to_path_buf()),
    })
}

/// Render a definition back to its file form, for the daemon's editor.
pub fn render_definition(agent: &AgentDefinition) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", agent.name));
    out.push_str(&format!("description: {}\n", agent.description));
    if let Some(tools) = &agent.tools {
        out.push_str(&format!("tools: [{}]\n", tools.join(", ")));
    }
    if !agent.disallowed_tools.is_empty() {
        out.push_str(&format!(
            "disallowed_tools: [{}]\n",
            agent.disallowed_tools.join(", ")
        ));
    }
    if let Some(model) = &agent.model {
        out.push_str(&format!("model: {model}\n"));
    }
    if let Some(turns) = agent.max_turns {
        out.push_str(&format!("max_turns: {turns}\n"));
    }
    if let Some(isolation) = agent.isolation {
        out.push_str(&format!("isolation: {}\n", isolation.as_str()));
    }
    out.push_str("---\n\n");
    out.push_str(agent.body.trim());
    out.push('\n');
    out
}

/// A name a custom definition may use: lowercase, digits and dashes, so it is safe as a file
/// stem and readable in a tool call.
pub fn valid_definition_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Flat `key: value` pairs and list items in order. Not the skills parser: a definition either
/// means what it says or is reported to its author.
/// Frontmatter `key: value` pairs, a list key repeated once per item.
type Fields<'a> = Vec<(&'a str, String)>;

fn split_frontmatter(content: &str) -> Result<(Fields<'_>, String), String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err("a definition starts with a '---' frontmatter fence".into());
    }
    let mut fields: Fields = Vec::new();
    let mut list: Option<&str> = None;
    let mut body = String::new();
    let mut closed = false;
    for line in lines {
        if !closed {
            if line.trim() == "---" {
                closed = true;
                continue;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(item) = trimmed.strip_prefix("- ") {
                let key = list.ok_or_else(|| "a list item must follow a key".to_string())?;
                let item = strip_quotes(item.trim());
                if !item.is_empty() {
                    fields.push((key, item));
                }
                continue;
            }
            let (key, value) = trimmed
                .split_once(':')
                .ok_or_else(|| format!("frontmatter line {trimmed:?} is not 'key: value'"))?;
            let key = key.trim();
            let value = strip_quotes(value.trim());
            if value.is_empty() {
                list = Some(key);
            } else {
                list = None;
                fields.push((key, value));
            }
            continue;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    if !closed {
        return Err("frontmatter is not closed with a '---' fence".into());
    }
    Ok((fields, body))
}

fn strip_quotes(value: &str) -> String {
    let trimmed = value.trim();
    for quote in ['"', '\''] {
        if trimmed.len() >= 2 && trimmed.starts_with(quote) && trimmed.ends_with(quote) {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

/// A list written as `[a, b]`, or as one item per `- ` line.
fn string_list(value: &str) -> Vec<String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(strip_quotes)
        .filter(|item| !item.is_empty())
        .collect()
}

/// The error a task gets when its agent does not exist: the names that do.
pub fn unknown_agent(asked: &str, names: &[&str]) -> String {
    format!(
        "no subagent named '{asked}'. Available: {}",
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    )
}
