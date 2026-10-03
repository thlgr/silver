//! Subagents on the daemon side: the Markdown definition store, and the runner that turns a
//! `delegate_task` batch into agent turns sharing the parent's run, session, scope and approval
//! gate. Only the report returns; a subagent's own turns are not kept.

use crate::config::Config;
use crate::git_extras;
use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use silver_core::agent::{Agent, AgentConfig, DiscardTranscript, RunControl, TurnOutcome};
use silver_core::context::RunContext;
use silver_core::error::{CoreError, CoreResult};
use silver_core::event::EventEmitter;
use silver_core::plan::Plan;
use silver_core::subagent::{
    nested_event, unknown_agent, AgentDefinition, AgentSummary, DefinitionSource, Isolation,
    SubagentOutcome, SubagentRequest, SubagentTask, Subagents, DEFAULT_AGENT,
};
use silver_protocol::{EventPayload, MessageInput, ToolStatus};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The project-local definitions directory, inside the workspace like `.silver/exec`.
const PROJECT_DIR: &str = ".silver/agents";

/// Definitions on disk: `~/.silver/agents` overlaid by `<workspace>/.silver/agents`, with the
/// built-ins always first. A file the parser rejects is logged and skipped, so one bad
/// definition never takes a run down with it.
pub struct AgentStore {
    global_root: PathBuf,
}

impl AgentStore {
    pub fn new(global_root: PathBuf) -> Self {
        Self { global_root }
    }

    pub fn global_root(&self) -> &Path {
        &self.global_root
    }

    /// Every definition available in a scope, built-ins first, then the custom files with the
    /// project ones shadowing the global ones.
    pub fn list(&self, project_root: Option<&Path>) -> Vec<AgentDefinition> {
        let mut agents = silver_core::subagent::builtin_agents();
        let project_dir = project_root
            .map(|root| root.join(PROJECT_DIR))
            .unwrap_or_else(|| PathBuf::from(PROJECT_DIR));
        for (root, source) in [
            (self.global_root.as_path(), DefinitionSource::Global),
            (project_dir.as_path(), DefinitionSource::Project),
        ] {
            for agent in read_dir(root, source) {
                match agents.iter().position(|known| known.name == agent.name) {
                    Some(index) => agents[index] = agent,
                    None => agents.push(agent),
                }
            }
        }
        agents
    }

    /// One definition by name, or nothing.
    pub fn find(&self, project_root: Option<&Path>, name: &str) -> Option<AgentDefinition> {
        self.list(project_root)
            .into_iter()
            .find(|agent| agent.name == name)
    }

    /// The directory a project's definitions live in.
    pub fn project_dir(workspace_root: &Path) -> PathBuf {
        workspace_root.join(PROJECT_DIR)
    }

    /// The Markdown behind a definition, for an editor. None for a built-in: it has no file.
    pub fn read_raw(&self, project_root: Option<&Path>, name: &str) -> Option<String> {
        let path = self.find(project_root, name)?.path?;
        std::fs::read_to_string(path).ok()
    }

    /// Write a definition into `dir`. The name must be safe as a file stem and the content
    /// must parse, so a definition is either usable or refused with a reason to fix.
    pub fn write(&self, dir: &Path, name: &str, content: &str) -> CoreResult<AgentDefinition> {
        check_name(name)?;
        let path = dir.join(format!("{name}.md"));
        let agent = silver_core::subagent::parse_definition(&path, content)
            .map_err(|error| CoreError::InvalidRequest(error.describe()))?;
        if agent.name != name {
            return Err(CoreError::InvalidRequest(format!(
                "the frontmatter says name: {}, but the agent is called {name}",
                agent.name
            )));
        }
        crate::atomic_file::write(&path, content.as_bytes())
            .map_err(|err| CoreError::Internal(format!("{}: {err}", path.display())))?;
        Ok(agent)
    }

    /// Remove a custom definition. A built-in is not a file and is never removed.
    pub fn delete(&self, dir: &Path, name: &str) -> CoreResult<()> {
        check_name(name)?;
        let path = dir.join(format!("{name}.md"));
        if !path.is_file() {
            return Err(CoreError::InvalidRequest(format!(
                "no custom agent named '{name}' in {}",
                dir.display()
            )));
        }
        std::fs::remove_file(&path)
            .map_err(|err| CoreError::Internal(format!("{}: {err}", path.display())))
    }
}

fn check_name(name: &str) -> CoreResult<()> {
    if silver_core::subagent::valid_definition_name(name) {
        return Ok(());
    }
    Err(CoreError::InvalidRequest(format!(
        "'{name}' is not a usable agent name: use lowercase letters, digits and dashes"
    )))
}

/// Read every `*.md` in one directory, reporting the ones the parser refuses.
fn read_dir(root: &Path, source: DefinitionSource) -> Vec<AgentDefinition> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut agents: Vec<AgentDefinition> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| {
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "could not read subagent definition");
                    return None;
                }
            };
            let mut agent = match silver_core::subagent::parse_definition(&path, &content) {
                Ok(agent) => agent,
                // A Markdown file that is not a definition at all is not an error: the
                // directory is a home for notes too. A malformed one is worth saying out loud.
                Err(_) if !content.trim_start().starts_with("---") => return None,
                Err(error) => {
                    tracing::warn!("{}", error.describe());
                    return None;
                }
            };
            agent.source = source;
            Some(agent)
        })
        .collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    agents
}

/// Runs a batch of subagents on the shared [`Agent`], so they use the delegating run's transport,
/// credentials and route.
pub struct DaemonSubagents {
    agent: Arc<Agent>,
    store: Arc<AgentStore>,
    config: Arc<Config>,
    /// The parent's loop configuration, with the subagent iteration budget already applied.
    subagent_config: AgentConfig,
    max_iterations: u32,
    timeout: Duration,
}

impl DaemonSubagents {
    pub fn new(
        agent: Arc<Agent>,
        store: Arc<AgentStore>,
        config: Arc<Config>,
        agent_config: AgentConfig,
    ) -> Self {
        let max_iterations = config.delegation.max_iterations;
        let timeout = Duration::from_secs(config.delegation.timeout_seconds);
        Self {
            agent,
            store,
            config,
            subagent_config: AgentConfig {
                max_iterations,
                ..agent_config
            },
            max_iterations,
            timeout,
        }
    }

    /// The definitions this run can use: those left with at least one tool in this run.
    fn available(&self, run: &RunContext) -> Vec<(AgentDefinition, Vec<String>)> {
        self.usable(
            run.workspace.as_ref().map(|ws| ws.canonical_root.as_path()),
            &self.agent.tool_names(run.has_workspace()),
        )
    }

    fn usable(
        &self,
        project_root: Option<&Path>,
        visible: &[String],
    ) -> Vec<(AgentDefinition, Vec<String>)> {
        self.store
            .list(project_root)
            .into_iter()
            .filter_map(|agent| {
                let tools = agent.resolve_tools(visible).ok()?;
                Some((agent, tools))
            })
            .collect()
    }

    /// Resolve one task's agent, with the tool list it will actually run with.
    fn resolve(
        &self,
        task: &SubagentTask,
        run: &RunContext,
    ) -> Result<(AgentDefinition, Vec<String>), String> {
        let name = task.agent.as_deref().unwrap_or(DEFAULT_AGENT);
        let mut available = self.available(run);
        match available.iter().position(|(agent, _)| agent.name == name) {
            Some(index) => Ok(available.swap_remove(index)),
            None => {
                let names: Vec<&str> = available
                    .iter()
                    .map(|(agent, _)| agent.name.as_str())
                    .collect();
                Err(unknown_agent(name, &names))
            }
        }
    }

    /// Run one subagent turn, streaming its events into the parent run and reporting what it
    /// said. Never returns an error for the subagent's own failure: a failed subagent is a
    /// report the parent model can read and act on.
    async fn run_one(
        &self,
        index: u32,
        task: SubagentTask,
        req: SubagentRequest<'_>,
    ) -> SubagentOutcome {
        let started = Instant::now();
        let fail = |task: SubagentTask, text: String| SubagentOutcome {
            index,
            agent: task.agent.unwrap_or_else(|| DEFAULT_AGENT.into()),
            description: task.description,
            status: ToolStatus::Failed,
            text,
            tool_uses: 0,
            duration_ms: started.elapsed().as_millis() as u64,
            worktree: None,
        };

        let (definition, tools) = match self.resolve(&task, req.run) {
            Ok(resolved) => resolved,
            Err(reason) => return fail(task, reason),
        };

        // Isolation: a worktree of the workspace's own repo, so a task that edits files cannot
        // touch the working tree the user is looking at.
        let worktree = match task.isolation.or(definition.isolation) {
            Some(Isolation::Worktree) => match self.create_worktree(req.run, index).await {
                Ok(worktree) => Some(worktree),
                Err(reason) => return fail(task, format!("could not create a worktree: {reason}")),
            },
            None => None,
        };

        req.events.emit(EventPayload::SubagentStarted {
            tool_call_id: silver_protocol::ToolCallId::clone(req.call_id),
            index,
            agent: String::clone(&definition.name),
            description: String::clone(&task.description),
            model: Option::clone(&definition.model)
                .unwrap_or_else(|| String::clone(&req.run.model)),
            worktree: worktree
                .as_ref()
                .map(|worktree| worktree.path.display().to_string()),
        });

        let child_ctx = self.child_context(req.run, &definition, worktree.as_ref());
        let child_agent = Agent::clone(self.agent.as_ref())
            .with_config(AgentConfig {
                max_iterations: definition.max_turns.unwrap_or(self.max_iterations),
                ..AgentConfig::clone(&self.subagent_config)
            })
            .with_only_tools(&tools)
            // The advisor reviews a run for the user; a subagent's steps are already visible in
            // the transcript, and a second opinion per subagent costs a model call each.
            .with_advisor(None);

        let (outcome, tool_uses) = self
            .run_child(index, child_ctx, child_agent, task.prompt, &req)
            .await;

        let (status, text) = match outcome {
            TurnOutcome::Completed { text, .. } => (ToolStatus::Completed, text),
            TurnOutcome::Failed { message, .. } => (ToolStatus::Failed, message),
            TurnOutcome::Cancelled { origin } => (
                ToolStatus::Failed,
                format!("the subagent was stopped ({origin})"),
            ),
        };
        let report = if text.trim().is_empty() {
            "the subagent finished without writing a report; treat the work as unfinished".into()
        } else {
            text
        };
        let kept = worktree.map(|worktree| self.finish_worktree(&worktree));

        SubagentOutcome {
            index,
            agent: definition.name,
            description: task.description,
            status,
            text: report,
            tool_uses,
            duration_ms: started.elapsed().as_millis() as u64,
            worktree: kept.flatten(),
        }
    }

    /// Run the child turn, publishing its nested events under `index`. Returns the outcome and
    /// how many tool calls it made.
    async fn run_child(
        &self,
        index: u32,
        child_ctx: Arc<RunContext>,
        child_agent: Agent,
        prompt: String,
        req: &SubagentRequest<'_>,
    ) -> (TurnOutcome, u32) {
        // The child's events leave through their own channel so the parent's sequence stays the
        // parent's, and so a nested turn can be dropped without disturbing it.
        let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
        let child_emitter = EventEmitter::from_sender(req.run.run_id, child_tx);
        let mut tool_uses = 0u32;
        let finished = {
            let forward = async {
                while let Some(event) = child_rx.recv().await {
                    if matches!(event.payload, EventPayload::ToolCompleted { .. }) {
                        tool_uses += 1;
                    }
                    if let Some(payload) = nested_event(req.call_id, index, event) {
                        req.events.emit(payload);
                    }
                }
            };
            let turn = tokio::time::timeout(
                self.timeout,
                child_agent.run_turn(
                    child_ctx,
                    MessageInput::text(prompt),
                    Vec::new(),
                    child_emitter,
                    RunControl::child_of(req.cancel),
                    Arc::clone(req.gate),
                    Arc::new(DiscardTranscript),
                ),
            );
            tokio::pin!(forward, turn);
            let mut forwarding = true;
            let finished = loop {
                tokio::select! {
                    finished = &mut turn => break finished,
                    () = &mut forward, if forwarding => forwarding = false,
                }
            };
            // The child emitter is gone with the turn; the forwarder drains what is left. A tool
            // that kept a copy alive must not hold the tool call open, hence the cap.
            if forwarding {
                drop(tokio::time::timeout(Duration::from_secs(2), forward).await);
            }
            finished
        };
        let outcome = match finished {
            Ok(outcome) => outcome,
            Err(_) => TurnOutcome::Failed {
                code: silver_protocol::ErrorCode::ToolTimeout,
                message: format!(
                    "the subagent ran out of its {}s budget; do the work yourself or ask for a \
                     narrower task",
                    self.timeout.as_secs()
                ),
            },
        };
        (outcome, tool_uses)
    }

    /// The child's run context: the parent's run, session, scope, services and approvals, with
    /// the subagent's own prompt, model and restrictions.
    fn child_context(
        &self,
        parent: &Arc<RunContext>,
        definition: &AgentDefinition,
        worktree: Option<&git_extras::WorktreeInfo>,
    ) -> Arc<RunContext> {
        let mut child = RunContext::clone(parent);
        child.base_system_prompt.clone_from(&definition.body);
        if let Some(model) = &definition.model {
            child.model.clone_from(model);
        }
        // Plan mode's refusals travel down; its tools do not, so a subagent can never put a plan
        // to the user or leave the parent in plan mode.
        if parent.plan.is_on() {
            child.plan = Plan::read_only(std::mem::take(&mut child.plan.file));
        }
        if let Some(worktree) = worktree {
            if let Some(workspace) = &mut child.workspace {
                workspace.canonical_root = std::fs::canonicalize(&worktree.path)
                    .unwrap_or_else(|_| PathBuf::clone(&worktree.path));
            }
        }
        Arc::new(child)
    }

    async fn create_worktree(
        &self,
        run: &RunContext,
        index: u32,
    ) -> Result<git_extras::WorktreeInfo, String> {
        let workspace = run
            .workspace
            .as_ref()
            .ok_or_else(|| "this run has no workspace".to_string())?;
        let root = PathBuf::clone(&workspace.canonical_root);
        let config = Arc::clone(&self.config);
        let name = format!("agent-{index}-{}", short_id());
        tokio::task::spawn_blocking(move || {
            git_extras::worktree_create(&config, &root, Some(&name), false)
        })
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
    }

    /// Remove the worktree if the subagent left it clean; otherwise hand the path back so the
    /// changes are not thrown away. Returns the path only when it had to be kept.
    fn finish_worktree(&self, worktree: &git_extras::WorktreeInfo) -> Option<String> {
        let Some(name) = worktree.path.file_name().and_then(|name| name.to_str()) else {
            return Some(worktree.path.display().to_string());
        };
        let removed = git_extras::worktree_remove(&self.config, &worktree.repo_root, name, false);
        match removed {
            Ok(_) => None,
            Err(err) => {
                tracing::info!(worktree = %worktree.path.display(), %err, "kept the subagent's worktree");
                Some(worktree.path.display().to_string())
            }
        }
    }
}

#[async_trait]
impl Subagents for DaemonSubagents {
    fn prompt_index(&self, project_root: Option<&Path>, visible: &[String]) -> Option<String> {
        let summaries = self
            .usable(project_root, visible)
            .into_iter()
            .map(|(agent, _)| agent.summary())
            .collect::<Vec<AgentSummary>>();
        silver_core::prompt::render_agents_index(&summaries)
    }

    async fn run(
        &self,
        tasks: Vec<SubagentTask>,
        req: SubagentRequest<'_>,
    ) -> CoreResult<Vec<SubagentOutcome>> {
        let running = FuturesUnordered::new();
        for (index, task) in tasks.into_iter().enumerate() {
            running.push(async move {
                let outcome = self.run_one(index as u32, task, req).await;
                // Also for a task that never started, so a client can close every row.
                req.events.emit(EventPayload::SubagentCompleted {
                    tool_call_id: silver_protocol::ToolCallId::clone(req.call_id),
                    index: outcome.index,
                    status: outcome.status,
                    summary: String::clone(&outcome.text),
                    tool_uses: outcome.tool_uses,
                    duration_ms: outcome.duration_ms,
                    worktree: Option::clone(&outcome.worktree),
                });
                outcome
            });
        }
        // The batch is the concurrency window: the tool refuses a wider one, and a run drives
        // one call at a time.
        let mut outcomes: Vec<SubagentOutcome> = running.collect().await;
        outcomes.sort_by_key(|outcome| outcome.index);
        Ok(outcomes)
    }
}

/// A short unique suffix for a temporary directory or worktree name.
fn short_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()[..8].to_string()
}
