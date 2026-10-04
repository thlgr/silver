//! SQLite persistence. Timestamps are RFC3339 text and ids their prefixed form; foreign keys are on
//! for every connection. All scope filtering lives here, so a caller cannot widen a scope
//! (INV-3, INV-4, INV-5).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use silver_core::plan::PlanMode;
use silver_core::services::{DocumentHit, DocumentInput};
use silver_core::session::{Message, Run, Session, SessionSearchHit};
use silver_core::workspace::Workspace;
use silver_protocol::{
    ApprovalId, ContentPart, EventId, EventPayload, MessageId, MessageRole, Preset, RunEvent,
    RunId, RunStatus, SessionGoal, SessionId, TokenUsage, ToolCallId, WorkspaceId,
};
use tokio_rusqlite::rusqlite::{self, params, OptionalExtension};

/// The migrations, embedded so a release binary carries its own. Entry `n` takes a database
/// from `user_version` `n` to `n + 1`.
const MIGRATIONS: [&str; 3] = [
    include_str!("../../../migrations/0001_initial.sql"),
    include_str!("../../../migrations/0002_chat.sql"),
    include_str!("../../../migrations/0003_bot_reasoning_effort.sql"),
];

/// How long SQLite waits for a competing writer before returning SQLITE_BUSY.
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// Upper bound on the WAL after a checkpoint. A long-lived WAL can otherwise grow without
/// limit between checkpoints; the bound is generous enough to never truncate a healthy
/// working set but keeps the sidecar from filling a small disk.
pub const JOURNAL_SIZE_LIMIT_BYTES: i64 = 64 * 1024 * 1024;

/// Every this many successful writes, one connection runs a WAL checkpoint and the
/// SQLite optimize pragma on a spawned task. The request path never waits for it.
const WRITE_MAINTENANCE_INTERVAL: u64 = 256;

/// Maximum length of a derived session title before ellipsis truncation.
const MAX_DERIVED_TITLE_CHARS: usize = 60;

/// Maximum length of a session-list preview before ellipsis truncation.
const MAX_SESSION_PREVIEW_CHARS: usize = 120;

/// Characters of a tool message stored in the search projection. Tool payloads are
/// frequently multi-megabyte machine output; only this bounded prefix reaches an FTS index.
const MAX_INDEXED_TOOL_TEXT_CHARS: usize = 8_192;

/// Appended to a truncated tool projection so a bounded prefix stays recognisable.
const TRUNCATED_TEXT_MARKER: &str = "\n[truncated]";

/// Upper bound on an insights window, so a malformed query cannot overflow the cutoff.
const MAX_INSIGHTS_DAYS: u32 = 3650;

/// Automatic retention never prunes a session touched within this many days.
const AUTO_PRUNE_MAX_AGE_DAYS: u32 = 90;

/// Automatic retention always keeps at least this many of the newest sessions.
const AUTO_PRUNE_KEEP_LAST: usize = 100;

/// Minimum spacing between automatic prune/vacuum passes over one connection.
const AUTO_MAINTENANCE_THROTTLE_SECS: u64 = 6 * 60 * 60;

/// Wall-clock budget for the best-effort startup maintenance pass.
pub const STARTUP_MAINTENANCE_BUDGET: Duration = Duration::from_millis(400);

/// Every database failure surfaced by this module.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] tokio_rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type DbResult<T> = Result<T, DbError>;

impl From<rusqlite::Error> for DbError {
    fn from(err: rusqlite::Error) -> Self {
        DbError::Sqlite(tokio_rusqlite::Error::from(err))
    }
}

/// Map a constraint violation to Conflict; every other sqlite failure passes through.
fn map_constraint(err: rusqlite::Error) -> DbError {
    match err {
        rusqlite::Error::SqliteFailure(code, message)
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            DbError::Conflict(
                message.unwrap_or_else(|| rusqlite::Error::SqliteFailure(code, None).to_string()),
            )
        }
        other => DbError::from(other),
    }
}

/// Coarse cause of a persistence failure, so the API can pick a stable error without forwarding
/// SQLite's text (which can quote statements and stored values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceClass {
    /// A competing writer holds the database (SQLITE_BUSY/SQLITE_LOCKED): transient, retry.
    Locked,
    /// The volume cannot accept more bytes (SQLITE_FULL).
    DiskFull,
    /// The database file is malformed or is not a database (SQLITE_CORRUPT/SQLITE_NOTADB).
    Corrupt,
    /// Any other persistence failure.
    Other,
}

impl PersistenceClass {
    /// Classify one rusqlite failure from its SQLite result code.
    pub fn of(err: &rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::SqliteFailure(code, _) => Self::from_code(code.code),
            _ => Self::Other,
        }
    }

    /// Classify a primary SQLite result code.
    pub fn from_code(code: rusqlite::ErrorCode) -> Self {
        match code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => Self::Locked,
            rusqlite::ErrorCode::DiskFull => Self::DiskFull,
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                Self::Corrupt
            }
            _ => Self::Other,
        }
    }

    /// Stable, SQL-free text used when this class reaches the API boundary.
    pub fn message(self) -> &'static str {
        match self {
            Self::Locked => "database is busy; retry shortly",
            Self::DiskFull => "disk full: the state database cannot accept more writes",
            Self::Corrupt => "database corrupt: the state database is damaged",
            Self::Other => "database error",
        }
    }
}

/// The API error for a failure: BUSY/LOCKED and constraint violations are Conflict, FULL and
/// CORRUPT/NOTADB name the cause, anything else is an opaque Internal.
impl From<DbError> for silver_core::error::CoreError {
    fn from(err: DbError) -> Self {
        use silver_core::error::CoreError;
        match err {
            DbError::Conflict(message) => CoreError::Conflict(message),
            DbError::Sqlite(source) => match source {
                tokio_rusqlite::Error::Error(inner) => core_from_sqlite(&inner),
                tokio_rusqlite::Error::Close((_, inner)) => core_from_sqlite(&inner),
                tokio_rusqlite::Error::ConnectionClosed => {
                    CoreError::Internal("database connection is closed".into())
                }
                // The error enum is non_exhaustive; a future variant is opaque.
                _ => CoreError::Internal("database error".into()),
            },
            DbError::Serde(_) => CoreError::Internal("state decode error".into()),
            DbError::Internal(message) => CoreError::Internal(message),
        }
    }
}

/// Map one SQLite failure to a stable domain error, dropping the raw message.
fn core_from_sqlite(err: &rusqlite::Error) -> silver_core::error::CoreError {
    use silver_core::error::CoreError;
    if let rusqlite::Error::SqliteFailure(code, _) = err {
        if code.code == rusqlite::ErrorCode::ConstraintViolation {
            return CoreError::Conflict("write rejected by a database constraint".into());
        }
    }
    let class = PersistenceClass::of(err);
    if class == PersistenceClass::Locked {
        CoreError::Conflict(class.message().into())
    } else {
        CoreError::Internal(class.message().into())
    }
}

/// Create the database file 0600 before SQLite touches it, so a freshly created state
/// database is never briefly world-readable. Existing files are tightened too.
#[cfg(unix)]
fn precreate_private_file(path: &Path) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
    {
        drop(file);
    }
    drop(std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(0o600),
    ));
}

/// Tighten the database file and any WAL sidecars to 0600. Best effort: SQLite creates the
/// sidecars itself and may apply the main file's mode to them.
#[cfg(unix)]
fn secure_state_files(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let private = || std::fs::Permissions::from_mode(0o600);
    drop(std::fs::set_permissions(path, private()));
    for suffix in ["-wal", "-shm"] {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(suffix);
        let sidecar = PathBuf::from(raw);
        if sidecar.exists() {
            drop(std::fs::set_permissions(&sidecar, private()));
        }
    }
}

/// What to do about the journal mode after asking SQLite for WAL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalPlan {
    /// WAL was accepted; the durability pragmas apply.
    Wal,
    /// The filesystem refused WAL (or reported another rollback mode); use DELETE.
    DeleteFallback,
    /// An in-memory database, where journal modes do not apply.
    NotApplicable,
}

/// The journal plan from the mode SQLite reports after asking for WAL. Some network filesystems
/// silently stay in DELETE, so anything but an exact `wal` falls back to DELETE.
pub fn plan_journal_mode(reported: &str) -> JournalPlan {
    let mode = reported.trim();
    if mode.eq_ignore_ascii_case("wal") {
        JournalPlan::Wal
    } else if mode.eq_ignore_ascii_case("memory") {
        JournalPlan::NotApplicable
    } else {
        JournalPlan::DeleteFallback
    }
}

/// Request WAL, read back the mode SQLite actually selected, and fall back to DELETE when the
/// filesystem refuses WAL. The durability and sizing pragmas apply only when WAL is live.
fn configure_journal(conn: &rusqlite::Connection) -> DbResult<()> {
    let reported: String = conn.query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))?;
    match plan_journal_mode(&reported) {
        JournalPlan::Wal => {
            conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
            set_journal_size_limit(conn)?;
        }
        JournalPlan::DeleteFallback => {
            tracing::warn!(
                reported = reported.as_str(),
                "journal_mode=WAL refused; using DELETE"
            );
            conn.execute_batch("PRAGMA journal_mode = DELETE;")?;
            set_journal_size_limit(conn)?;
        }
        JournalPlan::NotApplicable => {}
    }
    Ok(())
}

/// Bound the journal/WAL file so it cannot grow without limit between checkpoints.
fn set_journal_size_limit(conn: &rusqlite::Connection) -> DbResult<()> {
    let sql = format!("PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT_BYTES};");
    conn.execute_batch(&sql)?;
    Ok(())
}

fn ts(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}

fn parse_ts(raw: &str) -> DbResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|err| DbError::Internal(format!("invalid timestamp {raw:?}: {err}")))
}

/// Whole seconds since the Unix epoch; zero if the clock predates it.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Convert a caller-supplied length into an i64 LIMIT, saturating rather than wrapping.
fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn parse_id<T>(raw: &str, kind: &str) -> DbResult<T>
where
    T: std::str::FromStr<Err = silver_protocol::IdParseError>,
{
    raw.parse::<T>()
        .map_err(|err| DbError::Internal(format!("invalid {kind} id: {err}")))
}

fn role_from(raw: &str) -> Option<MessageRole> {
    match raw {
        "system" => Some(MessageRole::System),
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        "tool" => Some(MessageRole::Tool),
        _ => None,
    }
}

fn run_status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::WaitingApproval => "waiting_approval",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn run_status_from(raw: &str) -> Option<RunStatus> {
    match raw {
        "queued" => Some(RunStatus::Queued),
        "running" => Some(RunStatus::Running),
        "waiting_approval" => Some(RunStatus::WaitingApproval),
        "completed" => Some(RunStatus::Completed),
        "failed" => Some(RunStatus::Failed),
        "cancelled" => Some(RunStatus::Cancelled),
        _ => None,
    }
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> DbResult<Workspace> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let root: String = row.get(2)?;
    let canonical: String = row.get(3)?;
    let created: String = row.get(4)?;
    let updated: String = row.get(5)?;
    Ok(Workspace {
        id: parse_id(&id, "workspace")?,
        name,
        root: Path::new(&root).to_path_buf(),
        canonical_root: Path::new(&canonical).to_path_buf(),
        created_at: parse_ts(&created)?,
        updated_at: parse_ts(&updated)?,
    })
}

fn session_from_row(row: &rusqlite::Row<'_>) -> DbResult<Session> {
    let id: String = row.get(0)?;
    let workspace: Option<String> = row.get(1)?;
    let source: String = row.get(2)?;
    let external_key: Option<String> = row.get(3)?;
    let title: Option<String> = row.get(4)?;
    let created: String = row.get(5)?;
    let updated: String = row.get(6)?;
    Ok(Session {
        id: parse_id(&id, "session")?,
        workspace_id: match workspace {
            Some(raw) => Some(parse_id(&raw, "workspace")?),
            None => None,
        },
        source,
        external_key,
        title,
        created_at: parse_ts(&created)?,
        updated_at: parse_ts(&updated)?,
    })
}

/// SQL predicate over `messages` for the user message that opens a turn: the first user
/// message of its run. A steer delivered mid-run is a later user message of the same run and
/// belongs to that turn.
const TURN_START: &str = "role = 'user' AND (run_id IS NULL OR NOT EXISTS ( \
    SELECT 1 FROM messages o WHERE o.session_id = messages.session_id \
    AND o.run_id = messages.run_id AND o.role = 'user' \
    AND (o.created_at < messages.created_at \
         OR (o.created_at = messages.created_at AND o.id < messages.id))))";

fn message_from_row(row: &rusqlite::Row<'_>) -> DbResult<Message> {
    let id: String = row.get(0)?;
    let session: String = row.get(1)?;
    let run: Option<String> = row.get(2)?;
    let role: String = row.get(3)?;
    let content: String = row.get(4)?;
    let created: String = row.get(5)?;
    let parts: Vec<ContentPart> = serde_json::from_str(&content)?;
    Ok(Message {
        id: parse_id(&id, "message")?,
        session_id: parse_id(&session, "session")?,
        run_id: match run {
            Some(raw) => Some(parse_id(&raw, "run")?),
            None => None,
        },
        role: role_from(&role)
            .ok_or_else(|| DbError::Internal(format!("unknown message role {role:?}")))?,
        content: parts,
        created_at: parse_ts(&created)?,
    })
}

fn run_from_row(row: &rusqlite::Row<'_>) -> DbResult<Run> {
    let id: String = row.get(0)?;
    let session: String = row.get(1)?;
    let workspace: Option<String> = row.get(2)?;
    let status: String = row.get(3)?;
    let model: String = row.get(4)?;
    let created: String = row.get(5)?;
    let started: Option<String> = row.get(6)?;
    let finished: Option<String> = row.get(7)?;
    let error_code: Option<String> = row.get(8)?;
    let error_message: Option<String> = row.get(9)?;
    Ok(Run {
        id: parse_id(&id, "run")?,
        session_id: parse_id(&session, "session")?,
        workspace_id: match workspace {
            Some(raw) => Some(parse_id(&raw, "workspace")?),
            None => None,
        },
        status: run_status_from(&status)
            .ok_or_else(|| DbError::Internal(format!("unknown run status {status:?}")))?,
        model,
        created_at: parse_ts(&created)?,
        started_at: match started {
            Some(raw) => Some(parse_ts(&raw)?),
            None => None,
        },
        finished_at: match finished {
            Some(raw) => Some(parse_ts(&raw)?),
            None => None,
        },
        error_code,
        error_message,
    })
}

fn approval_from_row(row: &rusqlite::Row<'_>) -> DbResult<ApprovalRecord> {
    let id: String = row.get(0)?;
    let run: String = row.get(1)?;
    let tool: String = row.get(2)?;
    let hash: String = row.get(3)?;
    let status: String = row.get(4)?;
    let decided: Option<String> = row.get(5)?;
    let created: String = row.get(6)?;
    Ok(ApprovalRecord {
        id: parse_id(&id, "approval")?,
        run_id: parse_id(&run, "run")?,
        tool_call_id: parse_id(&tool, "tool call")?,
        arguments_hash: hash,
        status: ApprovalStatus::from_str(&status)
            .ok_or_else(|| DbError::Internal(format!("unknown approval status {status:?}")))?,
        decided_at: match decided {
            Some(raw) => Some(parse_ts(&raw)?),
            None => None,
        },
        created_at: parse_ts(&created)?,
    })
}

fn checkpoint_from_row(row: &rusqlite::Row<'_>) -> DbResult<Checkpoint> {
    let id: String = row.get(0)?;
    let session: String = row.get(1)?;
    let run: Option<String> = row.get(2)?;
    let path: String = row.get(3)?;
    let kind: String = row.get(4)?;
    let bytes: i64 = row.get(5)?;
    let created: String = row.get(6)?;
    let snapshot_dir: Option<String> = row.get(7)?;
    Ok(Checkpoint {
        id,
        session_id: parse_id(&session, "session")?,
        run_id: match run {
            Some(raw) => Some(parse_id(&raw, "run")?),
            None => None,
        },
        path,
        kind,
        bytes,
        created_at: parse_ts(&created)?,
        snapshot_dir,
    })
}

/// Lifecycle of one human approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    /// The text stored in the approvals table.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
        }
    }

    /// Parse the stored text back into a status.
    #[expect(
        clippy::should_implement_trait,
        reason = "kept as from_str for API clarity"
    )]
    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(ApprovalStatus::Pending),
            "approved" => Some(ApprovalStatus::Approved),
            "denied" => Some(ApprovalStatus::Denied),
            "expired" => Some(ApprovalStatus::Expired),
            _ => None,
        }
    }
}

/// One approval row, joined to the tool call it guards.
#[derive(Clone, Debug)]
pub struct ApprovalRecord {
    pub id: ApprovalId,
    pub run_id: RunId,
    pub tool_call_id: ToolCallId,
    pub arguments_hash: String,
    pub status: ApprovalStatus,
    pub decided_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// One run's persisted token counts.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RunUsage {
    pub run_id: RunId,
    pub model: String,
    /// How the run ended, so a reopened session still shows a stop.
    pub status: RunStatus,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Approximate USD list-price estimate, when the model is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Why the run failed, so a reopened session still shows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated token usage for one session plus its per-run breakdown.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionUsage {
    pub session_id: SessionId,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Sum of the per-run estimates, when at least one run reported a known model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub runs: Vec<RunUsage>,
    /// The context window as the session's latest model request filled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<silver_protocol::ContextUsage>,
}

/// One (model, UTC day) bucket of run usage over an insights window.
#[derive(Clone, Debug, serde::Serialize)]
pub struct InsightRow {
    pub model: String,
    /// UTC calendar day (YYYY-MM-DD) the runs were created on.
    pub day: String,
    pub run_count: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Sum of the per-run estimates, absent when no run reported a known price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Aggregated run usage over a trailing window, grouped by (model, UTC day).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Insights {
    pub days: u32,
    pub rows: Vec<InsightRow>,
}

/// The metadata columns stored alongside one message row. `finish_reason` and
/// `token_count` stay absent until a model response/per-message account is recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageMetadata {
    /// Name of the first tool call/result carried by the body.
    pub tool_name: Option<String>,
    /// Provider finish reason.
    pub finish_reason: Option<String>,
    /// Per-message token count.
    pub token_count: Option<i64>,
}

/// A bounded slice of one session's transcript plus the omission counts around it. The
/// messages are ordered oldest first.
#[derive(Clone, Debug)]
pub struct MessageWindow {
    pub messages: Vec<Message>,
    pub messages_before: usize,
    pub messages_after: usize,
}

/// One indexed filesystem checkpoint row. The snapshot_dir points at the shadow store
/// that holds the captured bytes; it is None when the checkpoint is only a marker.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub path: String,
    pub kind: String,
    /// Size of the captured payload in bytes.
    pub bytes: i64,
    pub created_at: DateTime<Utc>,
    pub snapshot_dir: Option<String>,
}

/// Outcome of rewinding N user turns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindOutcome {
    /// Newest removed user text (retry source when a single turn was undone).
    pub removed_user_text: Option<String>,
    /// How many user turns were actually removed.
    pub turns_undone: usize,
    /// How many message rows were deleted in total.
    pub rewound_count: usize,
    /// Whether any removed turn took file checkpoints, so its edits outlived the rewind.
    pub files_changed: bool,
}

/// A cheap cloneable handle to one SQLite connection driven by tokio-rusqlite.
#[derive(Clone)]
pub struct Db {
    conn: Arc<tokio_rusqlite::Connection>,
    /// Count of successful writes, used to trigger off-hot-path WAL maintenance.
    writes: Arc<AtomicU64>,
    /// Epoch seconds of the last heavy maintenance pass; zero until one runs.
    last_maintenance: Arc<AtomicU64>,
    /// Directory of the database file, or None when the path names no directory. Lets callers
    /// place daemon data (for example the ESTOP sentinel) beside the state file.
    data_dir: Option<PathBuf>,
}

impl Db {
    /// Open (creating parent directories) and configure a file-backed database.
    pub async fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                Self::ensure_private_dir(parent).await?;
            }
        }
        #[cfg(unix)]
        precreate_private_file(path);
        let conn = tokio_rusqlite::Connection::open(path)
            .await
            .map_err(DbError::from)?;
        let data_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf);
        let db = Self::initialize(conn, data_dir).await?;
        #[cfg(unix)]
        secure_state_files(path);
        Ok(db)
    }

    /// Create a data directory 0700 when this process is the one that creates it.
    async fn ensure_private_dir(dir: &Path) -> DbResult<()> {
        #[cfg(unix)]
        let existed = dir.exists();
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|err| DbError::Internal(format!("create {}: {err}", dir.display())))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            // create_dir_all honours the umask; tighten a directory we own.
            drop(tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await);
        }
        Ok(())
    }

    /// The directory holding the database file, or None when its path names no directory.
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    async fn initialize(
        conn: tokio_rusqlite::Connection,
        data_dir: Option<PathBuf>,
    ) -> DbResult<Self> {
        Self::run(&conn, |c| {
            c.execute_batch("PRAGMA foreign_keys = ON;")?;
            c.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
            configure_journal(c)?;
            Ok(())
        })
        .await?;
        Ok(Self {
            conn: Arc::new(conn),
            writes: Arc::new(AtomicU64::new(0)),
            last_maintenance: Arc::new(AtomicU64::new(0)),
            data_dir,
        })
    }

    /// Apply every embedded migration the database has not seen. Safe to call repeatedly: each
    /// runs at most once, atomically with its user_version stamp.
    pub async fn migrate(&self) -> DbResult<()> {
        self.call(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            let applied = usize::try_from(version).unwrap_or(0);
            for (index, script) in MIGRATIONS.iter().enumerate().skip(applied) {
                let script = format!(
                    "BEGIN;\n{script}\nPRAGMA user_version = {};\nCOMMIT;",
                    index + 1
                );
                conn.execute_batch(&script)?;
            }
            Ok(())
        })
        .await
    }

    async fn run<T, F>(conn: &tokio_rusqlite::Connection, function: F) -> DbResult<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        match conn.call(function).await {
            Ok(value) => Ok(value),
            Err(tokio_rusqlite::Error::ConnectionClosed) => {
                Err(DbError::Internal("sqlite connection is closed".into()))
            }
            Err(tokio_rusqlite::Error::Error(inner)) => Err(inner),
            // A close() failure still carries a rusqlite error; classify it rather than
            // formatting a message that could quote SQL.
            Err(tokio_rusqlite::Error::Close((_, inner))) => Err(DbError::from(inner)),
            // The enum is non_exhaustive; a future variant is an opaque internal failure.
            Err(_) => Err(DbError::Internal("sqlite error".into())),
        }
    }

    pub(crate) async fn call<T, F>(&self, function: F) -> DbResult<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        Self::run(&self.conn, function).await
    }

    /// Run a mutating statement and account for it. Counting on every successful write
    /// lets WAL maintenance run off the request path instead of on it.
    pub(crate) async fn write<T, F>(&self, function: F) -> DbResult<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let result = self.call(function).await;
        if result.is_ok() {
            self.note_write();
        }
        result
    }

    /// Record one write; every WRITE_MAINTENANCE_INTERVAL writes spawn a checkpoint.
    fn note_write(&self) {
        let count = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_multiple_of(WRITE_MAINTENANCE_INTERVAL) {
            let db = Db::clone(self);
            tokio::spawn(async move {
                drop(db.run_maintenance().await);
            });
        }
    }

    /// Checkpoint the WAL and refresh query-planner statistics, then run a throttled
    /// retention pass. Best effort: a failure (for example an in-memory database) must
    /// never surface to a caller.
    async fn run_maintenance(&self) -> DbResult<()> {
        self.call(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")?;
            Ok(())
        })
        .await?;
        if self.try_begin_maintenance(now_secs()) {
            self.maintenance_cycle().await;
        }
        Ok(())
    }

    /// Best-effort startup retention pass: sweep orphans, prune sessions past the age
    /// window, and reclaim free pages. Callers spawn it behind a timeout so the daemon
    /// starts serving immediately; it never panics and never returns an error.
    pub async fn run_startup_maintenance(&self) {
        if !self.try_begin_maintenance(now_secs()) {
            return;
        }
        self.maintenance_cycle().await;
    }

    /// Claim the heavy-maintenance slot when the throttle window has elapsed. The
    /// compare-and-swap keeps two spawned maintenance tasks from running a VACUUM at once.
    fn try_begin_maintenance(&self, now: u64) -> bool {
        let last = self.last_maintenance.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < AUTO_MAINTENANCE_THROTTLE_SECS {
            return false;
        }
        self.last_maintenance
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Sweep orphans, prune past retention, then vacuum. Every step is best effort: a
    /// failure in one must not stop the others or surface to a caller.
    async fn maintenance_cycle(&self) {
        drop(self.sweep_orphans().await);
        drop(
            self.prune_sessions(AUTO_PRUNE_KEEP_LAST, Some(AUTO_PRUNE_MAX_AGE_DAYS))
                .await,
        );
        drop(self.vacuum().await);
    }

    // -----------------------------------------------------------------------
    // workspaces
    // -----------------------------------------------------------------------

    /// Register a workspace. A duplicate name or canonical root is a Conflict.
    pub async fn create_workspace(&self, ws: Workspace) -> DbResult<Workspace> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO workspaces \
                 (id, name, root_path, canonical_root_path, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    ws.id.to_string(),
                    ws.name,
                    ws.root.to_string_lossy(),
                    ws.canonical_root.to_string_lossy(),
                    ts(ws.created_at),
                    ts(ws.updated_at)
                ],
            )
            .map_err(map_constraint)?;
            Ok(ws)
        })
        .await
    }

    /// Every registered workspace, oldest first.
    pub async fn list_workspaces(&self) -> DbResult<Vec<Workspace>> {
        self.call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, root_path, canonical_root_path, created_at, updated_at \
                 FROM workspaces ORDER BY created_at ASC, id ASC",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(workspace_from_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Look up one workspace by id.
    pub async fn get_workspace(&self, id: WorkspaceId) -> DbResult<Option<Workspace>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, root_path, canonical_root_path, created_at, updated_at \
                 FROM workspaces WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(workspace_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Look up one workspace by its unique name.
    pub async fn get_workspace_by_name(&self, name: &str) -> DbResult<Option<Workspace>> {
        let name = name.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, root_path, canonical_root_path, created_at, updated_at \
                 FROM workspaces WHERE name = ?1",
            )?;
            let mut rows = stmt.query(params![name])?;
            match rows.next()? {
                Some(row) => Ok(Some(workspace_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Remove a workspace row. Referencing sessions or runs keep it alive.
    /// Delete a workspace with every session, run and memory audit row it owns.
    pub async fn delete_workspace(&self, id: WorkspaceId) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let sessions: Vec<String> = tx
                .prepare("SELECT id FROM sessions WHERE workspace_id = ?1")?
                .query_map(params![id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for session in &sessions {
                delete_session_rows(&tx, session)?;
            }
            tx.execute(
                "DELETE FROM memory_changes WHERE workspace_id = ?1",
                params![id],
            )?;
            tx.execute(
                "UPDATE bots SET workspace_id = NULL WHERE workspace_id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM workspaces WHERE id = ?1", params![id])
                .map_err(map_constraint)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// True when a run in this workspace is still queued or running.
    pub async fn workspace_has_active_run(&self, id: WorkspaceId) -> DbResult<bool> {
        let id = id.to_string();
        self.call(move |conn| {
            let present: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE workspace_id = ?1 \
                 AND status IN ('queued', 'running', 'waiting_approval'))",
                params![id],
                |row| row.get(0),
            )?;
            Ok(present != 0)
        })
        .await
    }

    /// True when a workspace owns any session or run and therefore must not be dropped.
    pub async fn workspace_has_data(&self, id: WorkspaceId) -> DbResult<bool> {
        let id = id.to_string();
        self.call(move |conn| {
            let present: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE workspace_id = ?1) \
                 OR EXISTS(SELECT 1 FROM runs WHERE workspace_id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            Ok(present != 0)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // sessions
    // -----------------------------------------------------------------------

    /// Persist a session. External keys are unique per scope.
    pub async fn create_session(&self, s: Session) -> DbResult<Session> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO sessions \
                 (id, workspace_id, source, external_key, title, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    s.id.to_string(),
                    s.workspace_id.map(|w| w.to_string()),
                    s.source,
                    s.external_key,
                    s.title,
                    ts(s.created_at),
                    ts(s.updated_at)
                ],
            )
            .map_err(map_constraint)?;
            Ok(s)
        })
        .await
    }

    /// Look up one session by id.
    pub async fn get_session(&self, id: SessionId) -> DbResult<Option<Session>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, source, external_key, title, created_at, updated_at \
                 FROM sessions WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(session_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Find the session bound to one source and external key inside a scope. A None
    /// workspace means the global scope, never every workspace.
    pub async fn find_session_by_external_key(
        &self,
        source: &str,
        external_key: &str,
        workspace_id: Option<WorkspaceId>,
    ) -> DbResult<Option<Session>> {
        let source = source.to_string();
        let external = external_key.to_string();
        let workspace = workspace_id.map(|w| w.to_string());
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, source, external_key, title, created_at, updated_at \
                 FROM sessions \
                 WHERE source = ?1 AND external_key = ?2 AND workspace_id IS ?3 \
                 ORDER BY updated_at DESC, id DESC LIMIT 1",
            )?;
            let mut rows = stmt.query(params![source, external, workspace])?;
            match rows.next()? {
                Some(row) => Ok(Some(session_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// List sessions in one scope, newest first. A global listing only ever returns rows
    /// whose workspace is NULL; a None workspace with global false returns nothing.
    pub async fn list_sessions(
        &self,
        workspace_id: Option<WorkspaceId>,
        global: bool,
        limit: u32,
        cursor: Option<&str>,
    ) -> DbResult<Vec<Session>> {
        if !global && workspace_id.is_none() {
            return Ok(Vec::new());
        }
        let scope = if global {
            None
        } else {
            workspace_id.map(|w| w.to_string())
        };
        let (cursor_ts, cursor_id) = match cursor {
            Some(raw) => {
                let (stamp, id) = raw
                    .split_once('|')
                    .ok_or_else(|| DbError::Internal(format!("invalid session cursor {raw:?}")))?;
                (Some(ts(parse_ts(stamp)?)), Some(id.to_string()))
            }
            None => (None, None),
        };
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, source, external_key, title, created_at, updated_at \
                 FROM sessions \
                 WHERE workspace_id IS ?1 AND source != 'chat' \
                   AND (?2 IS NULL OR updated_at < ?2 OR (updated_at = ?2 AND id < ?3)) \
                 ORDER BY updated_at DESC, id DESC \
                 LIMIT ?4",
            )?;
            let mut rows = stmt.query(params![scope, cursor_ts, cursor_id, i64::from(limit)])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(session_from_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Remove a session with its messages, runs and run children in one transaction; memory-change
    /// rows survive with the run reference cleared.
    pub async fn delete_session(&self, id: SessionId) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            delete_session_rows(&tx, &id)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Remove every session of one source whose external key starts with `prefix`, with their
    /// runs: a deleted bot takes its chat sessions along.
    pub async fn delete_sessions_with_key_prefix(
        &self,
        source: &str,
        prefix: &str,
    ) -> DbResult<()> {
        let (source, pattern) = (source.to_string(), format!("{prefix}%"));
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let ids: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM sessions WHERE source = ?1 AND external_key LIKE ?2",
                )?;
                let rows = stmt.query_map(params![source, pattern], |row| row.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for id in &ids {
                delete_session_rows(&tx, id)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Prune sessions outside the retention window. The newest `keep_last` sessions are
    /// always kept, `max_age_days` optionally bounds inactivity, and a session with an
    /// active run is never pruned. Returns the number of sessions removed.
    pub async fn prune_sessions(
        &self,
        keep_last: usize,
        max_age_days: Option<u32>,
    ) -> DbResult<usize> {
        let keep = usize_to_i64(keep_last);
        let cutoff =
            max_age_days.map(|days| ts(Utc::now() - chrono::Duration::days(i64::from(days))));
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let ids = {
                let mut stmt = tx.prepare(
                    "SELECT s.id FROM sessions s \
                     WHERE NOT EXISTS ( \
                         SELECT 1 FROM ( \
                             SELECT id FROM sessions ORDER BY updated_at DESC, id DESC LIMIT ?1 \
                         ) keep WHERE keep.id = s.id \
                     ) \
                     AND (?2 IS NULL OR s.updated_at < ?2) \
                     AND NOT EXISTS ( \
                         SELECT 1 FROM runs r WHERE r.session_id = s.id \
                         AND r.status IN ('queued', 'running', 'waiting_approval') \
                     )",
                )?;
                let mut rows = stmt.query(params![keep, cutoff])?;
                let mut ids = Vec::new();
                while let Some(row) = rows.next()? {
                    ids.push(row.get::<_, String>(0)?);
                }
                ids
            };
            for id in &ids {
                delete_session_rows(&tx, id)?;
            }
            tx.commit()?;
            Ok(ids.len())
        })
        .await
    }

    /// Delete messages and runs whose session row no longer exists. Foreign keys normally
    /// prevent such rows, but a legacy or imported database can still carry them.
    /// Returns how many message and run rows were removed.
    pub async fn sweep_orphans(&self) -> DbResult<usize> {
        self.write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM run_events WHERE run_id IN ( \
                     SELECT id FROM runs WHERE session_id NOT IN (SELECT id FROM sessions))",
                [],
            )?;
            tx.execute(
                "DELETE FROM tool_calls WHERE run_id IN ( \
                     SELECT id FROM runs WHERE session_id NOT IN (SELECT id FROM sessions))",
                [],
            )?;
            tx.execute(
                "DELETE FROM approvals WHERE run_id IN ( \
                     SELECT id FROM runs WHERE session_id NOT IN (SELECT id FROM sessions))",
                [],
            )?;
            tx.execute(
                "UPDATE memory_changes SET run_id = NULL WHERE run_id IN ( \
                     SELECT id FROM runs WHERE session_id NOT IN (SELECT id FROM sessions))",
                [],
            )?;
            let messages = tx.execute(
                "DELETE FROM messages WHERE session_id NOT IN (SELECT id FROM sessions)",
                [],
            )?;
            let runs = tx.execute(
                "DELETE FROM runs WHERE session_id NOT IN (SELECT id FROM sessions)",
                [],
            )?;
            tx.commit()?;
            Ok(messages + runs)
        })
        .await
    }

    /// VACUUM only when there are free pages to reclaim, so a dense database is never
    /// rewritten for nothing. Returns whether a VACUUM ran.
    pub async fn vacuum(&self) -> DbResult<bool> {
        self.call(move |conn| {
            let free: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
            if free <= 0 {
                return Ok(false);
            }
            conn.execute_batch("VACUUM;")?;
            drop(conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;"));
            Ok(true)
        })
        .await
    }

    /// Bump the session updated_at used for ordering.
    pub async fn touch_session(&self, id: SessionId) -> DbResult<()> {
        let id = id.to_string();
        let now = ts(Utc::now());
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Set an automatic title from the opening user text when the session has none.
    /// A user-set title (or any existing non-empty title) is never overwritten.
    pub async fn ensure_session_title(&self, id: SessionId, first_user_text: &str) -> DbResult<()> {
        let Some(title) = derive_title(first_user_text) else {
            return Ok(());
        };
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET title = ?2 \
                 WHERE id = ?1 AND (title IS NULL OR TRIM(title) = '')",
                params![id, title],
            )?;
            Ok(())
        })
        .await
    }

    /// Set, or clear (None), a user-chosen session title. The edit bumps updated_at so
    /// the session's ordering reflects it.
    pub async fn set_session_title(&self, id: SessionId, title: Option<String>) -> DbResult<()> {
        let id = id.to_string();
        let updated = ts(Utc::now());
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, title, updated],
            )?;
            Ok(())
        })
        .await
    }

    /// Replace a blank or seed-derived title with a model-generated one; a title the user chose (or
    /// an earlier model title) is never overwritten.
    pub async fn upgrade_session_title(
        &self,
        id: SessionId,
        derived_seed: &str,
        title: &str,
    ) -> DbResult<()> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Ok(());
        }
        let derived = derive_title(derived_seed);
        let id = id.to_string();
        let updated = ts(Utc::now());
        self.write(move |conn| {
            match derived {
                Some(derived) => conn.execute(
                    "UPDATE sessions SET title = ?2, updated_at = ?3                      WHERE id = ?1 AND (title IS NULL OR TRIM(title) = '' OR title = ?4)",
                    params![id, title, updated, derived],
                )?,
                None => conn.execute(
                    "UPDATE sessions SET title = ?2, updated_at = ?3                      WHERE id = ?1 AND (title IS NULL OR TRIM(title) = '')",
                    params![id, title, updated],
                )?,
            };
            Ok(())
        })
        .await
    }

    /// Persist, or clear (None), the per-session model override and the provider it
    /// belongs to.
    pub async fn set_session_model_override(
        &self,
        id: SessionId,
        model: Option<String>,
        provider: Option<String>,
    ) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET model_override = ?2, provider_override = ?3 WHERE id = ?1",
                params![id, model, provider],
            )?;
            Ok(())
        })
        .await
    }

    /// The persisted, non-blank per-session model override, if any.
    pub async fn session_model_override(&self, id: SessionId) -> DbResult<Option<String>> {
        Ok(self.session_route(id).await?.map(|(model, _)| model))
    }

    /// The persisted model override and the provider it belongs to, when there is a model.
    pub async fn session_route(&self, id: SessionId) -> DbResult<Option<(String, Option<String>)>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn
                .prepare("SELECT model_override, provider_override FROM sessions WHERE id = ?1")?;
            let mut rows = stmt.query(params![id])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let model: Option<String> = row.get(0)?;
            let provider: Option<String> = row.get(1)?;
            Ok(model.filter(|model| !model.trim().is_empty()).map(|model| {
                let provider = provider.filter(|provider| !provider.trim().is_empty());
                (model, provider)
            }))
        })
        .await
    }

    /// Persist, or clear (None), the per-session reasoning effort.
    pub async fn set_session_reasoning_effort(
        &self,
        id: SessionId,
        effort: Option<String>,
    ) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET reasoning_effort = ?2 WHERE id = ?1",
                params![id, effort],
            )?;
            Ok(())
        })
        .await
    }

    /// The persisted, non-blank per-session reasoning effort, if any.
    pub async fn session_reasoning_effort(&self, id: SessionId) -> DbResult<Option<String>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare("SELECT reasoning_effort FROM sessions WHERE id = ?1")?;
            let mut rows = stmt.query(params![id])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let effort: Option<String> = row.get(0)?;
            Ok(effort.filter(|effort| !effort.trim().is_empty()))
        })
        .await
    }

    /// Reasoning efforts for a bounded page of sessions, keyed by session id. Sessions
    /// without a non-blank effort are omitted from the map.
    pub async fn session_reasoning_efforts(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, String>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, reasoning_effort FROM sessions WHERE id IN ({placeholders}) \
                 AND reasoning_effort IS NOT NULL AND TRIM(reasoning_effort) <> ''"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let effort: String = row.get(1)?;
                out.insert(parse_id(&session, "session")?, effort);
            }
            Ok(out)
        })
        .await
    }

    /// Model overrides for a bounded page of sessions, keyed by session id. Sessions
    /// without a non-blank override are omitted from the map.
    pub async fn session_model_overrides(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, String>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, model_override FROM sessions \
                 WHERE id IN ({placeholders}) \
                   AND model_override IS NOT NULL AND TRIM(model_override) <> ''"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let model: String = row.get(1)?;
                out.insert(parse_id(&session, "session")?, model);
            }
            Ok(out)
        })
        .await
    }

    /// Persist the per-session YOLO approval bypass.
    pub async fn set_session_yolo_mode(&self, id: SessionId, enabled: bool) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET yolo_mode = ?2 WHERE id = ?1",
                params![id, enabled as i64],
            )?;
            Ok(())
        })
        .await
    }

    /// Whether the per-session YOLO approval bypass is enabled. False for a missing row.
    pub async fn session_yolo_mode(&self, id: SessionId) -> DbResult<bool> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare("SELECT yolo_mode FROM sessions WHERE id = ?1")?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)? != 0),
                None => Ok(false),
            }
        })
        .await
    }

    /// Enter or leave plan mode. Entering while on, or leaving while off, changes nothing.
    pub async fn set_session_plan_mode(&self, id: SessionId, on: bool) -> DbResult<()> {
        let id = id.to_string();
        let sql = if on {
            "UPDATE sessions SET plan_mode = 'entered' \
             WHERE id = ?1 AND (plan_mode IS NULL OR plan_mode = 'exited')"
        } else {
            "UPDATE sessions SET plan_mode = 'exited' \
             WHERE id = ?1 AND plan_mode IN ('entered', 'active')"
        };
        self.write(move |conn| {
            conn.execute(sql, params![id])?;
            Ok(())
        })
        .await
    }

    /// Whether the session is in plan mode. False for a missing row.
    pub async fn session_plan_mode(&self, id: SessionId) -> DbResult<bool> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT 1 FROM sessions WHERE id = ?1 AND plan_mode IN ('entered', 'active')",
            )?;
            Ok(stmt.exists(params![id])?)
        })
        .await
    }

    /// The plan mode a run starts in. The session moves on to what its next run sees: a
    /// fresh entry becomes active and an exit is told once.
    pub async fn start_plan_run(&self, id: SessionId) -> DbResult<PlanMode> {
        let id = id.to_string();
        self.write(move |conn| {
            let mode: Option<String> = conn
                .query_row(
                    "SELECT plan_mode FROM sessions WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            conn.execute(
                "UPDATE sessions SET plan_mode = CASE plan_mode \
                 WHEN 'entered' THEN 'active' WHEN 'exited' THEN NULL ELSE plan_mode END \
                 WHERE id = ?1",
                params![id],
            )?;
            Ok(match mode.as_deref() {
                Some("entered") => PlanMode::Entered,
                Some("active") => PlanMode::Active,
                Some("exited") => PlanMode::Exited,
                _ => PlanMode::Off,
            })
        })
        .await
    }

    /// Enabled YOLO flags for a bounded page of sessions. Disabled rows are omitted.
    pub async fn session_yolo_modes(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, bool>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql =
                format!("SELECT id FROM sessions WHERE id IN ({placeholders}) AND yolo_mode <> 0");
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                out.insert(parse_id(&session, "session")?, true);
            }
            Ok(out)
        })
        .await
    }

    /// Every custom preset, ordered by name.
    pub async fn list_presets(&self) -> DbResult<Vec<Preset>> {
        self.call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, tools, skills FROM presets ORDER BY name COLLATE NOCASE",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let tools_raw: String = row.get(2)?;
                let skills_raw: String = row.get(3)?;
                let tools: Vec<String> = serde_json::from_str(&tools_raw)?;
                let skills = serde_json::from_str(&skills_raw)?;
                out.push(Preset {
                    id,
                    name,
                    tools,
                    skills,
                    builtin: false,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Insert or replace one custom preset.
    pub async fn save_preset(&self, preset: Preset) -> DbResult<Preset> {
        let tools = serde_json::to_string(&preset.tools)?;
        let skills = serde_json::to_string(&preset.skills)?;
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO presets (id, name, tools, skills) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(id) DO UPDATE SET name = ?2, tools = ?3, skills = ?4",
                params![preset.id, preset.name, tools, skills],
            )?;
            Ok(preset)
        })
        .await
    }

    /// Delete one preset; sessions on it move to Minimal in the same transaction.
    pub async fn delete_preset(&self, id: &str) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE sessions SET preset = NULL WHERE preset = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM presets WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Set, or clear (None), the preset a session runs with.
    pub async fn set_session_preset(&self, id: SessionId, preset: Option<String>) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET preset = ?2 WHERE id = ?1",
                params![id, preset],
            )?;
            Ok(())
        })
        .await
    }

    /// Presets for a bounded page of sessions, keyed by session id. Sessions on
    /// Minimal (NULL or blank) are omitted from the map.
    pub async fn session_presets(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, String>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, preset FROM sessions WHERE id IN ({placeholders}) \
                 AND preset IS NOT NULL AND TRIM(preset) <> ''"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let preset: String = row.get(1)?;
                out.insert(parse_id(&session, "session")?, preset);
            }
            Ok(out)
        })
        .await
    }

    /// Set, or clear (None), a session's /goal.
    pub async fn set_session_goal(
        &self,
        id: SessionId,
        goal: Option<&SessionGoal>,
    ) -> DbResult<()> {
        let id = id.to_string();
        let goal = goal.map(serde_json::to_string).transpose()?;
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET goal = ?2 WHERE id = ?1",
                params![id, goal],
            )?;
            Ok(())
        })
        .await
    }

    /// Goals for a bounded page of sessions. Sessions without one are omitted.
    pub async fn session_goals(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, SessionGoal>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, goal FROM sessions WHERE id IN ({placeholders}) AND goal IS NOT NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let goal: String = row.get(1)?;
                out.insert(parse_id(&session, "session")?, serde_json::from_str(&goal)?);
            }
            Ok(out)
        })
        .await
    }

    /// The non-terminal run of each session in a bounded page. Idle sessions are omitted.
    pub async fn session_active_runs(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, RunId>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT session_id, id FROM runs WHERE session_id IN ({placeholders}) \
                 AND status IN ('queued', 'running', 'waiting_approval')"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let run: String = row.get(1)?;
                out.insert(parse_id(&session, "session")?, parse_id(&run, "run")?);
            }
            Ok(out)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // messages
    // -----------------------------------------------------------------------

    /// Append a message. Its `text` column is the FTS projection: a tool row over
    /// `MAX_INDEXED_TOOL_TEXT_CHARS` is truncated with a marker, so huge payloads are not indexed.
    /// A bare tool result takes its `tool_name` from the tool_calls row.
    pub async fn append_message(&self, m: Message) -> DbResult<()> {
        let id = m.id.to_string();
        let session = m.session_id.to_string();
        let run = m.run_id.map(|r| r.to_string());
        let role = m.role.as_str().to_string();
        let content = serde_json::to_string(&m.content)?;
        let text = indexed_text(m.role, projection_text(&m.content));
        let (tool_name, tool_call_id) = tool_identity(m.content);
        let created = ts(m.created_at);
        self.write(move |conn| {
            let tool_name = match (tool_name, tool_call_id) {
                (Some(name), _) => Some(name),
                (None, Some(call_id)) => conn
                    .query_row(
                        "SELECT name FROM tool_calls WHERE id = ?1",
                        params![call_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?,
                (None, None) => None,
            };
            conn.execute(
                "INSERT INTO messages \
                 (id, session_id, run_id, role, content_json, text, tool_name, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, session, run, role, content, text, tool_name, created],
            )?;
            Ok(())
        })
        .await
    }

    /// Outcome of rewinding N user turns is [`RewindOutcome`].
    pub async fn rewind_last_turn(&self, session_id: SessionId) -> DbResult<Option<String>> {
        Ok(self
            .rewind_last_n_turns(session_id, 1)
            .await?
            .removed_user_text)
    }

    /// Remove the newest `n` user turns (at least 1) and everything after them, in one transaction
    /// (`/undo [N]`).
    pub async fn rewind_last_n_turns(
        &self,
        session_id: SessionId,
        n: usize,
    ) -> DbResult<RewindOutcome> {
        let n = n.max(1);
        let session = session_id.to_string();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let user_count: i64 = tx.query_row(
                &format!("SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND {TURN_START}"),
                params![session],
                |row| row.get(0),
            )?;
            if user_count == 0 {
                return Ok(RewindOutcome {
                    removed_user_text: None,
                    turns_undone: 0,
                    rewound_count: 0,
                    files_changed: false,
                });
            }
            let turns_undone = (n as i64).min(user_count) as usize;
            // Newest user text first (retry source), before the delete.
            let newest_text: Option<String> = tx
                .query_row(
                    &format!(
                        "SELECT content_json FROM messages WHERE session_id = ?1 AND {TURN_START} \
                         ORDER BY created_at DESC, id DESC LIMIT 1"
                    ),
                    params![session],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|json| plain_text(&json));
            // Target = oldest of the N newest user turns.
            let offset = (turns_undone as i64).saturating_sub(1);
            let target: Option<(String, String)> = tx
                .query_row(
                    &format!(
                        "SELECT id, created_at FROM messages WHERE session_id = ?1 AND {TURN_START} \
                         ORDER BY created_at DESC, id DESC LIMIT 1 OFFSET ?2"
                    ),
                    params![session, offset],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((target_id, created_at)) = target else {
                return Ok(RewindOutcome {
                    removed_user_text: None,
                    turns_undone: 0,
                    rewound_count: 0,
                    files_changed: false,
                });
            };
            // A checkpoint taken during a removed turn outlives it: the file change is on disk.
            let changed: i64 = tx.query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE session_id = ?1 AND created_at >= ?2",
                params![session, created_at],
                |row| row.get(0),
            )?;
            let rewound_count = tx.execute(
                "DELETE FROM messages WHERE session_id = ?1 \
                 AND (created_at > ?2 OR (created_at = ?2 AND id >= ?3))",
                params![session, created_at, target_id],
            )?;
            tx.commit()?;
            Ok(RewindOutcome {
                removed_user_text: newest_text,
                turns_undone,
                rewound_count,
                files_changed: changed > 0,
            })
        })
        .await
    }

    /// Remove the identified user message and every message that followed it, returning
    /// the removed user text. None unless the target exists and is a user message.
    pub async fn rewind_to_message(
        &self,
        session_id: SessionId,
        message_id: MessageId,
    ) -> DbResult<Option<String>> {
        self.rewind_from_user_message(session_id, Some(message_id))
            .await
    }

    async fn rewind_from_user_message(
        &self,
        session_id: SessionId,
        target: Option<MessageId>,
    ) -> DbResult<Option<String>> {
        let session = session_id.to_string();
        let target = target.map(|id| id.to_string());
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let row: Option<(String, String, String)> = match &target {
                Some(id) => tx
                    .query_row(
                        "SELECT id, created_at, content_json FROM messages \
                         WHERE session_id = ?1 AND id = ?2 AND role = 'user'",
                        params![session, id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?,
                None => tx
                    .query_row(
                        "SELECT id, created_at, content_json FROM messages \
                         WHERE session_id = ?1 AND role = 'user' \
                         ORDER BY created_at DESC, id DESC LIMIT 1",
                        params![session],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?,
            };
            let Some((target_id, created_at, content_json)) = row else {
                return Ok(None);
            };
            // The (created_at, id) key is the one every read path orders by, so an
            // explicit rewrite sees exactly the same suffix.
            tx.execute(
                "DELETE FROM messages WHERE session_id = ?1 \
                 AND (created_at > ?2 OR (created_at = ?2 AND id >= ?3))",
                params![session, created_at, target_id],
            )?;
            tx.commit()?;
            Ok(Some(plain_text(&content_json)))
        })
        .await
    }

    /// The newest limit messages for a session, returned oldest first. A limit of zero
    /// yields an empty history.
    pub async fn list_messages(&self, session_id: SessionId, limit: u32) -> DbResult<Vec<Message>> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, run_id, role, content_json, created_at \
                 FROM messages WHERE session_id = ?1 \
                 ORDER BY created_at DESC, id DESC LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![session, i64::from(limit)])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(message_from_row(row)?);
            }
            out.reverse();
            Ok(out)
        })
        .await
    }

    /// The newest messages of a session whose stored content fits in `max_bytes`, oldest
    /// first. The newest message is always included, however large.
    pub async fn recent_messages(
        &self,
        session_id: SessionId,
        max_bytes: u64,
    ) -> DbResult<Vec<Message>> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, run_id, role, content_json, created_at FROM ( \
                   SELECT *, SUM(length(content_json)) OVER \
                     (ORDER BY created_at DESC, id DESC) - length(content_json) AS newer \
                   FROM messages WHERE session_id = ?1) \
                 WHERE newer < ?2 ORDER BY created_at ASC, id ASC",
            )?;
            let mut rows = stmt.query(params![session, max_bytes as i64])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(message_from_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// A slice of a session's conversation plus omission counts: the newest `limit` messages, or
    /// `window` on each side of an anchor, answered with LIMITs and counts. The caller
    /// scope-checks.
    pub async fn message_window(
        &self,
        session_id: SessionId,
        around: Option<MessageId>,
        window: usize,
        limit: usize,
    ) -> DbResult<MessageWindow> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let Some(anchor) = around else {
                let total = count_messages(conn, &session)?;
                let messages = query_messages(
                    conn,
                    &format!(
                        "SELECT {MESSAGE_COLUMNS} FROM ( \
                             SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 \
                             AND {CONVERSATION} ORDER BY created_at DESC, id DESC LIMIT ?2 \
                         ) ORDER BY created_at ASC, id ASC"
                    ),
                    params![session, usize_to_i64(limit)],
                )?;
                return Ok(MessageWindow {
                    messages_before: total.saturating_sub(messages.len()),
                    messages,
                    messages_after: 0,
                });
            };
            let anchor = anchor.to_string();
            let position: Option<(String, String)> = conn
                .query_row(
                    "SELECT created_at, id FROM messages WHERE session_id = ?1 AND id = ?2",
                    params![session, anchor],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((anchor_ts, anchor_id)) = position else {
                return Ok(MessageWindow {
                    messages: Vec::new(),
                    messages_before: count_messages(conn, &session)?,
                    messages_after: 0,
                });
            };
            let half = usize_to_i64(window.max(1));
            let older_total = count_messages_before(conn, &session, &anchor_ts, &anchor_id)?;
            let newer_total = count_messages_after(conn, &session, &anchor_ts, &anchor_id)?;
            let mut older = query_messages(
                conn,
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND {CONVERSATION} \
                     AND (created_at < ?2 OR (created_at = ?2 AND id < ?3)) \
                     ORDER BY created_at DESC, id DESC LIMIT ?4"
                ),
                params![session, anchor_ts, anchor_id, half],
            )?;
            older.reverse();
            let newer = query_messages(
                conn,
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND {CONVERSATION} \
                     AND (created_at > ?2 OR (created_at = ?2 AND id > ?3)) \
                     ORDER BY created_at ASC, id ASC LIMIT ?4"
                ),
                params![session, anchor_ts, anchor_id, half],
            )?;
            let anchor_message = query_messages(
                conn,
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 AND id = ?2"
                ),
                params![session, anchor_id],
            )?
            .into_iter()
            .next();
            let older_len = older.len();
            let newer_len = newer.len();
            let mut messages = older;
            if let Some(message) = anchor_message {
                messages.push(message);
            }
            messages.extend(newer);
            Ok(MessageWindow {
                messages_before: older_total.saturating_sub(older_len),
                messages_after: newer_total.saturating_sub(newer_len),
                messages,
            })
        })
        .await
    }

    /// Metadata columns for the newest `limit` messages of a session, keyed by message
    /// id. The message view merges these into the rows it already returned, so the
    /// transcript query is not duplicated.
    pub async fn message_metadata(
        &self,
        session_id: SessionId,
        limit: u32,
    ) -> DbResult<HashMap<MessageId, MessageMetadata>> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, tool_name, finish_reason, token_count FROM ( \
                     SELECT id, tool_name, finish_reason, token_count FROM messages \
                     WHERE session_id = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2 \
                 )",
            )?;
            let mut rows = stmt.query(params![session, i64::from(limit)])?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                out.insert(
                    parse_id(&id, "message")?,
                    MessageMetadata {
                        tool_name: row.get(1)?,
                        finish_reason: row.get(2)?,
                        token_count: row.get(3)?,
                    },
                );
            }
            Ok(out)
        })
        .await
    }

    /// The oldest user message of a session, whitespace-collapsed and clipped; None when blank or
    /// absent.
    pub async fn session_preview(&self, session_id: SessionId) -> DbResult<Option<String>> {
        let mut previews = self.session_previews(&[session_id]).await?;
        Ok(previews.remove(&session_id))
    }

    /// Previews for a page of sessions in one query; sessions without one are left out.
    pub async fn session_previews(
        &self,
        session_ids: &[SessionId],
    ) -> DbResult<HashMap<SessionId, String>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        self.call(move |conn| {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT session_id, text FROM (                      SELECT session_id, text,                             ROW_NUMBER() OVER (                                 PARTITION BY session_id ORDER BY created_at ASC, id ASC                             ) AS rank                      FROM messages                      WHERE role = 'user' AND session_id IN ({placeholders})                  ) WHERE rank = 1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let text: String = row.get(1)?;
                if let Some(preview) = session_preview_from_text(&text) {
                    out.insert(parse_id(&session, "session")?, preview);
                }
            }
            Ok(out)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // runs
    // -----------------------------------------------------------------------

    /// Insert a run row without an idempotency key.
    pub async fn create_run(&self, r: Run) -> DbResult<Run> {
        self.create_run_with_key(r, None).await
    }

    /// Insert a run row, stamping an optional per-session idempotency key. A duplicate
    /// (session_id, idempotency_key) is a Conflict; a NULL key is always accepted.
    pub async fn create_run_with_key(
        &self,
        r: Run,
        idempotency_key: Option<&str>,
    ) -> DbResult<Run> {
        let key = idempotency_key.map(str::to_string);
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO runs \
                 (id, session_id, workspace_id, status, model, created_at, started_at, \
                  finished_at, error_code, error_message, idempotency_key) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    r.id.to_string(),
                    r.session_id.to_string(),
                    r.workspace_id.map(|w| w.to_string()),
                    run_status_str(r.status),
                    r.model,
                    ts(r.created_at),
                    r.started_at.map(ts),
                    r.finished_at.map(ts),
                    r.error_code,
                    r.error_message,
                    key
                ],
            )
            .map_err(map_constraint)?;
            Ok(r)
        })
        .await
    }

    /// Look up one run by id.
    pub async fn get_run(&self, id: RunId) -> DbResult<Option<Run>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, workspace_id, status, model, created_at, started_at, \
                        finished_at, error_code, error_message \
                 FROM runs WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(run_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Find the run reserved by an idempotency key inside one session, oldest first. The
    /// (session_id, idempotency_key) unique index makes this at most one row.
    pub async fn find_run_by_idempotency(
        &self,
        session_id: SessionId,
        idempotency_key: &str,
    ) -> DbResult<Option<Run>> {
        let session = session_id.to_string();
        let key = idempotency_key.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, workspace_id, status, model, created_at, started_at, \
                        finished_at, error_code, error_message \
                 FROM runs WHERE session_id = ?1 AND idempotency_key = ?2 \
                 ORDER BY created_at ASC, id ASC LIMIT 1",
            )?;
            let mut rows = stmt.query(params![session, key])?;
            match rows.next()? {
                Some(row) => Ok(Some(run_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Mark a queued run as running and stamp its start time.
    pub async fn start_run(&self, id: RunId, started_at: DateTime<Utc>) -> DbResult<()> {
        let id = id.to_string();
        let started = ts(started_at);
        self.write(move |conn| {
            conn.execute(
                "UPDATE runs SET status = 'running', started_at = ?2 WHERE id = ?1",
                params![id, started],
            )?;
            Ok(())
        })
        .await
    }

    /// Move a run to a terminal status and stamp its finish time.
    pub async fn finish_run(
        &self,
        id: RunId,
        status: RunStatus,
        error_code: Option<String>,
        error_message: Option<String>,
    ) -> DbResult<()> {
        let id = id.to_string();
        let status = run_status_str(status).to_string();
        let finished = ts(Utc::now());
        self.write(move |conn| {
            conn.execute(
                "UPDATE runs SET status = ?2, finished_at = ?3, error_code = ?4, \
                        error_message = ?5 WHERE id = ?1",
                params![id, status, finished, error_code, error_message],
            )?;
            Ok(())
        })
        .await
    }

    /// Change only the status of a run.
    pub async fn update_run_status(&self, id: RunId, status: RunStatus) -> DbResult<()> {
        let id = id.to_string();
        let status = run_status_str(status).to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE runs SET status = ?2 WHERE id = ?1",
                params![id, status],
            )?;
            Ok(())
        })
        .await
    }

    /// True when a session has a queued, running or waiting_approval run.
    pub async fn has_active_run(&self, session_id: SessionId) -> DbResult<bool> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let present: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id = ?1 \
                 AND status IN ('queued', 'running', 'waiting_approval'))",
                params![session],
                |row| row.get(0),
            )?;
            Ok(present != 0)
        })
        .await
    }

    /// Fail every run left non-terminal by a restart. Returns how many rows changed.
    pub async fn recover_interrupted_runs(&self) -> DbResult<usize> {
        let finished = ts(Utc::now());
        self.write(move |conn| {
            let changed = conn.execute(
                "UPDATE runs SET status = 'failed', error_code = 'daemon_restarted', \
                        finished_at = ?1 \
                 WHERE status IN ('queued', 'running', 'waiting_approval')",
                params![finished],
            )?;
            Ok(changed)
        })
        .await
    }

    /// Persist the token usage and approximate USD cost a completed run reported.
    pub async fn record_run_usage(
        &self,
        id: RunId,
        usage: TokenUsage,
        cost_usd: Option<f64>,
    ) -> DbResult<()> {
        let id = id.to_string();
        let prompt = usage.prompt_tokens as i64;
        let completion = usage.completion_tokens as i64;
        let total = usage.total_tokens as i64;
        self.write(move |conn| {
            conn.execute(
                "UPDATE runs SET prompt_tokens = ?2, completion_tokens = ?3, total_tokens = ?4, \
                        cost_usd = ?5 WHERE id = ?1",
                params![id, prompt, completion, total, cost_usd],
            )?;
            Ok(())
        })
        .await
    }

    /// Token usage for one session: the per-run rows plus their totals. Runs that never
    /// reported usage contribute zero.
    pub async fn session_usage(&self, session_id: SessionId) -> DbResult<SessionUsage> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, model, prompt_tokens, completion_tokens, total_tokens, cost_usd, \
                 error_message, status FROM runs WHERE session_id = ?1 ORDER BY created_at ASC, id ASC",
            )?;
            let mut rows = stmt.query(params![session])?;
            let mut usage = SessionUsage {
                session_id,
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cost_usd: None,
                runs: Vec::new(),
                context: None,
            };
            while let Some(row) = rows.next()? {
                let run: String = row.get(0)?;
                let model: String = row.get(1)?;
                let prompt = row.get::<_, i64>(2)?.max(0) as u64;
                let completion = row.get::<_, i64>(3)?.max(0) as u64;
                let total = row.get::<_, i64>(4)?.max(0) as u64;
                let cost: Option<f64> = row.get(5)?;
                usage.prompt_tokens += prompt;
                usage.completion_tokens += completion;
                usage.total_tokens += total;
                if let Some(cost) = cost {
                    usage.cost_usd = Some(usage.cost_usd.unwrap_or(0.0) + cost);
                }
                usage.runs.push(RunUsage {
                    run_id: parse_id(&run, "run")?,
                    model,
                    status: run_status_from(&row.get::<_, String>(7)?)
                        .unwrap_or(RunStatus::Completed),
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: total,
                    cost_usd: cost,
                    error: row.get(6)?,
                });
            }
            let latest: Option<String> = conn
                .query_row(
                    "SELECT e.payload_json FROM run_events e JOIN runs r ON r.id = e.run_id \
                     WHERE r.session_id = ?1 AND e.event_type = 'context.updated' \
                     ORDER BY r.created_at DESC, e.sequence DESC LIMIT 1",
                    params![session],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(EventPayload::ContextUpdated { context }) =
                latest.and_then(|json| serde_json::from_str(&json).ok())
            {
                usage.context = Some(context);
            }
            Ok(usage)
        })
        .await
    }

    /// Runs of the last `days` days by (model, UTC day). The day is the first ten characters of the
    /// stored RFC3339 timestamp, which is always UTC.
    pub async fn insights(&self, days: u32) -> DbResult<Insights> {
        let days = days.min(MAX_INSIGHTS_DAYS);
        let cutoff = ts(Utc::now() - chrono::Duration::days(i64::from(days)));
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT model, substr(created_at, 1, 10) AS day, COUNT(*), \
                        COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0), \
                        COALESCE(SUM(total_tokens), 0), SUM(cost_usd) \
                 FROM runs WHERE created_at >= ?1 \
                 GROUP BY model, day \
                 ORDER BY day DESC, model ASC",
            )?;
            let mut rows = stmt.query(params![cutoff])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(InsightRow {
                    model: row.get(0)?,
                    day: row.get(1)?,
                    run_count: row.get::<_, i64>(2)?.max(0) as u64,
                    prompt_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                    completion_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                    total_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                    cost_usd: row.get(6)?,
                });
            }
            Ok(Insights { days, rows: out })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // checkpoints and spend
    // -----------------------------------------------------------------------

    /// Record one filesystem checkpoint and return the stored row, including the
    /// generated id and creation time. The bytes argument is the size of the captured
    /// payload and snapshot_dir is the shadow-store location, or None for a marker.
    pub async fn record_checkpoint(
        &self,
        session_id: SessionId,
        run_id: Option<RunId>,
        path: &str,
        kind: &str,
        bytes: i64,
        snapshot_dir: Option<&str>,
    ) -> DbResult<Checkpoint> {
        let checkpoint = Checkpoint {
            id: format!("ckpt_{}", uuid::Uuid::now_v7().simple()),
            session_id,
            run_id,
            path: path.to_string(),
            kind: kind.to_string(),
            bytes,
            created_at: Utc::now(),
            snapshot_dir: snapshot_dir.map(str::to_string),
        };
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO checkpoints \
                 (id, session_id, run_id, path, kind, bytes, created_at, snapshot_dir) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    checkpoint.id,
                    checkpoint.session_id.to_string(),
                    checkpoint.run_id.map(|id| id.to_string()),
                    checkpoint.path,
                    checkpoint.kind,
                    bytes,
                    ts(checkpoint.created_at),
                    checkpoint.snapshot_dir
                ],
            )?;
            Ok(checkpoint)
        })
        .await
    }

    /// The newest limit checkpoints for one session, newest first.
    pub async fn list_checkpoints(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> DbResult<Vec<Checkpoint>> {
        let session = session_id.to_string();
        let limit = usize_to_i64(limit);
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, run_id, path, kind, bytes, created_at, snapshot_dir \
                 FROM checkpoints WHERE session_id = ?1 \
                 ORDER BY created_at DESC, rowid DESC LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![session, limit])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(checkpoint_from_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Look up one checkpoint by id.
    pub async fn get_checkpoint(&self, id: &str) -> DbResult<Option<Checkpoint>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, run_id, path, kind, bytes, created_at, snapshot_dir \
                 FROM checkpoints WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(checkpoint_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Keep only the newest keep_last checkpoints across all sessions, deleting the
    /// rest. A keep_last of 0 clears the table. Returns the number of rows removed.
    pub async fn prune_checkpoints(&self, keep_last: usize) -> DbResult<usize> {
        let keep = usize_to_i64(keep_last);
        self.write(move |conn| {
            let removed = conn.execute(
                "DELETE FROM checkpoints WHERE id NOT IN ( \
                     SELECT id FROM checkpoints ORDER BY created_at DESC, rowid DESC LIMIT ?1)",
                params![keep],
            )?;
            Ok(removed)
        })
        .await
    }

    /// Record one advisory USD list-price spend observation against a run and session.
    /// The value is never billing-accurate.
    pub async fn record_spend(
        &self,
        run_id: RunId,
        session_id: SessionId,
        model: &str,
        cost_usd: f64,
    ) -> DbResult<()> {
        let id = format!("spend_{}", uuid::Uuid::now_v7().simple());
        let run = run_id.to_string();
        let session = session_id.to_string();
        let model = model.to_string();
        let created = ts(Utc::now());
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO spend_events (id, run_id, session_id, model, cost_usd, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, run, session, model, cost_usd, created],
            )?;
            Ok(())
        })
        .await
    }

    /// Total recorded spend (USD) on or after the first instant of the given YYYY-MM-DD
    /// UTC calendar day. Returns 0.0 when nothing matches.
    pub async fn spend_since(&self, iso_day: &str) -> DbResult<f64> {
        let day = iso_day.to_string();
        self.call(move |conn| {
            let total: f64 = conn.query_row(
                "SELECT COALESCE(SUM(cost_usd), 0.0) FROM spend_events WHERE created_at >= ?1",
                params![day],
                |row| row.get(0),
            )?;
            Ok(total)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // events
    // -----------------------------------------------------------------------

    /// Persist one replayable run event. Text deltas, tool output and heartbeats are
    /// intentionally dropped.
    pub async fn append_event(&self, ev: &RunEvent) -> DbResult<()> {
        if !ev.payload.is_replayable() {
            return Ok(());
        }
        let run = ev.run_id.to_string();
        let sequence = ev.event_id.0 as i64;
        let event_type = ev.payload.name().to_string();
        let payload = serde_json::to_string(&ev.payload)?;
        let created = ts(ev.created_at);
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO run_events (run_id, sequence, event_type, payload_json, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run, sequence, event_type, payload, created],
            )?;
            Ok(())
        })
        .await
    }

    /// Replay events with a sequence strictly greater than after, in order.
    pub async fn list_events_after(&self, run_id: RunId, after: u64) -> DbResult<Vec<RunEvent>> {
        let run = run_id.to_string();
        let after = after as i64;
        self.call(move |conn| {
            let run_id: RunId = parse_id(&run, "run")?;
            let mut stmt = conn.prepare(
                "SELECT sequence, payload_json, created_at FROM run_events \
                 WHERE run_id = ?1 AND sequence > ?2 ORDER BY sequence ASC",
            )?;
            let mut rows = stmt.query(params![run, after])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let sequence: i64 = row.get(0)?;
                let payload: String = row.get(1)?;
                let created: String = row.get(2)?;
                out.push(RunEvent {
                    run_id,
                    event_id: EventId(sequence as u64),
                    created_at: parse_ts(&created)?,
                    payload: serde_json::from_str::<EventPayload>(&payload)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// What the loop added to the model's context across every run of a session, oldest
    /// first: the advisor's checks and the injected text.
    pub async fn list_session_timeline(&self, session_id: SessionId) -> DbResult<Vec<RunEvent>> {
        let session = session_id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT e.run_id, e.sequence, e.payload_json, e.created_at FROM run_events e \
                 JOIN runs r ON r.id = e.run_id \
                 WHERE r.session_id = ?1 \
                 AND e.event_type IN ('advisor.checked', 'context.injected', 'subagent.started',
                                      'subagent.step', 'subagent.completed') \
                 ORDER BY e.created_at ASC, e.sequence ASC",
            )?;
            let mut rows = stmt.query(params![session])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let run: String = row.get(0)?;
                let sequence: i64 = row.get(1)?;
                let payload: String = row.get(2)?;
                let created: String = row.get(3)?;
                out.push(RunEvent {
                    run_id: parse_id(&run, "run")?,
                    event_id: EventId(sequence as u64),
                    created_at: parse_ts(&created)?,
                    payload: serde_json::from_str::<EventPayload>(&payload)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The highest stored sequence for a run, or zero when it has no events.
    pub async fn latest_event_seq(&self, run_id: RunId) -> DbResult<u64> {
        let run = run_id.to_string();
        self.call(move |conn| {
            let sequence: i64 = conn.query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM run_events WHERE run_id = ?1",
                params![run],
                |row| row.get(0),
            )?;
            Ok(sequence.max(0) as u64)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // approvals
    // -----------------------------------------------------------------------

    /// Insert a pending approval. The referenced run and tool call must exist.
    pub async fn create_approval(&self, a: ApprovalRecord) -> DbResult<ApprovalRecord> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO approvals \
                 (id, run_id, tool_call_id, arguments_hash, status, decided_at, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    a.id.to_string(),
                    a.run_id.to_string(),
                    a.tool_call_id.to_string(),
                    a.arguments_hash,
                    a.status.as_str(),
                    a.decided_at.map(ts),
                    ts(a.created_at)
                ],
            )
            .map_err(map_constraint)?;
            Ok(a)
        })
        .await
    }

    /// Look up one approval by id.
    pub async fn get_approval(&self, id: ApprovalId) -> DbResult<Option<ApprovalRecord>> {
        let id = id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, run_id, tool_call_id, arguments_hash, status, decided_at, created_at \
                 FROM approvals WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(approval_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Record a decision on an approval.
    pub async fn resolve_approval(
        &self,
        id: ApprovalId,
        status: ApprovalStatus,
        decided_at: DateTime<Utc>,
    ) -> DbResult<()> {
        let id = id.to_string();
        let status = status.as_str().to_string();
        let decided = ts(decided_at);
        self.write(move |conn| {
            conn.execute(
                "UPDATE approvals SET status = ?2, decided_at = ?3 WHERE id = ?1",
                params![id, status, decided],
            )?;
            Ok(())
        })
        .await
    }

    /// The oldest still-pending approval for a run, if any.
    pub async fn get_pending_approval(&self, run_id: RunId) -> DbResult<Option<ApprovalRecord>> {
        let run = run_id.to_string();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, run_id, tool_call_id, arguments_hash, status, decided_at, created_at \
                 FROM approvals WHERE run_id = ?1 AND status = 'pending' \
                 ORDER BY created_at ASC, id ASC LIMIT 1",
            )?;
            let mut rows = stmt.query(params![run])?;
            match rows.next()? {
                Some(row) => Ok(Some(approval_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    // -----------------------------------------------------------------------
    // search
    // -----------------------------------------------------------------------

    /// Search user and assistant messages in one scope (the list_sessions rule), ranked by BM25;
    /// unicode61, then trigram, then an escaped LIKE scan, each only when the last found nothing.
    /// Tool results are left out: mostly file contents, they buried the conversation.
    pub async fn search_messages(
        &self,
        workspace_id: Option<WorkspaceId>,
        global: bool,
        query: &str,
        limit: u32,
    ) -> DbResult<Vec<SessionSearchHit>> {
        if !global && workspace_id.is_none() {
            return Ok(Vec::new());
        }
        let scope = if global {
            None
        } else {
            workspace_id.map(|w| w.to_string())
        };
        let needle = query.to_string();
        let words = query_words(query);
        self.call(move |conn| {
            let routes = [
                (FTS_TABLE, fts_match_query(&words)),
                (FTS_TRIGRAM_TABLE, trigram_match_query(&words)),
            ];
            for (table, match_query) in routes {
                let Some(match_query) = match_query else {
                    continue;
                };
                match fts_rows(conn, table, &scope, &match_query, limit) {
                    Ok(rows) if !rows.is_empty() => return rows_to_hits(rows, &words),
                    Ok(_) => {}
                    Err(err) if is_fts_error(&err) => {}
                    Err(err) => return Err(DbError::from(err)),
                }
            }
            like_search(conn, &scope, &needle, &words, limit)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // documents
    // -----------------------------------------------------------------------

    /// Store a document and its chunks, replacing any earlier copy whose size and mtime
    /// already match (an unchanged file is not re-read into the index). A changed file
    /// replaces its row, so a hit never names text the file no longer holds.
    pub async fn index_document(
        &self,
        workspace_id: WorkspaceId,
        document: DocumentInput,
    ) -> DbResult<()> {
        let workspace = workspace_id.to_string();
        let indexed_at = ts(Utc::now());
        self.write(move |conn| {
            let unchanged: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM documents WHERE workspace_id = ?1 AND path = ?2 \
                     AND bytes = ?3 AND mtime = ?4",
                    params![
                        workspace,
                        document.path,
                        document.bytes as i64,
                        document.mtime
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if unchanged.is_some() {
                return Ok(());
            }
            let document_id = format!("doc_{}", uuid::Uuid::now_v7().simple());
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM documents WHERE workspace_id = ?1 AND path = ?2",
                params![workspace, document.path],
            )?;
            tx.execute(
                "INSERT INTO documents \
                 (id, workspace_id, path, bytes, mtime, indexed_at, text) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    document_id,
                    workspace,
                    document.path,
                    document.bytes as i64,
                    document.mtime,
                    indexed_at,
                    document.text,
                ],
            )?;
            {
                let mut statement = tx.prepare(
                    "INSERT INTO document_chunks (id, document_id, ordinal, text) \
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (ordinal, text) in chunk_text(&document.text).into_iter().enumerate() {
                    statement.execute(params![
                        format!("dck_{}", uuid::Uuid::now_v7().simple()),
                        document_id,
                        ordinal as i64,
                        text,
                    ])?;
                }
            }
            Ok(tx.commit()?)
        })
        .await
    }

    /// Indexed document chunks matching `query` in one workspace, best first. unicode61,
    /// then trigram, then an escaped LIKE scan, each only when the last found nothing.
    pub async fn search_documents(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: u32,
    ) -> DbResult<Vec<DocumentHit>> {
        let workspace = workspace_id.to_string();
        let words = query_words(query);
        self.call(move |conn| {
            for (table, match_query) in [
                (DOC_FTS_TABLE, fts_match_query(&words)),
                (DOC_FTS_TRIGRAM_TABLE, trigram_match_query(&words)),
            ] {
                let Some(match_query) = match_query else {
                    continue;
                };
                match document_rows(conn, table, &workspace, &match_query, limit) {
                    Ok(rows) if !rows.is_empty() => return Ok(document_hits(rows, &words)),
                    Ok(_) => {}
                    // A build without the trigram tokenizer falls through to the LIKE route
                    // rather than failing the tool.
                    Err(err) if is_fts_error(&err) => {}
                    Err(err) => return Err(DbError::from(err)),
                }
            }
            Ok(Vec::new())
        })
        .await
    }
}

/// The text window one chunk holds, in characters. Big enough that a section survives whole,
/// small enough that a hit's snippet is most of the context around the match.
const DOCUMENT_CHUNK_CHARS: usize = 2_000;

/// Split extracted text into chunks on line boundaries, so a chunk starts at its own heading
/// and a hit reads as a section rather than a fragment.
fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut width = 0usize;
    for line in text.lines() {
        if width >= DOCUMENT_CHUNK_CHARS {
            chunks.push(std::mem::take(&mut current));
            width = 0;
        }
        if width > 0 {
            current.push('\n');
            width += 1;
        }
        current.push_str(line);
        width += line.chars().count();
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

/// One raw document row: (path, ordinal, chunk text).
type RawDocument = (String, i64, String);

fn document_hits(rows: Vec<RawDocument>, words: &[String]) -> Vec<DocumentHit> {
    rows.into_iter()
        .map(|(path, ordinal, text)| DocumentHit {
            path,
            ordinal: ordinal.max(0) as usize,
            snippet: snippet(&text, words),
        })
        .collect()
}

/// The two document indexes, mirroring the message pair: words first, then substrings.
const DOC_FTS_TABLE: &str = "document_fts";
const DOC_FTS_TRIGRAM_TABLE: &str = "document_fts_trigram";

/// Run the FTS5 MATCH against one document index, ordered by bm25. The table name is always
/// one of the two module constants, never caller input.
fn document_rows(
    conn: &rusqlite::Connection,
    table: &str,
    workspace: &str,
    match_query: &str,
    limit: u32,
) -> Result<Vec<RawDocument>, rusqlite::Error> {
    let sql = format!(
        "SELECT d.path, c.ordinal, c.text \
         FROM {table} JOIN document_chunks c ON c.rowid = {table}.rowid \
         JOIN documents d ON d.id = c.document_id \
         WHERE {table} MATCH ?1 AND d.workspace_id = ?2 \
         ORDER BY bm25({table}), d.path, c.ordinal LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![match_query, workspace, i64::from(limit)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?, row.get(2)?));
    }
    Ok(out)
}

/// One raw search row: (session id, message id, title, plain text, created_at, role).
type RawHit = (String, String, Option<String>, String, String, String);

/// Turn raw SQL rows into search hits, extracting a snippet from the plain-text column.
fn rows_to_hits(rows: Vec<RawHit>, words: &[String]) -> DbResult<Vec<SessionSearchHit>> {
    rows.into_iter()
        .map(|(session, message, title, text, created, role)| {
            Ok(SessionSearchHit {
                session_id: parse_id(&session, "session")?,
                message_id: parse_id(&message, "message")?,
                title,
                snippet: snippet(&text, words),
                created_at: parse_ts(&created)?,
                role: role_from(&role),
            })
        })
        .collect()
}

/// The query's words, lowercased and cut to five characters: a crude stem that lets
/// "conditions" find "condition" and "métodos" find "método" in any language. Splitting
/// on everything but letters and digits also keeps FTS syntax out of the MATCH.
fn query_words(query: &str) -> Vec<String> {
    const MAX_QUERY_CHARS: usize = 512;
    const STEM_CHARS: usize = 5;
    let truncated: String = query.chars().take(MAX_QUERY_CHARS).collect();
    truncated
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .map(|word| word.to_lowercase().chars().take(STEM_CHARS).collect())
        .collect()
}

/// The unicode61 MATCH: any word, as a prefix. None when no word remains.
fn fts_match_query(words: &[String]) -> Option<String> {
    let terms: Vec<String> = words.iter().map(|word| format!("\"{word}\"*")).collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// The trigram MATCH: any word of three or more characters, as a substring. A shorter word
/// emits no trigrams, so it is left out.
fn trigram_match_query(words: &[String]) -> Option<String> {
    let terms: Vec<String> = words
        .iter()
        .filter(|word| word.chars().count() >= 3)
        .map(|word| format!("\"{word}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// True when a SQLite failure came from an FTS index rather than the canonical tables.
/// A missing trigram tokenizer ("no such tokenizer: trigram") must fall through to the
/// LIKE route rather than surface as a search failure.
fn is_fts_error(err: &rusqlite::Error) -> bool {
    let text = err.to_string().to_lowercase();
    text.contains("fts5") || text.contains("message_fts") || text.contains("trigram")
}

/// The unicode61 index over the plain-text projection.
const FTS_TABLE: &str = "message_fts";

/// The trigram index over the same projection, for substring/CJK recall.
const FTS_TRIGRAM_TABLE: &str = "message_fts_trigram";

/// Run the FTS5 MATCH against one index, ordered by bm25 (best match first). The table
/// name is always one of the two module constants, never caller input.
fn fts_rows(
    conn: &rusqlite::Connection,
    table: &str,
    scope: &Option<String>,
    match_query: &str,
    limit: u32,
) -> Result<Vec<RawHit>, rusqlite::Error> {
    let sql = format!(
        "SELECT m.session_id, m.id, s.title, m.text, m.created_at, m.role \
         FROM {table} JOIN messages m ON m.rowid = {table}.rowid \
         JOIN sessions s ON s.id = m.session_id \
         WHERE {table} MATCH ?1 AND s.workspace_id IS ?2 AND m.role IN ('user', 'assistant') \
         ORDER BY bm25({table}), m.created_at DESC, m.id DESC LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![match_query, scope, i64::from(limit)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ));
    }
    Ok(out)
}

/// Escape LIKE wildcards so a user query is matched literally.
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escaped LIKE scan over the canonical messages, used when neither FTS index can
/// answer. It reads the plain-text projection, never the JSON scaffolding: matching
/// `content_json` would make the literal word "text" a hit on every message.
fn like_search(
    conn: &rusqlite::Connection,
    scope: &Option<String>,
    needle: &str,
    words: &[String],
    limit: u32,
) -> DbResult<Vec<SessionSearchHit>> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let pattern = format!("%{}%", escape_like(needle));
    let mut stmt = conn.prepare(
        r"SELECT m.session_id, m.id, s.title, m.text, m.created_at, m.role FROM messages m JOIN sessions s ON s.id = m.session_id WHERE m.text LIKE ?1 ESCAPE '\' AND s.workspace_id IS ?2 AND m.role IN ('user', 'assistant') ORDER BY m.created_at DESC, m.id DESC LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![pattern, scope, i64::from(limit)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ));
    }
    rows_to_hits(out, words)
}

/// Deterministic session title from the opening user text. The first non-empty line is
/// stripped of control/invisible characters and markdown markers, whitespace-collapsed
/// and truncated on a word boundary with an ellipsis. No model is involved.
pub fn derive_title(first_user_text: &str) -> Option<String> {
    let line = first_meaningful_line(first_user_text);
    if line.is_empty() {
        return None;
    }
    let cleaned = strip_title_markdown(&sanitize_title_chars(&line));
    let collapsed = collapse_whitespace(&cleaned);
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() > MAX_DERIVED_TITLE_CHARS {
        let cut: String = collapsed.chars().take(MAX_DERIVED_TITLE_CHARS).collect();
        let base = match cut.rfind(' ') {
            Some(space) if space > MAX_DERIVED_TITLE_CHARS / 2 => cut[..space].to_string(),
            _ => cut,
        };
        let base = base.trim_end_matches([' ', ',', '.', ';', ':', '—', '-']);
        return Some(format!("{base}…"));
    }
    Some(collapsed)
}

/// The first line that is not blank after trimming.
fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_attachment_marker(line))
        .unwrap_or("")
        .to_string()
}

/// Drop ASCII controls and zero-width/bidi/object-replacement characters. Horizontal
/// whitespace is kept for the later collapse pass.
fn sanitize_title_chars(line: &str) -> String {
    line.chars()
        .filter_map(|ch| {
            if (ch.is_control() && !matches!(ch, '\t' | '\n' | '\r')) || is_invisible_title_char(ch)
            {
                None
            } else {
                Some(if ch == '\t' { ' ' } else { ch })
            }
        })
        .collect()
}

/// Zero-width, bidi and replacement characters that must never reach a title.
fn is_invisible_title_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
            | '\u{fffc}'
            | '\u{fff9}'..='\u{fffb}'
    )
}

/// Remove leading markdown structure markers and inline code/emphasis noise.
fn strip_title_markdown(text: &str) -> String {
    let mut current = text.trim();
    loop {
        let trimmed = current.trim_start();
        let mut advanced = false;
        for marker in ["#", ">"] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                if rest.starts_with(' ') {
                    current = rest.trim_start();
                    advanced = true;
                    break;
                }
            }
        }
        if !advanced {
            for marker in ["- ", "* ", "+ "] {
                if let Some(rest) = trimmed.strip_prefix(marker) {
                    current = rest.trim_start();
                    advanced = true;
                    break;
                }
            }
        }
        if !advanced {
            let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                if let Some(rest) = trimmed[digits..].strip_prefix(". ") {
                    current = rest.trim_start();
                    advanced = true;
                }
            }
        }
        if !advanced {
            break;
        }
    }
    current
        .replace('`', "")
        .replace("**", "")
        .replace("__", "")
        .trim()
        .to_string()
}

/// Collapse every run of whitespace to a single space.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse a stored message to one line and clip it for a session list row.
///
/// Returns None when the message has no visible text at all.
fn session_preview_from_text(text: &str) -> Option<String> {
    let collapsed = collapse_whitespace(text);
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= MAX_SESSION_PREVIEW_CHARS {
        return Some(collapsed);
    }
    let mut out: String = collapsed
        .chars()
        .take(MAX_SESSION_PREVIEW_CHARS - 1)
        .collect();
    out.push('…');
    Some(out)
}

/// Extract the searchable text of a serialized message body. Used by the LIKE fallback.
fn plain_text(content_json: &str) -> String {
    match serde_json::from_str::<Vec<ContentPart>>(content_json) {
        Ok(parts) => {
            let text = projection_text(&parts);
            if text.is_empty() {
                content_json.to_string()
            } else {
                text
            }
        }
        Err(_) => content_json.to_string(),
    }
}

/// The composer's attachment marker: one text part naming a stored file. It is a path for the
/// model to act on, not something a person wrote, so it stays out of titles, previews and the
/// index. Keep this in step with MARKER in apps/web/src/lib/state.svelte.js.
fn is_attachment_marker(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("[attached: ") && text.ends_with(']') && text.contains(" → ")
}

/// Plain-text projection indexed by FTS5: human-visible text plus tool content. The JSON
/// scaffolding of content_json (the literal word "text", "type", argument keys) is
/// deliberately never indexed.
fn projection_text(parts: &[ContentPart]) -> String {
    let mut pieces: Vec<Cow<str>> = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } if !is_attachment_marker(text) => pieces.push(text.into()),
            ContentPart::Text { .. } => {}
            ContentPart::ToolCall {
                name, arguments, ..
            } => {
                pieces.push(format!("{name} {arguments}").into());
            }
            ContentPart::ToolResult { content, .. } => pieces.push(content.into()),
            ContentPart::Reasoning { .. }
            | ContentPart::Image { .. }
            | ContentPart::Attachment { .. } => {}
        }
    }
    pieces.join("\n")
}

/// The projection actually stored in `messages.text`: a tool row over the projection
/// limit is truncated with a marker, so a multi-megabyte payload is not fully indexed.
fn indexed_text(role: MessageRole, projection: String) -> String {
    if role == MessageRole::Tool && projection.chars().count() > MAX_INDEXED_TOOL_TEXT_CHARS {
        let prefix: String = projection
            .chars()
            .take(MAX_INDEXED_TOOL_TEXT_CHARS)
            .collect();
        return format!("{prefix}{TRUNCATED_TEXT_MARKER}");
    }
    projection
}

/// The first tool identity in a message body: the declared call name when the body
/// carries a tool call, otherwise the call id from a tool result so the append path can
/// resolve the name from the tool_calls row.
fn tool_identity(parts: Vec<ContentPart>) -> (Option<String>, Option<ToolCallId>) {
    for part in parts {
        match part {
            ContentPart::ToolCall { name, .. } => return (Some(name), None),
            ContentPart::ToolResult { tool_call_id, .. } => return (None, Some(tool_call_id)),
            _ => {}
        }
    }
    (None, None)
}

/// A short window around the first query word in the text, safe for byte-indexed slicing.
fn snippet(text: &str, words: &[String]) -> String {
    const WINDOW: usize = 300;
    const LEAD: usize = 80;
    let haystack = text.to_lowercase();
    let first = words.iter().filter_map(|word| haystack.find(word)).min();
    let mut start = first.map_or(0, |index| index.saturating_sub(LEAD));
    start = start.min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + WINDOW).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.push_str(text[start..end].trim());
    if end < text.len() {
        out.push_str("...");
    }
    out
}

/// Delete one session's messages, runs and run children, then the session row. The
/// caller owns the transaction: either the whole transcript goes or none of it does.
fn delete_session_rows(conn: &rusqlite::Connection, session_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM messages WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM run_events WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM tool_calls WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM approvals WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        params![session_id],
    )?;
    conn.execute(
        "UPDATE memory_changes SET run_id = NULL \
         WHERE run_id IN (SELECT id FROM runs WHERE session_id = ?1)",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM runs WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
    Ok(())
}

/// One raw message row, in the column order every message query selects.
const MESSAGE_COLUMNS: &str = "id, session_id, run_id, role, content_json, created_at";

/// Run a message query and decode every returned row.
fn query_messages<P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
) -> DbResult<Vec<Message>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(message_from_row(row)?);
    }
    Ok(out)
}

/// The messages a session_search read shows: user and assistant turns with visible text.
/// Tool results and tool-call-only steps would fill a small model's window with nothing.
const CONVERSATION: &str =
    "role IN ('user', 'assistant') AND content_json LIKE '%\"type\":\"text\"%'";

/// Count the conversation messages in one session.
fn count_messages(conn: &rusqlite::Connection, session: &str) -> DbResult<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND {CONVERSATION}"),
        params![session],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

/// Count messages ordered strictly before the (created_at, id) key.
fn count_messages_before(
    conn: &rusqlite::Connection,
    session: &str,
    created_at: &str,
    id: &str,
) -> DbResult<usize> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND {CONVERSATION} \
             AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))"
        ),
        params![session, created_at, id],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

/// Count messages ordered strictly after the (created_at, id) key.
fn count_messages_after(
    conn: &rusqlite::Connection,
    session: &str,
    created_at: &str,
    id: &str,
) -> DbResult<usize> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND {CONVERSATION} \
             AND (created_at > ?2 OR (created_at = ?2 AND id > ?3))"
        ),
        params![session, created_at, id],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}
