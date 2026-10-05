//! The bot chat contract: the roster, the messages in a chat or thread, and the live stream. The
//! server routes everything; a client never parses a mention or decides who speaks.

use crate::{ApprovalDecision, ApprovalId, RiskLevel, RunId, SessionId, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotKind {
    Agent,
    Group,
}

impl BotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BotKind::Agent => "agent",
            BotKind::Group => "group",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "agent" => Some(BotKind::Agent),
            "group" => Some(BotKind::Group),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotStatus {
    #[default]
    Idle,
    Working,
    NeedsInput,
    Error,
}

impl BotStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BotStatus::Idle => "idle",
            BotStatus::Working => "working",
            BotStatus::NeedsInput => "needs_input",
            BotStatus::Error => "error",
        }
    }
}

/// One window of a provider's usage limit: its session (Claude Code's five hours, OpenCode Go's
/// rolling window), its week, its month.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub name: String,
    /// How much of the window is used, 0 to 100.
    pub percent: f64,
    /// When the window resets, in unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// One roster row. A group's `status` and `activity` are those of its busy member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BotView {
    pub id: String,
    pub kind: BotKind,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub avatar_shape: String,
    pub avatar_color: String,
    /// The provider preset and model the bot answers with; absent follows the daemon default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort the bot's turns run at; absent follows the daemon default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The project folder the bot works in; absent means no folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Whether the bot acts without asking first.
    pub yolo: bool,
    /// A group's bots, by id.
    pub members: Vec<String>,
    pub pinned: bool,
    pub status: BotStatus,
    /// What the bot is doing right now, one line.
    pub activity: String,
    /// The latest thought of the turn in progress, when the model shares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// When the turn in progress started, in unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// Where the turn in progress talks: a chat id, and a thread root when it is in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    pub last_at: i64,
    pub unread: u32,
    /// An agent's provider's usage limits as last read, the session window first; empty when it
    /// has none we can read, or the session has reset since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<LimitWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    User,
    Agent,
    Notice,
    Permission,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::User => "user",
            EntryKind::Agent => "agent",
            EntryKind::Notice => "notice",
            EntryKind::Permission => "permission",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(EntryKind::User),
            "agent" => Some(EntryKind::Agent),
            "notice" => Some(EntryKind::Notice),
            "permission" => Some(EntryKind::Permission),
            _ => None,
        }
    }
}

/// Under a message that has replies: who answered, how many, how recently.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub count: u32,
    pub last_at: i64,
    /// Bot ids and `"user"`, first reply first.
    pub authors: Vec<String>,
    pub unread: u32,
}

/// An approval a bot is waiting on, shown as a card in its chat.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionView {
    pub approval_id: ApprovalId,
    pub tool: String,
    pub risk: RiskLevel,
    pub description: String,
    pub arguments: Value,
    /// `pending`, then how it ended: `approved`, `denied` or `expired`.
    pub status: String,
}

/// One message, notice or approval card in a chat or a thread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatEntry {
    pub id: String,
    /// Order within the whole chat store; a client sorts and pages by it.
    pub seq: i64,
    /// The bot or group the entry belongs to.
    pub chat_id: String,
    /// The root entry's id when this is a reply in a thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub kind: EntryKind,
    /// The bot that wrote an agent entry or owns a permission card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub text: String,
    /// A user message's delivery state: `queued`, `failed` or `cancelled`; absent once sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// A notice's look: `error` or `divider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// The run and session that wrote this entry, which "Full conversation" opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// The user's reactions, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionView>,
    /// The provider's usage limits when a bot's reply was written, the session window first;
    /// empty when the provider has none we can read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<LimitWindow>,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// What the live stream carries. A client upserts by id and reloads on `resync`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Bot {
        bot: BotView,
    },
    BotRemoved {
        id: String,
    },
    Entry {
        entry: ChatEntry,
    },
    /// The stream fell behind; reload what is on screen.
    Resync,
}

impl ChatEvent {
    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            ChatEvent::Bot { .. } => "bot",
            ChatEvent::BotRemoved { .. } => "bot_removed",
            ChatEvent::Entry { .. } => "entry",
            ChatEvent::Resync => "resync",
        }
    }
}

/// POST /v1/chat/bots. Empty avatar fields are chosen from the name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateBotRequest {
    #[serde(default)]
    pub kind: Option<BotKind>,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub avatar_shape: String,
    #[serde(default)]
    pub avatar_color: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub members: Vec<String>,
}

/// PATCH /v1/chat/bots/{id}. An absent field leaves that attribute unchanged; a blank
/// `provider`, `model`, `workspace_id` or `reasoning_effort` clears it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateBotRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub avatar_shape: Option<String>,
    #[serde(default)]
    pub avatar_color: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub yolo: Option<bool>,
    #[serde(default)]
    pub members: Option<Vec<String>>,
    #[serde(default)]
    pub pinned: Option<bool>,
}

/// POST /v1/chat/bots/{id}/send. `nonce` lets a client match its own message when it comes back.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
}

/// POST /v1/chat/bots/{id}/read: the main chat, or one thread.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReadRequest {
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// POST /v1/chat/entries/{id}/react toggles the user's reaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReactRequest {
    pub emoji: String,
}

/// POST /v1/chat/entries/{id}/answer decides an approval card.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnswerRequest {
    pub decision: ApprovalDecision,
    /// The reply to a question card; it approves the call.
    #[serde(default)]
    pub answer: Option<String>,
}
