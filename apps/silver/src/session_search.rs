//! Scope-confined session history search backed by SQLite.

use crate::db::Db;
use silver_core::error::{CoreError, CoreResult};
use silver_core::session::{Session, SessionSearch, SessionSearchHit, SessionTranscript};
use silver_protocol::{MessageId, Scope, SessionId, WorkspaceId};

pub struct DbSessionSearch {
    db: Db,
}

impl DbSessionSearch {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

/// True when the session is permanently bound to the run's scope (INV-5).
fn in_scope(scope: &Scope, session: &Session) -> bool {
    match scope {
        Scope::Global => session.workspace_id.is_none(),
        Scope::Workspace(id) => session.workspace_id == Some(*id),
    }
}

/// Split a scope into the list_sessions arguments.
fn scope_parts(scope: &Scope) -> (Option<WorkspaceId>, bool) {
    match scope {
        Scope::Global => (None, true),
        Scope::Workspace(id) => (Some(*id), false),
    }
}

#[async_trait::async_trait]
impl SessionSearch for DbSessionSearch {
    async fn search(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<SessionSearchHit>> {
        let (workspace_id, global) = scope_parts(scope);
        self.db
            .search_messages(workspace_id, global, query, limit as u32)
            .await
            .map_err(CoreError::from)
    }

    async fn read_session(
        &self,
        scope: &Scope,
        session_id: SessionId,
        around: Option<MessageId>,
        window: usize,
        limit: usize,
    ) -> CoreResult<Option<SessionTranscript>> {
        // Resolve the session first and reject it when it belongs to another scope:
        // a caller-supplied id must never widen the run's history (INV-5).
        let Some(session) = self.db.get_session(session_id).await? else {
            return Ok(None);
        };
        if !in_scope(scope, &session) {
            return Ok(None);
        }
        // The session was resolved and scope-checked above, so every query below is
        // confined to it. The database answers the window with SQL LIMITs and counts
        // rather than hydrating the whole transcript.
        let window = self
            .db
            .message_window(session_id, around, window, limit)
            .await?;
        Ok(Some(SessionTranscript {
            session,
            messages: window.messages,
            messages_before: window.messages_before,
            messages_after: window.messages_after,
        }))
    }

    async fn recent_sessions(&self, scope: &Scope, limit: usize) -> CoreResult<Vec<Session>> {
        let (workspace_id, global) = scope_parts(scope);
        self.db
            .list_sessions(workspace_id, global, limit as u32, None)
            .await
            .map_err(CoreError::from)
    }
}
