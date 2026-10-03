//! Per-session todo lists, keyed by SessionId and kept in memory on purpose: a task list is session
//! state, not durable user data.

use silver_core::error::CoreResult;
use silver_core::services::{TodoItem, TodoStore as TodoStoreBackend};
use silver_protocol::SessionId;
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Per-session in-memory todo lists.
pub struct TodoStore {
    sessions: Mutex<HashMap<SessionId, Vec<TodoItem>>>,
}

impl TodoStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TodoStoreBackend for TodoStore {
    async fn list(&self, session: SessionId) -> CoreResult<Vec<TodoItem>> {
        let sessions = self.sessions.lock().await;
        Ok(sessions.get(&session).cloned().unwrap_or_default())
    }

    async fn write(&self, session: SessionId, items: Vec<TodoItem>) -> CoreResult<Vec<TodoItem>> {
        let mut sessions = self.sessions.lock().await;
        sessions.insert(session, Vec::clone(&items));
        Ok(items)
    }
}
