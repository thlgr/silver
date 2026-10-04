//! The HTTP/SSE contract between the silver server and its web UI. The agent loop emits the same
//! RunEvent values that go onto the SSE stream and are persisted for replay.

pub mod commands;
pub mod providers;

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Error returned when an opaque identifier string cannot be parsed.
#[derive(Debug, thiserror::Error)]
#[error("invalid {kind} id: {value:?}")]
pub struct IdParseError {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! id_type {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a new time-ordered (v7) identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID.
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $prefix, self.0.simple())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $prefix, self.0.simple())
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let rest = s.strip_prefix($prefix).unwrap_or(s);
                Uuid::parse_str(rest).map(Self).map_err(|_| IdParseError {
                    kind: $kind,
                    value: s.to_string(),
                })
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdParseError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.to_string()
            }
        }
    };
}

id_type!(WorkspaceId, "ws_", "workspace");
id_type!(SessionId, "ses_", "session");
id_type!(RunId, "run_", "run");
id_type!(MessageId, "msg_", "message");
id_type!(ApprovalId, "apr_", "approval");

/// A tool call id as the provider chose it (a UUID, LM Studio's 32-char base62, a counter), kept
/// verbatim: providers pair the call with its result by it, and some echo it back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolCallId(pub String);

impl ToolCallId {
    /// Generate an id for a call the provider did not name, or that it named twice.
    pub fn new() -> Self {
        Self(format!("call_{}", Uuid::now_v7().simple()))
    }

    /// The id as the provider sent it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ToolCallId> for String {
    fn from(value: ToolCallId) -> String {
        value.0
    }
}

impl TryFrom<String> for ToolCallId {
    type Error = IdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() {
            return Err(IdParseError {
                kind: "tool call",
                value,
            });
        }
        Ok(Self(value))
    }
}

impl std::str::FromStr for ToolCallId {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_string())
    }
}

/// Monotonic per-run event sequence number. Doubles as the SSE id field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub u64);

impl EventId {
    pub fn next(&self) -> EventId {
        EventId(self.0 + 1)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The one scope a run belongs to: global or exactly one workspace, never "all" (INV-3, INV-4).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    Workspace(WorkspaceId),
}

impl Scope {
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            Scope::Global => None,
            Scope::Workspace(id) => Some(*id),
        }
    }

    pub fn is_global(&self) -> bool {
        matches!(self, Scope::Global)
    }
}

/// Lifecycle status of a run. Terminal states are Completed, Failed and Cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        )
    }

    /// Whether the state machine permits self transitioning to next.
    pub fn can_transition_to(self, next: RunStatus) -> bool {
        use RunStatus::*;
        match (self, next) {
            (Queued, Running) | (Queued, Cancelled) => true,
            (Running, WaitingApproval)
            | (Running, Completed)
            | (Running, Failed)
            | (Running, Cancelled) => true,
            (WaitingApproval, Running) | (WaitingApproval, Cancelled) => true,
            (a, b) if a == b => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// The wire name, as serde writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

/// One part of a message body. Text, assistant tool calls, and tool results round-trip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// Provider reasoning/thinking text. Not shown to the user, but reasoning models such as
    /// DeepSeek require it to be echoed back on the assistant turn that follows it.
    Reasoning {
        text: String,
    },
    /// A picture the model may look at, produced by `view_image`. `data` is bare base64 with no
    /// data-URL prefix; only the in-flight turn carries the pixels, never the transcript.
    Image {
        media_type: String,
        data: String,
    },
    /// A file the user attached in the composer, already stored in the workspace. It is not
    /// prose: a projection that shows text must not show this, and a provider must be told
    /// the path so it can read the file with a tool.
    Attachment {
        name: String,
        path: String,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { text: text.into() }
    }

    pub fn reasoning(text: impl Into<String>) -> Self {
        ContentPart::Reasoning { text: text.into() }
    }

    /// Encode raw image bytes, so no caller hand-rolls base64 padding.
    pub fn image(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        ContentPart::Image {
            media_type: media_type.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// The line every provider is sent for an attachment. One format, written here, so the
    /// model reads the same thing whichever transport carries it.
    pub fn attachment_note(name: &str, path: &str) -> String {
        format!("[attached: {name} → {path}]")
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn as_reasoning(&self) -> Option<&str> {
        match self {
            ContentPart::Reasoning { text } => Some(text),
            _ => None,
        }
    }
}

/// Risk classification attached to every tool call. Policy, not transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Read,
    Memory,
    Write,
    Process,
    Destructive,
}

impl RiskLevel {
    /// Destructive writes and process execution require an explicit approval.
    pub fn requires_approval(self) -> bool {
        matches!(
            self,
            RiskLevel::Write | RiskLevel::Process | RiskLevel::Destructive
        )
    }
}

/// Stable domain error codes exposed over the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    WorkspaceNotFound,
    WorkspaceUnavailable,
    PathOutsideWorkspace,
    SessionNotFound,
    SessionWorkspaceMismatch,
    SessionBusy,
    Conflict,
    RunNotFound,
    RunNotActive,
    ApprovalNotFound,
    ApprovalStale,
    ToolNotAllowed,
    ToolTimeout,
    ProviderUnavailable,
    ProviderRateLimited,
    ContextTooLarge,
    DaemonRestarted,
    InvalidRequest,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::WorkspaceNotFound => "workspace_not_found",
            ErrorCode::WorkspaceUnavailable => "workspace_unavailable",
            ErrorCode::PathOutsideWorkspace => "path_outside_workspace",
            ErrorCode::SessionNotFound => "session_not_found",
            ErrorCode::SessionWorkspaceMismatch => "session_workspace_mismatch",
            ErrorCode::SessionBusy => "session_busy",
            ErrorCode::Conflict => "conflict",
            ErrorCode::RunNotFound => "run_not_found",
            ErrorCode::RunNotActive => "run_not_active",
            ErrorCode::ApprovalNotFound => "approval_not_found",
            ErrorCode::ApprovalStale => "approval_stale",
            ErrorCode::ToolNotAllowed => "tool_not_allowed",
            ErrorCode::ToolTimeout => "tool_timeout",
            ErrorCode::ProviderUnavailable => "provider_unavailable",
            ErrorCode::ProviderRateLimited => "provider_rate_limited",
            ErrorCode::ContextTooLarge => "context_too_large",
            ErrorCode::DaemonRestarted => "daemon_restarted",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::Internal => "internal",
        }
    }

    /// HTTP status mapping (explicit, never inferred in transport code).
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::WorkspaceNotFound
            | ErrorCode::SessionNotFound
            | ErrorCode::RunNotFound
            | ErrorCode::ApprovalNotFound => 404,
            ErrorCode::SessionWorkspaceMismatch
            | ErrorCode::SessionBusy
            | ErrorCode::Conflict
            | ErrorCode::ApprovalStale
            | ErrorCode::RunNotActive => 409,
            ErrorCode::PathOutsideWorkspace | ErrorCode::ToolNotAllowed => 403,
            ErrorCode::InvalidRequest | ErrorCode::ContextTooLarge => 400,
            ErrorCode::ProviderRateLimited => 429,
            ErrorCode::WorkspaceUnavailable
            | ErrorCode::ToolTimeout
            | ErrorCode::ProviderUnavailable => 503,
            ErrorCode::DaemonRestarted | ErrorCode::Internal => 500,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The wire error envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ApiErrorBody {
                code,
                message: message.into(),
                request_id: None,
                details: serde_json::Value::Null,
            },
        }
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.error.request_id = Some(request_id.into());
        self
    }
}

impl fmt::Display for ApiError {
    /// The message a CLI or TUI should show the user, falling back to the error code when the
    /// gateway sent an empty one. Debug output is not user-facing: dumping the struct hides the
    /// reason (e.g. "not inside a git repository") behind wrapper noise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = self.error.message.trim();
        if message.is_empty() {
            f.write_str(self.error.code.as_str())
        } else {
            f.write_str(message)
        }
    }
}

/// Token accounting for a run. The cache and reasoning buckets default to zero and are omitted when
/// zero, so producers that only know the three totals keep working.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Prompt tokens served from the provider prompt cache, when reported.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cached_tokens: u64,
    /// Completion tokens attributable to reasoning/thinking, when reported. Already
    /// included in completion_tokens; carried separately for cost and telemetry.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub reasoning_tokens: u64,
}

/// Serde predicate: omit a u64 bucket from the wire form when it is zero.
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
    Denied,
    Blocked,
}

impl ToolStatus {
    /// The wire name, as serde writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Running => "running",
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
            ToolStatus::Denied => "denied",
            ToolStatus::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    /// Remember only for this session, same tool and arguments.
    ApproveSession,
    /// Remember across runs and daemon restarts, same tool and arguments.
    ApproveAlways,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFileKind {
    Memory,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryOpKind {
    Add,
    Replace,
    Remove,
}

/// One incremental agent event. Persisted for replay and streamed as SSE.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: RunId,
    pub event_id: EventId,
    pub created_at: DateTime<Utc>,
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl RunEvent {
    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        self.payload.name()
    }
}

/// GET/POST /v1/advisor: whether Jev hints are on, whether an OpenRouter key is available,
/// and the text of each question, by the name `advisor.checked` answers are reported under.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdvisorStatus {
    pub enabled: bool,
    pub has_key: bool,
    pub questions: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetAdvisorRequest {
    pub enabled: bool,
}

/// Payload variants. Serialised internally tagged by type.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    #[serde(rename = "run.queued")]
    RunQueued {
        session_id: SessionId,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
    },
    #[serde(rename = "run.started")]
    RunStarted {
        model: String,
        started_at: DateTime<Utc>,
    },
    /// How full the context window is for the request about to be sent, measured after
    /// compaction. Emitted before every model request of the run.
    #[serde(rename = "context.updated")]
    ContextUpdated { context: ContextUsage },
    #[serde(rename = "text.delta")]
    TextDelta { delta: String },
    /// A slice of the model's reasoning stream, for a live "thinking" indicator; never part
    /// of the transcript and never persisted.
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta { delta: String },
    #[serde(rename = "text.completed")]
    TextCompleted { text: String },
    #[serde(rename = "tool.started")]
    ToolStarted {
        tool_call_id: ToolCallId,
        name: String,
        preview: serde_json::Value,
    },
    #[serde(rename = "tool.output")]
    ToolOutput {
        tool_call_id: ToolCallId,
        chunk: String,
    },
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        tool_call_id: ToolCallId,
        status: ToolStatus,
        summary: String,
    },
    #[serde(rename = "approval.required")]
    ApprovalRequired {
        approval_id: ApprovalId,
        tool_call_id: ToolCallId,
        name: String,
        risk: RiskLevel,
        description: String,
        arguments_preview: serde_json::Value,
    },
    #[serde(rename = "approval.resolved")]
    ApprovalResolved {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    #[serde(rename = "memory.changed")]
    MemoryChanged {
        file: MemoryFileKind,
        operation: MemoryOpKind,
        content_hash: String,
    },
    #[serde(rename = "run.waiting")]
    RunWaiting { reason: String },
    /// A queued mid-turn steering message was delivered to the model at a safe point.
    #[serde(rename = "steer.delivered")]
    SteerDelivered { text: String },
    /// The advisor (Jev) looked at the run: at the `start`, after a round of `tools`, or at an
    /// `answer` about to end the turn. `answers` holds its yes-probability per question; `hints`
    /// what it handed the model this time (empty when nothing applied or all were given).
    #[serde(rename = "advisor.checked")]
    AdvisorChecked {
        point: String,
        answers: std::collections::BTreeMap<String, f64>,
        hints: Vec<String>,
    },
    /// Text silver put in the model's context that neither the user nor a tool wrote,
    /// exactly as sent: the system prompt and each file loaded into it at the start of the
    /// run, then every notice, nudge and discovered AGENTS.md. `label` names it.
    #[serde(rename = "context.injected")]
    ContextInjected { label: String, text: String },
    /// The user approved the plan, so the session left plan mode.
    #[serde(rename = "plan_mode.exited")]
    PlanModeExited,
    /// A subagent started, as one task of a `delegate_task` batch. `index` is its position in
    /// the batch, so a client nests everything that follows under the same tool call.
    #[serde(rename = "subagent.started")]
    SubagentStarted {
        tool_call_id: ToolCallId,
        index: u32,
        agent: String,
        description: String,
        model: String,
        /// The git worktree the task works in, when it is isolated in one: its paths are
        /// relative to this rather than to the workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
    },
    /// One event of the subagent's own turn, wrapped. The envelope lets a client render the
    /// nested turn with the same reducer it uses for the run.
    #[serde(rename = "subagent.step")]
    SubagentStep {
        tool_call_id: ToolCallId,
        index: u32,
        event: Box<EventPayload>,
    },
    /// A subagent finished, or failed, or was stopped. Every task of the batch gets one, a
    /// task that never started included.
    #[serde(rename = "subagent.completed")]
    SubagentCompleted {
        tool_call_id: ToolCallId,
        index: u32,
        status: ToolStatus,
        /// The report the parent model reads, or the failure that replaced it.
        summary: String,
        tool_uses: u32,
        duration_ms: u64,
        /// Set when the task ran in a git worktree that was kept because it has changes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
    },
    #[serde(rename = "run.completed")]
    RunCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        /// Approximate USD list-price estimate for the run, when the model family is known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        duration_ms: u64,
    },
    #[serde(rename = "run.failed")]
    RunFailed { code: ErrorCode, message: String },
    #[serde(rename = "run.cancelled")]
    RunCancelled { origin: String },
    /// Keepalive; never persisted.
    #[serde(rename = "heartbeat")]
    Heartbeat {
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The requested Last-Event-ID is older than the retained window.
    #[serde(rename = "replay.gap")]
    ReplayGap { from_event_id: EventId },
}

impl EventPayload {
    pub fn name(&self) -> &'static str {
        match self {
            EventPayload::RunQueued { .. } => "run.queued",
            EventPayload::RunStarted { .. } => "run.started",
            EventPayload::ContextUpdated { .. } => "context.updated",
            EventPayload::TextDelta { .. } => "text.delta",
            EventPayload::ReasoningDelta { .. } => "reasoning.delta",
            EventPayload::TextCompleted { .. } => "text.completed",
            EventPayload::ToolStarted { .. } => "tool.started",
            EventPayload::ToolOutput { .. } => "tool.output",
            EventPayload::ToolCompleted { .. } => "tool.completed",
            EventPayload::ApprovalRequired { .. } => "approval.required",
            EventPayload::ApprovalResolved { .. } => "approval.resolved",
            EventPayload::MemoryChanged { .. } => "memory.changed",
            EventPayload::RunWaiting { .. } => "run.waiting",
            EventPayload::SteerDelivered { .. } => "steer.delivered",
            EventPayload::AdvisorChecked { .. } => "advisor.checked",
            EventPayload::ContextInjected { .. } => "context.injected",
            EventPayload::PlanModeExited => "plan_mode.exited",
            EventPayload::SubagentStarted { .. } => "subagent.started",
            EventPayload::SubagentStep { .. } => "subagent.step",
            EventPayload::SubagentCompleted { .. } => "subagent.completed",
            EventPayload::RunCompleted { .. } => "run.completed",
            EventPayload::RunFailed { .. } => "run.failed",
            EventPayload::RunCancelled { .. } => "run.cancelled",
            EventPayload::Heartbeat { .. } => "heartbeat",
            EventPayload::ReplayGap { .. } => "replay.gap",
        }
    }

    /// Whether the event must be persisted for replay. Text deltas are coalesced in a
    /// bounded buffer rather than stored one row per token.
    pub fn is_replayable(&self) -> bool {
        !matches!(
            self,
            EventPayload::TextDelta { .. }
                | EventPayload::ReasoningDelta { .. }
                | EventPayload::Heartbeat { .. }
                | EventPayload::ToolOutput { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: WorkspaceId,
    pub name: String,
    pub path: String,
    pub available: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub external_key: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

fn default_source() -> String {
    "local".to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Which skills a preset lets the agent load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillFilter {
    /// Every skill except the listed ones (new skills are on).
    Except(Vec<String>),
    /// Only the listed skills (new skills are off).
    Only(Vec<String>),
}

impl Default for SkillFilter {
    fn default() -> Self {
        SkillFilter::Except(Vec::new())
    }
}

impl SkillFilter {
    pub fn allows(&self, name: &str) -> bool {
        match self {
            SkillFilter::Except(except) => !except.iter().any(|n| n == name),
            SkillFilter::Only(only) => only.iter().any(|n| n == name),
        }
    }
}

/// A named tool and skill selection a session runs with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: SkillFilter,
    #[serde(default, skip_serializing_if = "is_false")]
    pub builtin: bool,
}

/// Approval handling mode selected for the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Prompt for every gated call (the safe default).
    Manual,
    /// Let the auxiliary model auto-approve low-risk calls and prompt otherwise.
    Smart,
    /// Skip approval prompts entirely (YOLO).
    Off,
}

/// Effective approval configuration, as reported by GET /v1/approvals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalStatus {
    /// Mode selected in the configuration.
    pub mode: ApprovalMode,
    /// True when the daemon was started with --yolo / SILVER_YOLO_MODE=1, which pins the
    /// mode to off and refuses runtime changes.
    pub frozen: bool,
}

/// Body for POST /v1/approvals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetApprovalModeRequest {
    /// The mode to persist.
    pub mode: ApprovalMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// One-line preview of the session's opening user message, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Persisted per-session model override; absent when the daemon default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Persisted per-session reasoning effort; absent when the daemon default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Whether the per-session YOLO approval bypass is enabled.
    #[serde(default, skip_serializing_if = "is_false")]
    pub yolo_mode: bool,
    /// Whether the session is in plan mode (reported by the single-session view).
    #[serde(default, skip_serializing_if = "is_false")]
    pub plan_mode: bool,
    /// Id of the preset the session runs with; absent means Minimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The session's standing /goal, when one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SessionGoal>,
    /// The run working in this session right now, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<RunId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A standing objective: after each completed run the daemon starts another run toward it,
/// up to `max` continuations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGoal {
    pub objective: String,
    pub status: GoalStatus,
    /// Continuations started so far.
    pub used: u32,
    pub max: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Exhausted,
}

/// A change to a session's goal: `"pause"`, `"resume"`, `"clear"` or `{"budget": N}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalUpdate {
    Pause,
    /// Reactivate (an exhausted goal gets its full budget back) and continue now if idle.
    Resume,
    Clear,
    Budget(u32),
}

/// PATCH /v1/sessions/{id}. An absent field leaves that attribute unchanged; an empty or
/// blank string clears it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Set or clear the per-session reasoning effort; blank means the daemon default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Set or clear the per-session YOLO approval bypass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_mode: Option<bool>,
    /// Enter or leave plan mode from the session's next run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    /// Switch the session's preset from the session's next run; blank or
    /// `"minimal"` means Minimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalUpdate>,
}

/// GET /v1/sessions/{id}/plan: where the session's plan file lives and what it says.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanView {
    pub path: String,
    /// The plan's text; absent until the model writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageView {
    pub id: MessageId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub role: MessageRole,
    pub content: Vec<ContentPart>,
    /// Name of the first tool call/result carried by the body, when one is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Provider finish reason, when the daemon recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Per-message token count, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<i64>,
    pub created_at: DateTime<Utc>,
}

/// POST /v1/sessions/{id}/rewind. `removed_user_text` is the user prompt that was
/// undone, absent when the session had no user turn left to rewind.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewindResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_user_text: Option<String>,
    /// How many user turns were actually removed (supports `/undo N`).
    #[serde(default)]
    pub turns_undone: usize,
    /// How many message rows were deleted in total.
    #[serde(default)]
    pub rewound_count: usize,
    /// Whether the removed turns edited files, which stay on disk and can be restored from
    /// Checkpoints.
    #[serde(default)]
    pub files_changed: bool,
}

/// Optional query for POST /v1/sessions/{id}/rewind: `?turns=N` removes the newest
/// N user turns (default 1). Clamped server-side to at least 1.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewindQuery {
    #[serde(default)]
    pub turns: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageInput {
    pub content: Vec<ContentPart>,
}

impl MessageInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentPart::text(text)],
        }
    }

    /// Concatenate all text parts.
    pub fn plain_text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentPart::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRunRequest {
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub external_key: Option<String>,
    pub message: MessageInput,
    #[serde(default)]
    pub model: Option<String>,
    /// Set the resolved session's reasoning effort before the run starts (same role as `model`).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Set the resolved session's YOLO approval bypass before the run starts.
    #[serde(default)]
    pub yolo: Option<bool>,
    /// Set the resolved session's preset before the run starts (same role as `yolo`).
    #[serde(default)]
    pub preset: Option<String>,
    /// Enter or leave plan mode before the run starts (same role as `yolo`).
    #[serde(default)]
    pub plan_mode: Option<bool>,
    /// Make this message the session's /goal, continued up to this many times.
    #[serde(default)]
    pub goal_budget: Option<u32>,
    /// Ambient text a client wants in the model's grounding, injected into the run's system
    /// prompt (the AG-UI endpoint renders its `context` and `forwardedProps` into it).
    #[serde(default)]
    pub external_context: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunCreatedResponse {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub status: RunStatus,
    pub events_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunView {
    pub id: RunId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    pub status: RunStatus,
    pub model: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalDecisionRequest {
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
    /// The user's reply to an `ask_user_question` call; it approves the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteerRequest {
    pub message: String,
}

/// How the next request fills the window: measured bytes against the compaction budget (3 bytes per
/// token, a quarter kept for the reply) and the resolved window. After the reply it is sent again
/// with `prompt_tokens`, the provider's own count, which calibrates the bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub system_prompt_bytes: u64,
    pub tool_schema_bytes: u64,
    pub conversation_bytes: u64,
    pub total_bytes: u64,
    pub budget_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilitiesResponse {
    pub server_version: String,
    pub protocol_version: String,
    pub providers: Vec<String>,
    pub tools: Vec<ToolCapability>,
    pub limits: LimitsView,
    pub features: Vec<String>,
    /// The model a run uses when it names none. A client can show what it is about to talk
    /// to before the first prompt is sent, which for a local model is the thing worth
    /// checking first.
    #[serde(default)]
    pub model: String,
    /// The context window resolved for that model, when the daemon knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCapability {
    pub name: String,
    pub risk: RiskLevel,
    pub requires_workspace: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LimitsView {
    pub max_concurrent_runs: u32,
    pub run_timeout_seconds: u64,
    pub max_message_bytes: u64,
    pub memory_max_prompt_bytes_per_file: u64,
}

/// Configuration schema version shared by the daemon and the CLI so the two can never drift.
pub const CONFIG_VERSION: u32 = 1;
