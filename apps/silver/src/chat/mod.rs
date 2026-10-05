//! The bot chat server: bots and groups, the chat each keeps, and the turns that answer in them.
//! A bot's turns are silver runs in a session of its own (source `chat`); the chat shows only
//! messages, final replies, approval cards and notices.

mod group;
mod limits;
mod permission;
mod store;
mod team;
mod turn;

pub use store::BotRow;
pub use team::TeamBackend;
pub use turn::Lane;

use crate::db::Db;
use crate::run_manager::RunManager;
use silver_core::error::{CoreError, CoreResult};
use silver_protocol::chat::{
    AnswerRequest, BotKind, BotStatus, BotView, ChatEntry, ChatEvent, CreateBotRequest, EntryKind,
    LimitWindow, SendMessageRequest, UpdateBotRequest,
};
use silver_protocol::{ApprovalDecisionRequest, RunId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use store::{now_ms, BotStats};
use tokio::sync::{broadcast, Notify};

/// The `sessions.source` of every session a bot runs in.
pub const SESSION_SOURCE: &str = "chat";

const SHAPES: [&str; 8] = [
    "blob", "pebble", "squircle", "tablet", "wedge", "hex", "cloud", "teardrop",
];
const COLORS: [&str; 11] = [
    "black", "brown", "red", "orange", "yellow", "green", "cyan", "blue", "violet", "magenta",
    "gray",
];
const MAX_NAME_CHARS: usize = 60;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_REACTIONS: usize = 8;
const MAX_GROUP_MEMBERS: usize = 12;
const PREVIEW_CHARS: usize = 120;

/// What a bot is doing right now, kept in memory only.
#[derive(Default)]
struct Runtime {
    status: BotStatus,
    activity: String,
    thinking: String,
    started_at: Option<i64>,
    /// Where the turn in progress talks.
    lane: Option<Lane>,
    run: Option<RunId>,
    task: Option<turn::Task>,
    /// Stop was asked before the run existed.
    stop_requested: bool,
    /// Approvals the turn is waiting on.
    pending: usize,
}

#[derive(Default)]
struct State {
    runtime: HashMap<String, Runtime>,
    queues: HashMap<String, VecDeque<turn::Job>>,
    /// Bots with a worker draining their queue.
    workers: HashSet<String>,
    /// How many times a room turn started in a lane; a newer one ends the older.
    rooms: HashMap<String, u64>,
    asks: team::Asks,
    /// An external agent's requests the user has not answered yet, by their card's entry.
    permissions: HashMap<String, permission::Waiting>,
    /// The usage limits last read for each provider, shared by every bot that answers with it.
    limits: HashMap<String, Vec<LimitWindow>>,
    /// Why each provider's limits could not be read last time, so a repeat is not logged again.
    limit_errors: HashMap<String, String>,
}

pub struct ChatHub {
    db: Db,
    runs: OnceLock<Arc<RunManager>>,
    events: broadcast::Sender<ChatEvent>,
    state: Mutex<State>,
    /// Wakes the usage-limit watch before its next round.
    recheck: Notify,
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::InvalidRequest(message.into())
}

fn new_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

/// A message on one line, clipped, for a roster row; the lines that only name an attached file
/// are left out.
fn preview(text: &str) -> String {
    let words: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with("[attached:"))
        .flat_map(str::split_whitespace)
        .collect();
    let line = words.join(" ");
    match line.char_indices().nth(PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line,
    }
}

/// An avatar part the caller named, or one chosen from the bot's name so it is stable.
fn avatar_part(choice: &str, options: &[&str], name: &str, salt: usize) -> CoreResult<String> {
    let choice = choice.trim();
    if choice.is_empty() {
        let sum = name.bytes().fold(salt, |sum, byte| sum + usize::from(byte));
        return Ok(options[sum % options.len()].to_string());
    }
    if options.contains(&choice) {
        return Ok(choice.to_string());
    }
    Err(invalid(format!(
        "unknown avatar {choice:?}; use one of {}",
        options.join(", ")
    )))
}

fn clean_name(name: &str) -> CoreResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("a bot needs a name"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(invalid(format!(
            "a name can have at most {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_string())
}

/// `None` for a blank string, so an empty field clears the setting.
fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A reasoning effort the caller named, or None for the daemon default; a level the daemon
/// does not accept is a bad request.
fn effort(value: Option<String>) -> CoreResult<Option<String>> {
    let value = non_blank(value);
    if let Some(level) = &value {
        if !crate::config::is_valid_reasoning_effort(level) {
            return Err(invalid(format!(
                "reasoning_effort {level:?} must be one of {}",
                crate::config::REASONING_EFFORTS.join(", ")
            )));
        }
    }
    Ok(value)
}

impl ChatHub {
    pub fn new(db: Db) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        Arc::new(Self {
            db,
            runs: OnceLock::new(),
            events,
            state: Mutex::new(State::default()),
            recheck: Notify::new(),
        })
    }

    /// Give the hub the run manager its bots run on. Called once at startup; the manager holds
    /// the hub's team tools, so the two are joined after both exist.
    pub fn attach(&self, runs: Arc<RunManager>) {
        drop(self.runs.set(runs));
    }

    /// Finish what a restart interrupted. Call once, before serving.
    pub async fn recover(&self) -> CoreResult<()> {
        Ok(self.db.chat_recover().await?)
    }

    fn runs(&self) -> CoreResult<&Arc<RunManager>> {
        self.runs
            .get()
            .ok_or_else(|| CoreError::Internal("the chat has no run manager".into()))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.events.subscribe()
    }

    /// Nobody listening is fine: the chat is on disk and a client reloads when it connects.
    fn emit(&self, event: ChatEvent) {
        drop(self.events.send(event));
    }

    // MARK: roster

    pub async fn bots(&self) -> CoreResult<Vec<BotView>> {
        let (rows, stats) = tokio::try_join!(self.db.chat_bots(), self.db.chat_bot_stats())?;
        Ok(self.views(rows, &stats))
    }

    fn views(&self, rows: Vec<BotRow>, stats: &HashMap<String, BotStats>) -> Vec<BotView> {
        let names: HashMap<String, String> = rows
            .iter()
            .map(|bot| (bot.id.clone(), bot.name.clone()))
            .collect();
        let state = self.state();
        rows.into_iter()
            .map(|row| {
                let stats = stats.get(&row.id);
                let limits = self.limits_in(&state, &row);
                view(row, &names, &state.runtime, stats, limits)
            })
            .collect()
    }

    pub async fn bot_view(&self, id: &str) -> CoreResult<BotView> {
        self.bots()
            .await?
            .into_iter()
            .find(|bot| bot.id == id)
            .ok_or_else(|| invalid(format!("no bot {id}")))
    }

    /// Tell every client about a bot, and about the groups it is in, whose row follows it.
    async fn publish_bot(&self, id: &str) {
        match self.bots().await {
            Ok(bots) => {
                for bot in bots {
                    if bot.id == id || bot.members.iter().any(|member| member == id) {
                        self.emit(ChatEvent::Bot { bot });
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "the bot roster could not be read"),
        }
    }

    pub(crate) async fn bot(&self, id: &str) -> CoreResult<BotRow> {
        self.db
            .chat_bot(id)
            .await?
            .ok_or_else(|| invalid(format!("no bot {id}")))
    }

    /// The agent bots that can be asked for something: everyone but `except`.
    async fn agents(&self, except: Option<&str>) -> CoreResult<Vec<BotRow>> {
        Ok(self
            .db
            .chat_bots()
            .await?
            .into_iter()
            .filter(|bot| bot.kind == BotKind::Agent && Some(bot.id.as_str()) != except)
            .collect())
    }

    pub async fn create_bot(self: &Arc<Self>, request: CreateBotRequest) -> CoreResult<BotView> {
        let kind = request.kind.unwrap_or(BotKind::Agent);
        let name = clean_name(&request.name)?;
        let members = match kind {
            BotKind::Agent => Vec::new(),
            BotKind::Group => self.valid_members(&request.members).await?,
        };
        if kind == BotKind::Group {
            if let Some(existing) = self.group_of(&members).await? {
                return self.bot_view(&existing).await;
            }
        }
        if let Some(id) = request.workspace_id {
            self.workspace(id).await?;
        }
        let row = BotRow {
            id: new_id(),
            kind,
            avatar_shape: avatar_part(&request.avatar_shape, &SHAPES, &name, 0)?,
            avatar_color: avatar_part(&request.avatar_color, &COLORS, &name, 1)?,
            name,
            description: request.description.trim().to_string(),
            instructions: request.instructions.trim().to_string(),
            provider: non_blank(request.provider),
            model: non_blank(request.model),
            reasoning_effort: effort(request.reasoning_effort)?,
            workspace_id: request.workspace_id,
            yolo: request.yolo,
            members,
            pinned: false,
            epoch: 0,
            created_at: now_ms(),
        };
        let row = self.db.save_chat_bot(row).await?;
        self.recheck.notify_one();
        self.publish_bot(&row.id).await;
        self.bot_view(&row.id).await
    }

    pub async fn update_bot(
        self: &Arc<Self>,
        id: &str,
        request: UpdateBotRequest,
    ) -> CoreResult<BotView> {
        let mut row = self.bot(id).await?;
        if let Some(name) = request.name {
            row.name = clean_name(&name)?;
        }
        if let Some(description) = request.description {
            row.description = description.trim().to_string();
        }
        if let Some(pinned) = request.pinned {
            row.pinned = pinned;
        }
        if let Some(members) = request.members.filter(|_| row.kind == BotKind::Group) {
            row.members = self.valid_members(&members).await?;
            // The same members share one group, as in `create_bot`; refuse an edit that
            // would make this group a duplicate of another.
            if let Some(existing) = self.group_of(&row.members).await? {
                if existing != row.id {
                    return Err(invalid("a group with these bots already exists"));
                }
            }
        }
        if row.kind == BotKind::Agent {
            if let Some(instructions) = request.instructions {
                row.instructions = instructions.trim().to_string();
            }
            if let Some(shape) = request.avatar_shape {
                row.avatar_shape = avatar_part(&shape, &SHAPES, &row.name, 0)?;
            }
            if let Some(color) = request.avatar_color {
                row.avatar_color = avatar_part(&color, &COLORS, &row.name, 1)?;
            }
            if request.provider.is_some() {
                row.provider = non_blank(request.provider);
            }
            if request.model.is_some() {
                row.model = non_blank(request.model);
            }
            if request.reasoning_effort.is_some() {
                row.reasoning_effort = effort(request.reasoning_effort)?;
            }
            if let Some(yolo) = request.yolo {
                row.yolo = yolo;
            }
        }
        if let Some(workspace) = request.workspace_id {
            let workspace = match non_blank(Some(workspace)) {
                Some(raw) => {
                    let parsed = raw
                        .parse()
                        .map_err(|error: silver_protocol::IdParseError| {
                            invalid(error.to_string())
                        })?;
                    self.workspace(parsed).await?;
                    Some(parsed)
                }
                None => None,
            };
            // A session never changes folder, so a new folder starts a new session.
            if workspace != row.workspace_id {
                row.workspace_id = workspace;
                row.epoch += 1;
            }
        }
        self.db.save_chat_bot(row).await?;
        self.recheck.notify_one();
        self.publish_bot(id).await;
        self.bot_view(id).await
    }

    async fn workspace(&self, id: silver_protocol::WorkspaceId) -> CoreResult<()> {
        match self.db.get_workspace(id).await? {
            Some(_) => Ok(()),
            None => Err(CoreError::WorkspaceNotFound(id)),
        }
    }

    pub async fn delete_bot(self: &Arc<Self>, id: &str) -> CoreResult<()> {
        let row = self.bot(id).await?;
        self.stop(id).await?;
        // A stopped run still writes its last events to the session it is about to lose.
        for _ in 0..50 {
            if !self.is_busy(id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        self.db
            .delete_sessions_with_key_prefix(SESSION_SOURCE, &format!("chat:{id}:"))
            .await?;
        self.db.delete_chat_bot(id).await?;
        self.state().runtime.remove(id);
        self.emit(ChatEvent::BotRemoved { id: row.id });
        // A deleted bot leaves its groups, and a group left with nobody goes too.
        for group in self.db.chat_bots().await? {
            if group.kind == BotKind::Group && group.members.iter().any(|member| member == id) {
                let members: Vec<String> = group
                    .members
                    .iter()
                    .filter(|member| *member != id)
                    .cloned()
                    .collect();
                if members.is_empty() {
                    Box::pin(self.delete_bot(&group.id)).await?;
                } else {
                    let group = self.db.save_chat_bot(BotRow { members, ..group }).await?;
                    self.publish_bot(&group.id).await;
                }
            }
        }
        Ok(())
    }

    /// Existing, distinct agent bots, in the order given.
    async fn valid_members(&self, ids: &[String]) -> CoreResult<Vec<String>> {
        let agents = self.agents(None).await?;
        let mut members: Vec<String> = Vec::new();
        for id in ids.iter().map(|id| id.trim()) {
            if agents.iter().any(|bot| bot.id == id) && !members.iter().any(|member| member == id) {
                members.push(id.to_string());
            }
        }
        if members.is_empty() {
            return Err(invalid("a group needs at least one bot"));
        }
        if members.len() > MAX_GROUP_MEMBERS {
            return Err(invalid(format!(
                "a group can have at most {MAX_GROUP_MEMBERS} bots"
            )));
        }
        Ok(members)
    }

    /// The group whose members are exactly `members`, if there is one.
    async fn group_of(&self, members: &[String]) -> CoreResult<Option<String>> {
        // Members are distinct, so the same length and every one present is the same set.
        Ok(self.db.chat_bots().await?.into_iter().find_map(|bot| {
            let same = bot.members.len() == members.len()
                && bot.members.iter().all(|member| members.contains(member));
            (bot.kind == BotKind::Group && same).then_some(bot.id)
        }))
    }

    /// Start the bot over with a fresh context. The chat stays.
    pub async fn new_session(self: &Arc<Self>, id: &str) -> CoreResult<()> {
        let mut row = self.bot(id).await?;
        if row.kind != BotKind::Agent {
            return Err(invalid("only a bot has a session"));
        }
        row.epoch += 1;
        self.db.save_chat_bot(row).await?;
        let mut divider = new_entry(id, None, EntryKind::Notice);
        divider.text = "New session".into();
        divider.style = Some("divider".into());
        self.add_entry(divider).await?;
        Ok(())
    }

    // MARK: entries

    /// One page of a lane, newest last, with the thread summaries of its messages.
    pub async fn entries(
        &self,
        chat_id: &str,
        thread: Option<&str>,
        before: Option<i64>,
        limit: u32,
    ) -> CoreResult<Vec<ChatEntry>> {
        self.bot(chat_id).await?;
        let mut entries = self
            .db
            .chat_entries(chat_id, thread, before, limit.clamp(1, 200))
            .await?;
        if thread.is_none() {
            let roots: Vec<String> = entries
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::User | EntryKind::Agent))
                .map(|entry| entry.id.clone())
                .collect();
            let mut summaries = self.db.chat_thread_summaries(chat_id, roots).await?;
            for entry in &mut entries {
                entry.thread = summaries.remove(&entry.id);
            }
        }
        Ok(entries)
    }

    pub async fn add_entry(&self, entry: ChatEntry) -> CoreResult<ChatEntry> {
        let entry = self.db.chat_insert_entry(entry).await?;
        self.publish_entry(&entry).await;
        Ok(entry)
    }

    /// Tell clients about an entry and, when it is a reply, about its root's new summary.
    async fn publish_entry(&self, entry: &ChatEntry) {
        let mut entry = ChatEntry::clone(entry);
        if let Some(root) = entry.thread_id.clone() {
            self.emit(ChatEvent::Entry { entry });
            self.publish_root(&root).await;
        } else {
            if matches!(entry.kind, EntryKind::User | EntryKind::Agent) {
                entry.thread = self.summary_of(&entry.chat_id, &entry.id).await;
            }
            self.emit(ChatEvent::Entry { entry });
        }
    }

    async fn summary_of(
        &self,
        chat_id: &str,
        root: &str,
    ) -> Option<silver_protocol::chat::ThreadSummary> {
        let roots = vec![root.to_string()];
        let mut summaries = self.db.chat_thread_summaries(chat_id, roots).await.ok()?;
        summaries.remove(root)
    }

    async fn publish_root(&self, root: &str) {
        if let Ok(Some(mut root)) = self.db.chat_entry(root).await {
            root.thread = self.summary_of(&root.chat_id, &root.id).await;
            self.emit(ChatEvent::Entry { entry: root });
        }
    }

    /// Write an entry's changes to disk and tell clients.
    pub async fn save_entry(&self, entry: ChatEntry) -> CoreResult<ChatEntry> {
        let entry = self.db.chat_update_entry(entry).await?;
        self.publish_entry(&entry).await;
        Ok(entry)
    }

    // MARK: sending

    /// Store the user's message and start whoever answers it.
    pub async fn send(
        self: &Arc<Self>,
        id: &str,
        request: SendMessageRequest,
    ) -> CoreResult<ChatEntry> {
        let bot = self.bot(id).await?;
        let text = request.text.trim();
        if text.is_empty() {
            return Err(invalid("a message needs some text"));
        }
        if text.len() > MAX_TEXT_BYTES {
            return Err(invalid(format!(
                "a message can have at most {} KiB",
                MAX_TEXT_BYTES / 1024
            )));
        }
        if let Some(root) = request.thread_id.as_deref() {
            let valid = self.db.chat_entry(root).await?.is_some_and(|entry| {
                entry.chat_id == bot.id
                    && entry.thread_id.is_none()
                    && matches!(entry.kind, EntryKind::User | EntryKind::Agent)
            });
            if !valid {
                return Err(invalid("that message cannot start a thread"));
            }
        }
        // A retry of a message that did arrive is that message, not a second one.
        if let Some(nonce) = request.nonce.as_deref() {
            if let Some(sent) = self.db.chat_entry_by_nonce(id, nonce).await? {
                return Ok(sent);
            }
        }
        let lane = Lane {
            chat: bot.id.clone(),
            thread: request.thread_id,
        };
        let mut entry = new_entry(id, lane.thread.as_deref(), EntryKind::User);
        entry.text = text.to_string();
        entry.nonce = request.nonce;
        if bot.kind == BotKind::Agent && self.is_busy(id) {
            entry.status = Some("queued".into());
        }
        let entry = self.add_entry(entry).await?;
        self.db
            .chat_mark_read(&lane.chat, lane.thread.as_deref())
            .await?;
        match bot.kind {
            BotKind::Agent => self.enqueue(
                id,
                turn::Job::User {
                    lane,
                    entries: vec![entry.id.clone()],
                },
            ),
            BotKind::Group => group::start(self, &bot, lane),
        }
        self.publish_bot(id).await;
        Ok(entry)
    }

    fn is_busy(&self, bot: &str) -> bool {
        self.state().workers.contains(bot)
    }

    pub async fn react(&self, entry_id: &str, emoji: &str) -> CoreResult<ChatEntry> {
        let emoji = emoji.trim();
        if emoji.is_empty() || emoji.chars().count() > 8 {
            return Err(invalid("a reaction is one emoji"));
        }
        let mut entry = self
            .db
            .chat_entry(entry_id)
            .await?
            .ok_or_else(|| invalid(format!("no message {entry_id}")))?;
        if let Some(at) = entry.reactions.iter().position(|have| have == emoji) {
            entry.reactions.remove(at);
        } else if entry.reactions.len() < MAX_REACTIONS {
            entry.reactions.push(emoji.to_string());
        }
        self.save_entry(entry).await
    }

    /// Mark a chat or one of its threads as read.
    pub async fn read(&self, id: &str, thread: Option<&str>) -> CoreResult<()> {
        self.bot(id).await?;
        if self.db.chat_mark_read(id, thread).await? {
            if let Some(root) = thread {
                self.publish_root(root).await;
            }
            self.publish_bot(id).await;
        }
        Ok(())
    }

    /// Answer an approval card, which lets the waiting run go on.
    pub async fn answer(&self, entry_id: &str, request: AnswerRequest) -> CoreResult<()> {
        let entry = self
            .db
            .chat_entry(entry_id)
            .await?
            .ok_or_else(|| invalid(format!("no card {entry_id}")))?;
        let (Some(run), Some(card)) = (entry.run_id, entry.permission) else {
            return Err(invalid("that is not an approval card"));
        };
        if card.status != "pending" {
            return Err(invalid("that card was already answered"));
        }
        if self.answer_permission(entry_id, request.decision).await {
            return Ok(());
        }
        self.runs()?.decide_approval(
            run,
            ApprovalDecisionRequest {
                approval_id: card.approval_id,
                decision: request.decision,
                answer: request.answer,
            },
        )
    }
}

fn new_entry(chat: &str, thread: Option<&str>, kind: EntryKind) -> ChatEntry {
    ChatEntry {
        id: new_id(),
        seq: 0,
        chat_id: chat.to_string(),
        thread_id: thread.map(str::to_string),
        kind,
        author: None,
        text: String::new(),
        status: None,
        style: None,
        run_id: None,
        session_id: None,
        nonce: None,
        reactions: Vec::new(),
        thread: None,
        permission: None,
        limits: Vec::new(),
        created_at: now_ms(),
    }
}

/// A roster row: the bot as stored, what it is doing and what its chat last said. A group shows
/// its busiest member: one waiting on the user, else one writing in the group.
fn view(
    row: BotRow,
    names: &HashMap<String, String>,
    runtime: &HashMap<String, Runtime>,
    stats: Option<&BotStats>,
    limits: Vec<LimitWindow>,
) -> BotView {
    let name_of = |id: &str| names.get(id).map_or("Bot", String::as_str);
    let (live, speaker) = match row.kind {
        BotKind::Agent => (runtime.get(&row.id), None),
        BotKind::Group => {
            let busy = |wanted: BotStatus| {
                row.members.iter().find_map(|member| {
                    let live = runtime.get(member)?;
                    let here = live.lane.as_ref().is_some_and(|lane| lane.chat == row.id);
                    (here && live.status == wanted).then_some((live, member.as_str()))
                })
            };
            match busy(BotStatus::NeedsInput).or_else(|| busy(BotStatus::Working)) {
                Some((live, member)) => (Some(live), Some(name_of(member))),
                None => (None, None),
            }
        }
    };
    let activity = match (live, speaker) {
        (Some(live), Some(name)) => format!("{name}: {}", live.activity),
        (Some(live), None) => live.activity.clone(),
        (None, _) => String::new(),
    };
    let last = stats.and_then(|stats| stats.last.as_ref());
    let last_message = last.map(|(kind, author, text, _)| match (row.kind, kind, author) {
        (BotKind::Group, EntryKind::Agent, Some(author)) => {
            preview(&format!("{}: {text}", name_of(author)))
        }
        _ => preview(text),
    });
    BotView {
        status: live.map_or(BotStatus::Idle, |live| live.status),
        activity,
        thinking: live
            .map(|live| live.thinking.clone())
            .filter(|thought| !thought.is_empty()),
        started_at: live.and_then(|live| live.started_at),
        working_chat: live.and_then(|live| live.lane.as_ref().map(|lane| lane.chat.clone())),
        working_thread: live.and_then(|live| live.lane.as_ref()?.thread.clone()),
        last_message,
        last_at: last.map_or(row.created_at, |(_, _, _, at)| *at),
        unread: stats.map_or(0, |stats| stats.unread),
        limits,
        id: row.id,
        kind: row.kind,
        name: row.name,
        description: row.description,
        instructions: row.instructions,
        avatar_shape: row.avatar_shape,
        avatar_color: row.avatar_color,
        provider: row.provider,
        model: row.model,
        reasoning_effort: row.reasoning_effort,
        workspace_id: row.workspace_id,
        yolo: row.yolo,
        members: row.members,
        pinned: row.pinned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_collapse_whitespace_and_clip() {
        assert_eq!(preview("  hello\n\nthere  "), "hello there");
        assert_eq!(preview("look\n[attached: a.png → .silver/a.png]"), "look");
        let long = "x".repeat(300);
        assert_eq!(preview(&long).chars().count(), PREVIEW_CHARS + 1);
    }

    #[test]
    fn avatars_are_stable_and_validated() {
        let first = avatar_part("", &SHAPES, "Alice", 0).unwrap();
        assert_eq!(first, avatar_part("", &SHAPES, "Alice", 0).unwrap());
        assert_eq!(avatar_part("hex", &SHAPES, "Alice", 0).unwrap(), "hex");
        assert!(avatar_part("square", &SHAPES, "Alice", 0).is_err());
    }

    #[tokio::test]
    async fn a_group_edited_to_match_another_is_returned_not_duplicated() {
        let dir = std::env::temp_dir().join(format!("silver-chat-mod-{}", uuid::Uuid::now_v7()));
        let db = crate::db::Db::open(&dir.join("state.db")).await.unwrap();
        db.migrate().await.unwrap();
        let hub = ChatHub::new(db);

        let alice = hub
            .create_bot(CreateBotRequest {
                name: "Alice".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let bob = hub
            .create_bot(CreateBotRequest {
                name: "Bob".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let cara = hub
            .create_bot(CreateBotRequest {
                name: "Cara".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let group = |members: Vec<String>| CreateBotRequest {
            kind: Some(BotKind::Group),
            name: "Gang".into(),
            members,
            ..Default::default()
        };
        let first = hub
            .create_bot(group(vec![alice.id.clone(), bob.id.clone()]))
            .await
            .unwrap();
        let second = hub
            .create_bot(group(vec![cara.id.clone(), bob.id.clone()]))
            .await
            .unwrap();
        assert_ne!(first.id, second.id);

        // Editing `second` to the same members as `first` is refused: the same members
        // share one group, and a duplicate would lose nothing but the message.
        let error = hub
            .update_bot(
                &second.id,
                UpdateBotRequest {
                    members: Some(vec![alice.id.clone(), bob.id.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        // No duplicate group appeared and `second` kept its own members.
        let groups = hub.bots().await.unwrap();
        assert_eq!(
            groups
                .iter()
                .filter(|bot| bot.kind == BotKind::Group)
                .count(),
            2
        );
        let second = groups.into_iter().find(|bot| bot.id == second.id).unwrap();
        assert_eq!(second.members, [cara.id, bob.id]);
    }

    #[tokio::test]
    async fn a_bots_effort_is_stored_validated_and_cleared() {
        let dir = std::env::temp_dir().join(format!("silver-chat-mod-{}", uuid::Uuid::now_v7()));
        let db = crate::db::Db::open(&dir.join("state.db")).await.unwrap();
        db.migrate().await.unwrap();
        let hub = ChatHub::new(db);

        let bot = hub
            .create_bot(CreateBotRequest {
                name: "Effort".into(),
                reasoning_effort: Some("high".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(bot.reasoning_effort.as_deref(), Some("high"));

        // A level the daemon does not accept is a bad request.
        let error = hub
            .create_bot(CreateBotRequest {
                name: "Too much".into(),
                reasoning_effort: Some("ultra".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must be one of"), "{error}");

        // A blank clears it, back to the daemon default.
        let cleared = hub
            .update_bot(
                &bot.id,
                UpdateBotRequest {
                    reasoning_effort: Some(String::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(cleared.reasoning_effort, None);
    }
}
