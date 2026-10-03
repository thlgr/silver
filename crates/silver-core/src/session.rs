//! Session, message and run domain types.

use chrono::{DateTime, Utc};
use silver_protocol::{
    ContentPart, MessageId, MessageRole, RunId, RunStatus, SessionId, WorkspaceId,
};

#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    pub workspace_id: Option<WorkspaceId>,
    pub source: String,
    pub external_key: Option<String>,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    /// The scope this session is permanently bound to (INV-5).
    pub fn scope(&self) -> silver_protocol::Scope {
        match self.workspace_id {
            Some(id) => silver_protocol::Scope::Workspace(id),
            None => silver_protocol::Scope::Global,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: MessageId,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub role: MessageRole,
    pub content: Vec<ContentPart>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Run {
    pub id: RunId,
    pub session_id: SessionId,
    pub workspace_id: Option<WorkspaceId>,
    pub status: RunStatus,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

impl Run {
    pub fn can_transition_to(&self, next: RunStatus) -> bool {
        self.status.can_transition_to(next)
    }
}

/// One hit from scope-confined session history search. The scope filter is applied by the
/// implementation; the model can never override it.
#[derive(Clone, Debug)]
pub struct SessionSearchHit {
    pub session_id: SessionId,
    /// The matching message, so the model can read the messages around it.
    pub message_id: MessageId,
    pub title: Option<String>,
    pub snippet: String,
    pub created_at: DateTime<Utc>,
    /// Role of the message the snippet came from, when the backend reports it.
    pub role: Option<MessageRole>,
}

/// A scope-confined transcript read: one session plus the messages returned for
/// the requested read or scroll shape. The scope filter is applied by the
/// implementation; the model can never override it.
#[derive(Clone, Debug)]
pub struct SessionTranscript {
    pub session: Session,
    /// Messages ordered oldest first.
    pub messages: Vec<Message>,
    /// Older messages omitted before the returned window.
    pub messages_before: usize,
    /// Newer messages omitted after the returned window.
    pub messages_after: usize,
}

#[async_trait::async_trait]
pub trait SessionSearch: Send + Sync {
    async fn search(
        &self,
        scope: &silver_protocol::Scope,
        query: &str,
        limit: usize,
    ) -> crate::error::CoreResult<Vec<SessionSearchHit>>;

    /// One session inside `scope`: the `window` messages on each side of `around`, else the newest
    /// `limit`. None outside the scope, which the implementation enforces.
    async fn read_session(
        &self,
        scope: &silver_protocol::Scope,
        session_id: SessionId,
        around: Option<MessageId>,
        window: usize,
        limit: usize,
    ) -> crate::error::CoreResult<Option<SessionTranscript>>;

    /// The most recent sessions in 'scope', newest first.
    async fn recent_sessions(
        &self,
        scope: &silver_protocol::Scope,
        limit: usize,
    ) -> crate::error::CoreResult<Vec<Session>>;
}
