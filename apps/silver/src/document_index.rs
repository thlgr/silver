//! The document index backing `search_documents`, over SQLite.

use crate::db::Db;
use silver_core::error::CoreResult;
use silver_core::services::{DocumentHit, DocumentIndex, DocumentInput};
use silver_protocol::{Scope, WorkspaceId};

pub struct DbDocumentIndex {
    db: Db,
}

impl DbDocumentIndex {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

/// Documents are workspace-scoped (INV-3), so a global run has none of its own to show and
/// never sees another workspace's.
fn workspace_of(scope: &Scope) -> Option<WorkspaceId> {
    match scope {
        Scope::Workspace(id) => Some(*id),
        Scope::Global => None,
    }
}

#[async_trait::async_trait]
impl DocumentIndex for DbDocumentIndex {
    async fn index(&self, scope: &Scope, document: DocumentInput) -> CoreResult<()> {
        let Some(workspace_id) = workspace_of(scope) else {
            return Ok(());
        };
        self.db
            .index_document(workspace_id, document)
            .await
            .map_err(Into::into)
    }

    async fn search(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<DocumentHit>> {
        let Some(workspace_id) = workspace_of(scope) else {
            return Ok(Vec::new());
        };
        self.db
            .search_documents(workspace_id, query, limit as u32)
            .await
            .map_err(Into::into)
    }
}
