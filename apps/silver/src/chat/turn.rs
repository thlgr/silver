//! A bot's turns: the queue that runs them one at a time, and how a run shows up in the chat.
//! The bubble follows the text being written; tools and thinking stay in the session.

use super::store::{now_ms, BotRow};
use super::{new_entry, preview, ChatHub, Runtime};
use serde_json::Value;
use silver_core::error::{CoreError, CoreResult};
use silver_core::session::Session;
use silver_protocol::chat::{BotStatus, ChatEntry, EntryKind, PermissionView};
use silver_protocol::{
    ApprovalDecision, ApprovalId, CreateRunRequest, EventPayload, MessageInput, RiskLevel, RunId,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Streamed text reaches clients at most this often.
const STREAM_EVERY: Duration = Duration::from_millis(100);
/// What the bot is thinking is kept and shown only up to this much, newest last.
const THINKING_CHARS: usize = 4000;
/// Lines of the chat before a thread's message that a new thread session is told about.
const THREAD_CONTEXT_LINES: u32 = 8;

/// Where a message lives: a bot's or group's main chat, or one thread in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    pub chat: String,
    pub thread: Option<String>,
}

impl Lane {
    /// A stable key for the lane, for counters.
    pub fn key(&self) -> Cow<'_, str> {
        match &self.thread {
            Some(thread) => Cow::Owned(format!("{}/{thread}", self.chat)),
            None => Cow::Borrowed(&self.chat),
        }
    }
}

/// One turn's worth of work for a bot.
pub enum Job {
    /// The user's messages in the bot's own lane.
    User { lane: Lane, entries: Vec<String> },
    /// One member's turn in a group's room. The reply is what the room sees, or `None` for a pass.
    Room {
        lane: Lane,
        prompt: String,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Another bot's request. Only the requester sees the answer.
    Ask {
        id: String,
        prompt: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// What a turn is for, so Stop can end the right ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Task {
    Reply,
    /// A turn in the room of this group.
    Room(String),
    /// A request with this id.
    Ask(String),
}

impl Job {
    fn task(&self) -> Task {
        match self {
            Job::User { .. } => Task::Reply,
            Job::Room { lane, .. } => Task::Room(lane.chat.clone()),
            Job::Ask { id, .. } => Task::Ask(id.clone()),
        }
    }
}

/// How a turn ended: the reply, if it said anything, and what went wrong, if something did.
pub struct Outcome {
    pub text: Option<String>,
    pub error: Option<String>,
}

impl Outcome {
    fn failed(reason: impl Into<String>) -> Self {
        Self {
            text: None,
            error: Some(reason.into()),
        }
    }
}

/// Everything one turn needs to run and to show itself.
struct Spec<'a> {
    bot: &'a BotRow,
    session: Session,
    /// Where the reply and the approval cards appear.
    lane: Lane,
    /// A request from another bot has no reply bubble; its notices tell the story.
    silent: bool,
    prompt: String,
    /// A reply that is only "(pass)" says nothing: it is not shown and not passed on.
    pass: bool,
}

impl Spec<'_> {
    /// Where the reply is written, unless there is none.
    fn out(&self) -> Option<&Lane> {
        (!self.silent).then_some(&self.lane)
    }
}

/// "(pass)" and the spellings models give it.
pub fn is_pass(text: &str) -> bool {
    let text = text
        .trim()
        .trim_matches(|c: char| c == '"' || c == '.' || c == '*')
        .to_lowercase();
    text.is_empty() || text == "(pass)" || text == "pass"
}

/// Whether `text` could still turn out to be "(pass)": it is shown only once it cannot.
fn maybe_pass(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    "(pass)".starts_with(&text) || text == "pass"
}

/// A short line for what a tool call is doing.
pub fn activity(tool: &str, args: &Value) -> String {
    let arg = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| args.get(key).and_then(Value::as_str))
            .map(preview)
    };
    let with = |verb: &str, target: Option<String>| match target {
        Some(target) => format!("{verb} {target}"),
        None => format!("{verb}…"),
    };
    match tool {
        "read_file" | "view_image" => with("Reading", arg(&["path"])),
        "write_file" | "patch" => with("Editing", arg(&["path"])),
        "list_files" | "search_files" => with("Searching", arg(&["pattern", "query", "path"])),
        "run_command" | "bash" | "execute_code" | "process_manage" => {
            with("Running", arg(&["command", "code"]))
        }
        "web_search" => with("Searching the web for", arg(&["query"])),
        "web_extract" => with("Reading", arg(&["url"])),
        "delegate_task" => "Delegating a task…".into(),
        "ask_bot" => "Asking another bot…".into(),
        "memory" => "Updating its memory…".into(),
        other => format!("Using {}…", other.replace('_', " ")),
    }
}

impl ChatHub {
    // MARK: queue

    pub(super) fn enqueue(self: &Arc<Self>, bot: &str, job: Job) {
        let start = {
            let mut state = self.state();
            state
                .queues
                .entry(bot.to_string())
                .or_default()
                .push_back(job);
            state.workers.insert(bot.to_string())
        };
        if start {
            tokio::spawn(Arc::clone(self).work(bot.to_string()));
        }
    }

    async fn work(self: Arc<Self>, bot: String) {
        while let Some(job) = self.next_job(&bot) {
            self.run_job(&bot, job).await;
        }
    }

    /// The next job, with the user messages queued right behind it in the same lane folded in.
    /// With nothing left the worker is retired in the same step, so a job queued a moment
    /// later starts a new one.
    fn next_job(&self, bot: &str) -> Option<Job> {
        let mut state = self.state();
        let Some(job) = state
            .queues
            .get_mut(bot)
            .and_then(|queue| queue.pop_front())
        else {
            state.workers.remove(bot);
            return None;
        };
        let Job::User { lane, mut entries } = job else {
            return Some(job);
        };
        if let Some(queue) = state.queues.get_mut(bot) {
            while let Some(Job::User { lane: next, .. }) = queue.front() {
                if *next != lane {
                    break;
                }
                if let Some(Job::User { entries: more, .. }) = queue.pop_front() {
                    entries.extend(more);
                }
            }
        }
        Some(Job::User { lane, entries })
    }

    async fn run_job(self: &Arc<Self>, bot_id: &str, job: Job) {
        let bot = match self.bot(bot_id).await {
            Ok(bot) => bot,
            Err(error) => return self.drop_job(job, &error.to_string()).await,
        };
        match job {
            Job::User { lane, entries } => self.answer_user(&bot, lane, entries).await,
            Job::Room {
                lane,
                prompt,
                reply,
            } => {
                let outcome = match self.session(&bot, None).await {
                    Ok((session, _)) => {
                        let task = Task::Room(lane.chat.clone());
                        let spec = Spec {
                            bot: &bot,
                            session,
                            lane,
                            silent: false,
                            prompt,
                            pass: true,
                        };
                        self.turn(spec, task).await
                    }
                    Err(error) => Outcome::failed(error.to_string()),
                };
                drop(reply.send(match outcome.error {
                    Some(error) => Err(error),
                    None => Ok(outcome.text),
                }));
            }
            Job::Ask { id, prompt, reply } => {
                let outcome = match self.session(&bot, None).await {
                    Ok((session, _)) => {
                        let lane = Lane {
                            chat: bot.id.clone(),
                            thread: None,
                        };
                        let spec = Spec {
                            bot: &bot,
                            session,
                            lane,
                            silent: true,
                            prompt,
                            pass: false,
                        };
                        self.turn(spec, Task::Ask(id)).await
                    }
                    Err(error) => Outcome::failed(error.to_string()),
                };
                drop(reply.send(match (outcome.error, outcome.text) {
                    (Some(error), _) => Err(error),
                    (None, Some(text)) => Ok(text),
                    (None, None) => Err("the bot had nothing to say".into()),
                }));
            }
        }
    }

    /// Release whoever waits on a job that will not run.
    async fn drop_job(&self, job: Job, reason: &str) {
        match job {
            Job::User { entries, .. } => {
                for id in entries {
                    if let Ok(Some(mut entry)) = self.db.chat_entry(&id).await {
                        entry.status = Some("cancelled".into());
                        drop(self.save_entry(entry).await);
                    }
                }
            }
            Job::Room { reply, .. } => drop(reply.send(Err(reason.to_string()))),
            Job::Ask { reply, .. } => drop(reply.send(Err(reason.to_string()))),
        }
    }

    /// The user's queued messages, answered as one turn.
    async fn answer_user(self: &Arc<Self>, bot: &BotRow, lane: Lane, entries: Vec<String>) {
        let mut texts = Vec::new();
        for id in &entries {
            let Ok(Some(mut entry)) = self.db.chat_entry(id).await else {
                continue;
            };
            if entry.status.as_deref() == Some("cancelled") {
                continue;
            }
            if entry.status.take().is_some() {
                match self.save_entry(entry).await {
                    Ok(saved) => entry = saved,
                    Err(_) => continue,
                }
            }
            texts.push(entry.text);
        }
        if texts.is_empty() {
            return;
        }
        let mut prompt = texts.join("\n\n");
        let session = match self.session(bot, lane.thread.as_deref()).await {
            Ok((session, fresh)) => {
                if let Some(root) = lane.thread.as_deref().filter(|_| fresh) {
                    prompt = format!("{}\n\n{prompt}", self.thread_intro(bot, root).await);
                }
                session
            }
            Err(error) => return self.notice(&lane, &error.to_string(), "error").await,
        };
        let spec = Spec {
            bot,
            session,
            lane,
            silent: false,
            prompt,
            pass: false,
        };
        // A failure is already a notice in the chat.
        drop(self.turn(spec, Task::Reply).await);
    }

    /// What a thread's new session is told about the message it is on.
    async fn thread_intro(&self, bot: &BotRow, root: &str) -> String {
        let Ok(Some(root)) = self.db.chat_entry(root).await else {
            return String::new();
        };
        let before = self
            .db
            .chat_entries(&bot.id, None, Some(root.seq), THREAD_CONTEXT_LINES)
            .await
            .unwrap_or_default();
        let line = |entry: &ChatEntry| match entry.kind {
            EntryKind::User => Some(format!("User: {}", entry.text)),
            EntryKind::Agent => Some(format!("You: {}", entry.text)),
            _ => None,
        };
        let mut lines = vec![
            "[This is a thread on one message in your chat with the user. What is said here \
             does not reach the main chat.]"
                .to_string(),
        ];
        let earlier: Vec<String> = before.iter().filter_map(line).collect();
        if !earlier.is_empty() {
            lines.push("Earlier in the chat:".into());
            lines.extend(earlier);
        }
        lines.push("The message this thread is about:".into());
        lines.extend(line(&root));
        lines.join("\n")
    }

    /// The bot's session for a lane, and whether it was just made. A new folder or "New
    /// session" changes the epoch in the key, so the bot starts a fresh one.
    pub(super) async fn session(
        &self,
        bot: &BotRow,
        thread: Option<&str>,
    ) -> CoreResult<(Session, bool)> {
        let key = match thread {
            Some(root) => format!("chat:{}:{}:{root}", bot.id, bot.epoch),
            None => format!("chat:{}:{}", bot.id, bot.epoch),
        };
        if let Some(session) = self
            .db
            .find_session_by_external_key(super::SESSION_SOURCE, &key, bot.workspace_id)
            .await?
        {
            return Ok((session, false));
        }
        let now = chrono::Utc::now();
        let session = Session {
            id: silver_protocol::SessionId::new(),
            workspace_id: bot.workspace_id,
            source: super::SESSION_SOURCE.to_string(),
            external_key: Some(key),
            title: Some(bot.name.clone()),
            created_at: now,
            updated_at: now,
        };
        Ok((self.db.create_session(session).await?, true))
    }

    /// A line in the chat, such as an error.
    pub(super) async fn notice(&self, lane: &Lane, text: &str, style: &str) {
        let mut entry = new_entry(&lane.chat, lane.thread.as_deref(), EntryKind::Notice);
        entry.text = text.to_string();
        entry.style = Some(style.to_string());
        if let Err(error) = self.add_entry(entry).await {
            tracing::warn!(%error, "a chat notice could not be saved");
        }
    }

    // MARK: running

    fn update_runtime(&self, bot: &str, change: impl FnOnce(&mut Runtime)) {
        let mut state = self.state();
        match state.runtime.get_mut(bot) {
            Some(live) => change(live),
            None => change(state.runtime.entry(bot.to_string()).or_default()),
        }
    }

    /// Run one turn to its end and show it in the chat.
    async fn turn(self: &Arc<Self>, spec: Spec<'_>, task: Task) -> Outcome {
        let bot = &spec.bot.id;
        self.update_runtime(bot, |live| {
            *live = Runtime {
                status: BotStatus::Working,
                activity: "Working…".into(),
                started_at: Some(now_ms()),
                lane: Some(spec.lane.clone()),
                task: Some(task),
                ..Runtime::default()
            };
        });
        self.publish_bot(bot).await;
        let outcome = self.run_to_end(&spec).await;
        self.expire_permissions(bot).await;
        let failed = outcome.error.as_deref().filter(|error| *error != "stopped");
        self.update_runtime(bot, |live| {
            *live = Runtime {
                status: if failed.is_some() {
                    BotStatus::Error
                } else {
                    BotStatus::Idle
                },
                activity: failed.map(preview).unwrap_or_default(),
                ..Runtime::default()
            };
        });
        self.publish_bot(bot).await;
        outcome
    }

    async fn run_to_end(self: &Arc<Self>, spec: &Spec<'_>) -> Outcome {
        let runs = match self.runs() {
            Ok(runs) => Arc::clone(runs),
            Err(error) => return Outcome::failed(error.to_string()),
        };
        let bot = spec.bot;
        // The bot's agent is its session's route; set it every turn so an edit takes effect. A
        // provider alone means that provider's default model, which for an agent mode is the
        // mode itself.
        let model = bot.model.clone().or_else(|| {
            let preset = silver_protocol::providers::preset(bot.provider.as_deref()?)?;
            Some(preset.default_model.to_string())
        });
        let provider = bot.provider.clone().filter(|_| model.is_some());
        if let Err(error) = self
            .db
            .set_session_model_override(spec.session.id, model, provider)
            .await
        {
            return self.fail(spec, CoreError::from(error)).await;
        }
        // The effort is the session's too, set (or cleared) every turn so an edit takes effect.
        if let Err(error) = self
            .db
            .set_session_reasoning_effort(spec.session.id, bot.reasoning_effort.clone())
            .await
        {
            return self.fail(spec, CoreError::from(error)).await;
        }
        let created = runs
            .create_run(CreateRunRequest {
                workspace_id: bot.workspace_id,
                session_id: Some(spec.session.id),
                source: super::SESSION_SOURCE.into(),
                external_key: None,
                message: MessageInput::text(spec.prompt.as_str()),
                model: None,
                reasoning_effort: None,
                yolo: Some(bot.yolo),
                preset: None,
                plan_mode: None,
                goal_budget: None,
                external_context: Some(context(bot)),
            })
            .await;
        let created = match created {
            Ok(created) => created,
            Err(error) => return self.fail(spec, error).await,
        };
        let stop_asked = self.state().runtime.get_mut(&bot.id).is_some_and(|live| {
            live.run = Some(created.run_id);
            std::mem::take(&mut live.stop_requested)
        });
        if stop_asked {
            drop(runs.stop_run(created.run_id).await);
        }
        let mut subscription = match runs.subscribe(created.run_id, None).await {
            Ok(subscription) => subscription,
            Err(error) => return self.fail(spec, error).await,
        };
        let mut writer = Writer {
            hub: self,
            spec,
            run: created.run_id,
            entry: None,
            segment: String::new(),
            segment_over: false,
            reply: None,
            cards: HashMap::new(),
            error: None,
            last_emit: Instant::now(),
            last_thought: Instant::now(),
        };
        while let Some(event) = subscription.next().await {
            if writer.apply(event.payload).await {
                break;
            }
        }
        writer.finish().await
    }

    /// A turn that could not start or carry on: say why in the chat.
    async fn fail(&self, spec: &Spec<'_>, error: CoreError) -> Outcome {
        let message = error.to_string();
        if let Some(lane) = spec.out() {
            self.notice(lane, &message, "error").await;
        }
        Outcome::failed(message)
    }

    // MARK: stopping

    /// End the turns of `bot` that `wanted` picks: drop the queued ones and stop the running one.
    pub(super) async fn stop_turns(&self, bot: &str, wanted: impl Fn(&Task) -> bool) {
        let (dropped, run) = {
            let mut state = self.state();
            let mut dropped = Vec::new();
            if let Some(queue) = state.queues.get_mut(bot) {
                let (gone, kept): (Vec<Job>, Vec<Job>) = std::mem::take(queue)
                    .into_iter()
                    .partition(|job| wanted(&job.task()));
                queue.extend(kept);
                dropped = gone;
            }
            let run = state.runtime.get_mut(bot).and_then(|live| {
                if !live.task.as_ref().is_some_and(wanted) {
                    return None;
                }
                if live.run.is_none() {
                    live.stop_requested = true;
                }
                live.run
            });
            (dropped, run)
        };
        for job in dropped {
            self.drop_job(job, "stopped").await;
        }
        if let (Some(run), Ok(runs)) = (run, self.runs()) {
            drop(runs.stop_run(run).await);
        }
    }

    /// Stop whatever a bot or group is doing and forget what was waiting behind it.
    pub async fn stop(self: &Arc<Self>, id: &str) -> CoreResult<()> {
        let bot = self.bot(id).await?;
        match bot.kind {
            silver_protocol::chat::BotKind::Agent => self.stop_turns(id, |_| true).await,
            silver_protocol::chat::BotKind::Group => group::stop(self, &bot).await,
        }
        Ok(())
    }
}

use super::group;

/// What the bot is told about itself on every turn, beside the system prompt.
fn context(bot: &BotRow) -> String {
    let mut text = format!(
        "You are {}, a teammate the user messages in a chat app.",
        bot.name
    );
    if !bot.description.is_empty() {
        text.push(' ');
        text.push_str(&bot.description);
    }
    if !bot.instructions.is_empty() {
        text.push_str("\n\n");
        text.push_str(&bot.instructions);
    }
    text.push_str(
        "\n\nThe user sees only your final reply of each turn, not your tool calls: do the \
         work, then answer briefly and conversationally. You can ask the user's other bots for \
         help with list_bots and ask_bot.",
    );
    text
}

/// A run's events, written into the chat as they arrive.
struct Writer<'a, 'b> {
    hub: &'a Arc<ChatHub>,
    spec: &'a Spec<'b>,
    run: RunId,
    /// The reply being written, once there is something to show.
    entry: Option<ChatEntry>,
    /// The text since the last tool call.
    segment: String,
    /// A tool ran since the last text, so the next text starts a new segment.
    segment_over: bool,
    /// The final answer, when the run reached one.
    reply: Option<String>,
    cards: HashMap<ApprovalId, ChatEntry>,
    error: Option<String>,
    last_emit: Instant,
    last_thought: Instant,
}

impl Writer<'_, '_> {
    /// Handle one event. True once the run is over.
    async fn apply(&mut self, event: EventPayload) -> bool {
        match event {
            EventPayload::TextDelta { delta } => self.delta(&delta).await,
            EventPayload::TextCompleted { text } => self.reply = Some(text),
            EventPayload::ReasoningDelta { delta } => self.thought(&delta).await,
            EventPayload::ToolStarted { name, preview, .. } => {
                self.segment_over = true;
                self.working(activity(&name, &preview)).await;
            }
            EventPayload::ApprovalRequired {
                approval_id,
                name,
                risk,
                description,
                arguments_preview,
                ..
            } => {
                self.card(approval_id, name, risk, description, arguments_preview)
                    .await;
            }
            EventPayload::ApprovalResolved {
                approval_id,
                decision,
            } => self.resolved(approval_id, decision).await,
            EventPayload::RunFailed { message, .. } => {
                self.error = Some(message);
                return true;
            }
            EventPayload::RunCancelled { .. } => {
                self.error = Some("stopped".into());
                return true;
            }
            EventPayload::RunCompleted { .. } => return true,
            _ => {}
        }
        false
    }

    async fn working(&self, activity: String) {
        let bot = &self.spec.bot.id;
        self.hub.update_runtime(bot, |live| {
            live.activity = activity;
            live.thinking.clear();
            if live.pending == 0 {
                live.status = BotStatus::Working;
            }
        });
        self.hub.publish_bot(bot).await;
    }

    async fn thought(&mut self, delta: &str) {
        let bot = &self.spec.bot.id;
        self.hub.update_runtime(bot, |live| {
            live.thinking.push_str(delta);
            if let Some((start, _)) = live.thinking.char_indices().rev().nth(THINKING_CHARS) {
                live.thinking.drain(..start);
            }
        });
        if self.last_thought.elapsed() >= Duration::from_millis(400) {
            self.last_thought = Instant::now();
            self.hub.publish_bot(bot).await;
        }
    }

    /// Text arrived: show it in the reply bubble, at the pace clients can take.
    async fn delta(&mut self, delta: &str) {
        if self.segment_over {
            self.segment.clear();
            self.segment_over = false;
            if let Some(entry) = &mut self.entry {
                entry.text.clear();
            }
        }
        self.segment.push_str(delta);
        let Some(lane) = self.spec.out() else { return };
        if self.spec.pass && maybe_pass(&self.segment) {
            return;
        }
        if self.entry.is_none() {
            let mut entry = new_entry(&lane.chat, lane.thread.as_deref(), EntryKind::Agent);
            entry.author = Some(self.spec.bot.id.clone());
            entry.run_id = Some(self.run);
            entry.session_id = Some(self.spec.session.id);
            entry.is_final = false;
            entry.text.clone_from(&self.segment);
            match self.hub.add_entry(entry).await {
                Ok(entry) => self.entry = Some(entry),
                Err(error) => tracing::warn!(%error, "a reply could not be saved"),
            }
            self.last_emit = Instant::now();
        } else if self.last_emit.elapsed() >= STREAM_EVERY {
            self.last_emit = Instant::now();
            if let Some(entry) = &mut self.entry {
                entry.text.clone_from(&self.segment);
                self.hub.stream_entry(entry);
            }
        }
    }

    async fn card(
        &mut self,
        approval_id: ApprovalId,
        tool: String,
        risk: RiskLevel,
        description: String,
        arguments: Value,
    ) {
        let bot = &self.spec.bot.id;
        let lane = &self.spec.lane;
        let waiting = format!("Needs your approval: {}", activity(&tool, &arguments));
        let mut entry = new_entry(&lane.chat, lane.thread.as_deref(), EntryKind::Permission);
        entry.author = Some(bot.clone());
        entry.run_id = Some(self.run);
        entry.session_id = Some(self.spec.session.id);
        entry.permission = Some(PermissionView {
            approval_id,
            tool,
            risk,
            description,
            arguments,
            status: "pending".into(),
        });
        match self.hub.add_entry(entry).await {
            Ok(entry) => {
                self.cards.insert(approval_id, entry);
            }
            Err(error) => tracing::warn!(%error, "an approval card could not be saved"),
        }
        self.hub.update_runtime(bot, |live| {
            live.pending += 1;
            live.status = BotStatus::NeedsInput;
            live.activity = waiting;
        });
        self.hub.publish_bot(bot).await;
    }

    async fn resolved(&mut self, approval_id: ApprovalId, decision: ApprovalDecision) {
        let Some(card) = self.cards.remove(&approval_id) else {
            return;
        };
        let status = if decision == ApprovalDecision::Deny {
            "denied"
        } else {
            "approved"
        };
        self.settle(card, status).await;
        let bot = &self.spec.bot.id;
        self.hub.update_runtime(bot, |live| {
            live.pending = live.pending.saturating_sub(1);
            if live.pending == 0 {
                live.status = BotStatus::Working;
                live.activity = "Working…".into();
            }
        });
        self.hub.publish_bot(bot).await;
    }

    async fn settle(&self, mut card: ChatEntry, status: &str) {
        if let Some(permission) = &mut card.permission {
            permission.status = status.into();
        }
        if let Err(error) = self.hub.save_entry(card).await {
            tracing::warn!(%error, "an approval card could not be updated");
        }
    }

    /// The run is over: settle the bubble and the cards, and say how it went.
    async fn finish(mut self) -> Outcome {
        let hub = self.hub;
        let reply = self
            .reply
            .take()
            .unwrap_or_else(|| std::mem::take(&mut self.segment));
        let said = if self.spec.pass {
            !is_pass(&reply)
        } else {
            !reply.trim().is_empty()
        };
        if let Some(mut entry) = self.entry.take() {
            if said {
                entry.text.clone_from(&reply);
                entry.is_final = true;
                if let Err(error) = hub.save_entry(entry).await {
                    tracing::warn!(%error, "a reply could not be saved");
                }
            } else if let Err(error) = hub.remove_entry(entry).await {
                tracing::warn!(%error, "a passed reply could not be removed");
            }
        } else if let (true, Some(lane)) = (said, self.spec.out()) {
            let mut entry = new_entry(&lane.chat, lane.thread.as_deref(), EntryKind::Agent);
            entry.author = Some(self.spec.bot.id.clone());
            entry.run_id = Some(self.run);
            entry.session_id = Some(self.spec.session.id);
            entry.text.clone_from(&reply);
            if let Err(error) = hub.add_entry(entry).await {
                tracing::warn!(%error, "a reply could not be saved");
            }
        }
        for (_, card) in std::mem::take(&mut self.cards) {
            self.settle(card, "expired").await;
        }
        if let (Some(error), Some(lane)) = (self.error.as_deref(), self.spec.out()) {
            if error != "stopped" {
                hub.notice(lane, error, "error").await;
            }
        }
        Outcome {
            text: said.then_some(reply),
            error: self.error,
        }
    }
}
