//! The search_documents tool: index a document the model was not given, then recall over
//! everything the workspace has already indexed. The scope always comes from the run context
//! (INV-4), so a model cannot widen the search.

use crate::services::{DocumentHit, DocumentInput};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Number of chunks returned. A small model reads the first few and asks again with a narrower
/// query, so a wide list costs more than it explains.
const CHUNK_LIMIT: usize = 5;

/// Register the search_documents tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(SearchDocuments));
}

struct SearchDocuments;

#[async_trait::async_trait]
impl Tool for SearchDocuments {
    fn name(&self) -> &'static str {
        "search_documents"
    }

    fn description(&self) -> &'static str {
        "Search the text of this workspace's documents. With path: index that file first, then search. Without: search what is already indexed. Use it for a document you were not given."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to find in the documents; any word may match."
                },
                "path": {
                    "type": "string",
                    "description": "Document to index before searching, relative to the workspace root."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: Value,
    ) -> crate::error::CoreResult<ToolOutcome> {
        let Some(index) = ctx.run.services.documents.as_ref() else {
            return Ok(ToolOutcome::error("document search is unavailable"));
        };
        let query = super::fs::required_str(&args, "query")?;
        if query.trim().is_empty() {
            return Ok(ToolOutcome::error(
                "search_documents: query must name at least one word",
            ));
        }
        let mut note = String::new();
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            match index_one(ctx, index.as_ref(), path).await {
                Ok(indexed) => note = indexed,
                Err(outcome) => return Ok(outcome),
            }
        }
        let hits = index.search(&ctx.run.scope, &query, CHUNK_LIMIT).await?;
        Ok(ToolOutcome::ok(render(&note, &query, &hits)))
    }
}

/// A path the model named, and a read it asked for, are both things it can fix, so they come
/// back as an outcome rather than a failed run.
fn denied(error: &crate::error::CoreError) -> ToolOutcome {
    ToolOutcome::error(error.to_string())
}

/// Read and extract one workspace file into the index, returning the status note.
async fn index_one(
    ctx: &ToolContext<'_>,
    index: &dyn crate::services::DocumentIndex,
    requested: &str,
) -> Result<String, ToolOutcome> {
    let resolved = ctx
        .run
        .resolve_path(requested)
        .map_err(|error| denied(&error))?;
    if let Some(reason) = crate::safety::read_denied_reason(&resolved) {
        return Err(super::fs::read_denied(&resolved, reason));
    }
    let unreadable = |error: std::io::Error| {
        ToolOutcome::error(format!("cannot read {}: {error}", resolved.display()))
    };
    let metadata = std::fs::metadata(&resolved).map_err(unreadable)?;
    let bytes = std::fs::read(&resolved).map_err(unreadable)?;
    let Some(text) = super::fs::extract_text(&resolved, &bytes) else {
        return Err(ToolOutcome::error(format!(
            "{requested} is not a UTF-8 text file or a PDF, so it has no text to index"
        )));
    };
    let root = &ctx
        .run
        .require_workspace()
        .map_err(|error| denied(&error))?
        .canonical_root;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let path = super::fs::relative_to(root, &resolved);
    let note = format!("indexed {path}; ");
    index
        .index(
            &ctx.run.scope,
            DocumentInput {
                path,
                bytes: bytes.len() as u64,
                mtime,
                text,
            },
        )
        .await
        .map_err(|error| denied(&error))?;
    Ok(note)
}

fn render(note: &str, query: &str, hits: &[DocumentHit]) -> String {
    if hits.is_empty() {
        return format!("{note}no indexed document matches \"{query}\"");
    }
    let mut out = String::new();
    for hit in hits {
        out.push_str(&format!(
            "{} [chunk {}]: {}\n\n",
            hit.path, hit.ordinal, hit.snippet
        ));
    }
    out.push_str("Read the file with read_file to see the whole section.");
    format!("{note}{out}")
}
