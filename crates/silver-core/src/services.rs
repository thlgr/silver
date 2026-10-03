//! Optional per-run backends that concrete tools use. The daemon supplies them; tests can
//! supply in-memory doubles. Keeping them here lets the core tools stay transport-free.

use crate::error::CoreResult;
use crate::workspace::Workspace;
use chrono::{DateTime, Utc};
use silver_protocol::{RunId, Scope, SessionId};
use std::path::PathBuf;
use std::sync::Arc;

/// Everything a tool may need beyond the run context. All entries are optional: a missing
/// backend makes the corresponding tool return a clear error outcome.
#[derive(Clone, Default)]
pub struct ToolServices {
    pub terminal: Option<Arc<dyn TerminalBackend>>,
    pub todos: Option<Arc<dyn TodoStore>>,
    pub skills: Option<Arc<dyn SkillsBackend>>,
    pub web: Option<Arc<dyn WebBackend>>,
    /// Optional pre-write snapshot hook (filesystem checkpoints). A missing sink means
    /// mutating file tools write without capturing undo state.
    pub checkpoints: Option<Arc<dyn CheckpointSink>>,
    /// Language servers for the lsp tool and for checking what an edit broke. A missing
    /// manager means LSP is off: the lsp tool says so and edits go unchecked.
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// The subagents a run may delegate to, and the runner that starts them. A missing
    /// backend means the delegate_task tool is not registered at all.
    pub subagents: Option<Arc<dyn crate::subagent::Subagents>>,
    /// Extra parent environment variables a tool-spawned child may keep, on top of
    /// `tools::command::PRESERVED_ENV`. Empty means the allow-list alone.
    pub env_passthrough: Vec<String>,
    /// Present for every run; supplied by the daemon memory store.
    pub memory: Option<Arc<dyn crate::memory::MemoryStore>>,
    /// Present for every run; applies the scope filter itself.
    pub session_search: Option<Arc<dyn crate::session::SessionSearch>>,
    /// Absent when the daemon has no document store; the search_documents tool then says so.
    pub documents: Option<Arc<dyn DocumentIndex>>,
}

/// What a mutating file tool is about to do to one path. The string form is persisted in
/// the checkpoints.kind column and drives how restore reverses the mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointKind {
    /// The path already existed; its previous bytes are captured so restore rewrites them.
    Replace,
    /// The path did not exist; restore removes whatever the write created.
    Create,
    /// The path was captured before a deletion; restore copies the old bytes back.
    Delete,
}

impl CheckpointKind {
    /// The stable label stored in the checkpoints.kind column.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointKind::Replace => "replace",
            CheckpointKind::Create => "create",
            CheckpointKind::Delete => "delete",
        }
    }
}

/// A pre-write snapshot request. A shared sink cannot tell which run writes, so the tool passes the
/// session and run, the model-facing path (stored, used to restore) and the absolute one.
#[derive(Clone, Debug)]
pub struct CheckpointTarget {
    pub session_id: SessionId,
    /// The admitted run the write belongs to, when the run has been admitted.
    pub run_id: Option<RunId>,
    /// Path exactly as the model addressed it. Stored in the row and resolved against the
    /// workspace root on restore.
    pub path: String,
    /// Absolute, workspace-resolved path that is about to change.
    pub absolute_path: PathBuf,
    pub kind: CheckpointKind,
}

/// Optional pre-write snapshot hook. Best effort: a failure must never block a write, so callers
/// log and drop it.
#[async_trait::async_trait]
pub trait CheckpointSink: Send + Sync {
    async fn before_write(&self, target: CheckpointTarget) -> CoreResult<()>;
}

#[derive(Clone, Debug)]
pub struct TerminalOutput {
    pub output: String,
    pub exit_code: Option<i32>,
    /// Set when the call started a background process.
    pub process_id: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub id: String,
    pub command: String,
    pub running: bool,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAction {
    Read,
    Kill,
    Wait,
}

/// Shell commands and background processes, scoped so one workspace or the global
/// scope never sees another scope's processes.
#[async_trait::async_trait]
pub trait TerminalBackend: Send + Sync {
    async fn run(
        &self,
        scope: &Scope,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
        background: bool,
    ) -> CoreResult<TerminalOutput>;
    async fn processes(&self, scope: &Scope) -> CoreResult<Vec<ProcessInfo>>;
    async fn process_action(
        &self,
        scope: &Scope,
        process_id: &str,
        action: ProcessAction,
    ) -> CoreResult<String>;

    /// Optional lifecycle hook invoked when a scope is torn down (for example a workspace
    /// delete). The default is a no-op so lightweight test doubles need not implement it.
    async fn cleanup(&self, _scope: &Scope) {}
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    /// Hermes status word: pending | in_progress | completed.
    pub status: String,
}

#[async_trait::async_trait]
pub trait TodoStore: Send + Sync {
    async fn list(&self, session: SessionId) -> CoreResult<Vec<TodoItem>>;
    async fn write(&self, session: SessionId, items: Vec<TodoItem>) -> CoreResult<Vec<TodoItem>>;
}

#[derive(Clone, Debug)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct SkillDoc {
    pub summary: SkillSummary,
    pub content: String,
    /// Usage counters for this skill from the daemon's sidecar ledger.
    pub usage: SkillUsage,
}

/// Per-skill usage counters. `created_by` marks provenance ("agent" for skills made through
/// `skill_manage`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillUsage {
    /// Number of times `skill_view` loaded the skill.
    #[serde(default)]
    pub views: u64,
    /// RFC 3339 timestamp of the most recent view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<String>,
    /// Number of times `skill_manage` patched the skill.
    #[serde(default)]
    pub patches: u64,
    /// RFC 3339 timestamp of the most recent patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_patched_at: Option<String>,
    /// Provenance marker written when `skill_manage` created or installed the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

#[async_trait::async_trait]
pub trait SkillsBackend: Send + Sync {
    async fn list(&self) -> CoreResult<Vec<SkillSummary>>;
    /// Listing gated by platform (None skips gating), with `project_dir`'s `.agents/skills`
    /// overlaid and winning name clashes. The default ignores both, so test doubles only need
    /// `list`.
    async fn list_for_platform(
        &self,
        _platform: Option<&str>,
        _project_dir: Option<&std::path::Path>,
    ) -> CoreResult<Vec<SkillSummary>> {
        self.list().await
    }
    /// View a skill by name. `project_dir` overlays the workspace's `.agents/skills`
    /// (checked before the global store) when set.
    async fn view(
        &self,
        name: &str,
        _project_dir: Option<&std::path::Path>,
    ) -> CoreResult<Option<SkillDoc>>;
    /// action: create | update | delete | install. When `project_dir` is set, new
    /// skills are written under that workspace's `.agents/skills`; updates and
    /// deletes act on whichever store holds the skill (project first).
    async fn manage(
        &self,
        action: &str,
        name: &str,
        content: Option<&str>,
        project_dir: Option<&std::path::Path>,
    ) -> CoreResult<String>;
}

#[derive(Clone, Debug)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait::async_trait]
pub trait WebBackend: Send + Sync {
    async fn search(&self, query: &str, limit: usize) -> CoreResult<Vec<WebResult>>;
    async fn extract(&self, url: &str, max_bytes: usize) -> CoreResult<String>;
}

/// One document handed to the index for storage. The reader owns the bytes and the extracted
/// text (only the core crate can extract a PDF); the index owns the storage and the search.
#[derive(Clone, Debug)]
pub struct DocumentInput {
    /// Path as the model addressed it, stored so a hit names a file it can open.
    pub path: String,
    pub bytes: u64,
    /// Modification time in whole seconds, compared with the stored row to skip a re-read.
    pub mtime: i64,
    pub text: String,
}

/// One matching chunk of an indexed document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentHit {
    pub path: String,
    /// Position of the chunk in the document, counting from 0.
    pub ordinal: usize,
    pub snippet: String,
}

#[async_trait::async_trait]
pub trait DocumentIndex: Send + Sync {
    /// Store a document, replacing any row for the same path whose size and mtime already match.
    async fn index(&self, scope: &Scope, document: DocumentInput) -> CoreResult<()>;
    /// Chunks matching `query`, best first, never outside `scope`.
    async fn search(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<DocumentHit>>;
}

/// Helper used by the bash tool to resolve the working directory inside the run scope.
pub fn resolve_cwd<'a>(
    workspace: Option<&'a Workspace>,
    cwd: Option<&str>,
) -> CoreResult<std::borrow::Cow<'a, std::path::Path>> {
    let workspace = workspace
        .ok_or_else(|| crate::CoreError::ToolNotAllowed("bash requires a workspace".into()))?;
    match cwd {
        Some(relative) => Ok(workspace
            .resolve_path(std::path::Path::new(relative))?
            .into()),
        None => Ok(workspace.canonical_root.as_path().into()),
    }
}
