//! Active run registry, cancellation, approvals and event publishing.

use crate::ai_memory::{MemoryHooks, RunCapture};
use crate::approval_memory::ApprovalMemory;
use crate::config::{ApprovalMode, Config};
use crate::db::{ApprovalRecord, ApprovalStatus, Db, DbError};
use chrono::Utc;
use futures::StreamExt;
use silver_core::agent::{
    Agent, ApprovalGate, ApprovalOutcome, ApprovalRequest, RunControl, TranscriptSink, TurnOutcome,
};
use silver_core::context::{load_project_instructions, RunContext, DEFAULT_BASE_PROMPT};
use silver_core::error::{CoreError, CoreResult};
use silver_core::event::EventEmitter;
use silver_core::guard::cost::{CostGuard, CostGuardConfig};
use silver_core::model::{Model, ModelMessage, ModelRequest, ModelStreamEvent};
use silver_core::plan::Plan;
use silver_core::services::ToolServices;
use silver_core::session::{Message, Run, Session};
use silver_core::workspace::Workspace;
use silver_protocol::{
    ApprovalDecision, ApprovalId, CreateRunRequest, EventId, EventPayload, GoalStatus, GoalUpdate,
    MessageInput, MessageRole, Preset, RunCreatedResponse, RunId, RunStatus, RunView, Scope,
    SessionGoal, SessionId, WorkspaceId,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct ApprovalRegistry {
    pending: Mutex<HashMap<ApprovalId, PendingApproval>>,
}

struct PendingApproval {
    sender: oneshot::Sender<ApprovalOutcome>,
    run_id: RunId,
}

impl ApprovalRegistry {
    fn register(&self, id: ApprovalId, run_id: RunId) -> oneshot::Receiver<ApprovalOutcome> {
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .expect("approval lock")
            .insert(id, PendingApproval { sender, run_id });
        receiver
    }

    /// Resolve a pending approval, rejecting one that belongs to another run.
    pub fn resolve_for_run(&self, run_id: RunId, id: ApprovalId, outcome: ApprovalOutcome) -> bool {
        let mut map = self.pending.lock().expect("approval lock");
        match map.get(&id) {
            Some(pending) if pending.run_id == run_id => {}
            _ => return false,
        }
        if let Some(pending) = map.remove(&id) {
            drop(pending.sender.send(outcome));
            true
        } else {
            false
        }
    }

    /// The approvals a run is waiting on, in any order. Parallel subagents share the parent's
    /// gate, so a run can have several pending at once.
    pub fn pending(&self, run_id: RunId) -> Vec<ApprovalId> {
        self.pending
            .lock()
            .expect("approval lock")
            .iter()
            .filter(|(_, pending)| pending.run_id == run_id)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn cancel_run(&self, run_id: RunId) {
        self.pending
            .lock()
            .expect("approval lock")
            .retain(|_, pending| pending.run_id != run_id);
    }

    /// Drop one pending entry. Used once a request leaves the waiting state so a timed-out
    /// or cancelled approval does not linger until the whole run ends.
    fn remove(&self, id: ApprovalId) {
        self.pending.lock().expect("approval lock").remove(&id);
    }
}

/// The approval window when none is configured, matching Hermes' approvals.timeout default.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Name of the persistent emergency-stop sentinel inside the daemon data directory.
pub const ESTOP_FILE: &str = "ESTOP";

/// How much of a session's transcript is replayed into the next run. Well above the largest
/// compaction budget, so compaction (not a row count) decides what the model stops seeing.
const REPLAY_HISTORY_BYTES: u64 = 8 * 1024 * 1024;

pub struct ChannelApprovalGate {
    registry: Arc<ApprovalRegistry>,
    memory: Arc<ApprovalMemory>,
    db: Db,
    /// How long a request waits for a decision before it fails closed as a timeout.
    timeout: Duration,
}

impl ChannelApprovalGate {
    /// Build a gate with the default approval window (DEFAULT_APPROVAL_TIMEOUT).
    pub fn new(registry: Arc<ApprovalRegistry>, memory: Arc<ApprovalMemory>, db: Db) -> Self {
        Self::with_timeout(registry, memory, db, DEFAULT_APPROVAL_TIMEOUT)
    }

    /// Build a gate with an explicit approval window. A zero timeout still yields an
    /// immediate ApprovalOutcome::Timeout rather than blocking.
    pub fn with_timeout(
        registry: Arc<ApprovalRegistry>,
        memory: Arc<ApprovalMemory>,
        db: Db,
        timeout: Duration,
    ) -> Self {
        Self {
            registry,
            memory,
            db,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalGate for ChannelApprovalGate {
    async fn request(
        &self,
        request: ApprovalRequest<'_>,
        cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        let receiver = self.registry.register(request.approval_id, request.run_id);
        let arguments_hash = silver_core::hash::content_hash(
            &silver_core::guard::tool_guardrails::canonical_tool_args(request.arguments),
        );
        let record = ApprovalRecord {
            id: request.approval_id,
            run_id: request.run_id,
            tool_call_id: silver_protocol::ToolCallId::clone(request.tool_call_id),
            arguments_hash,
            status: ApprovalStatus::Pending,
            decided_at: None,
            created_at: Utc::now(),
        };
        drop(self.db.create_approval(record).await);

        let wait = async {
            tokio::select! {
                resolved = receiver => resolved.unwrap_or(ApprovalOutcome::Cancelled),
                _ = cancel.cancelled() => ApprovalOutcome::Cancelled,
            }
        };
        // Cancellation wins the select immediately; the deadline is the outer bound for a
        // request nobody answers, which the caller lets fail closed as a deny.
        let outcome = match tokio::time::timeout(self.timeout, wait).await {
            Ok(outcome) => outcome,
            Err(_) => ApprovalOutcome::Timeout,
        };
        self.registry.remove(request.approval_id);

        // Only an explicit "session" or "always" choice is remembered; denials, timeouts
        // and cancellation leave no trace.
        match outcome {
            ApprovalOutcome::ApprovedSession => self.memory.approve_session(
                request.session_id,
                request.tool_name,
                request.arguments,
            ),
            ApprovalOutcome::ApprovedAlways => self.memory.approve_always(
                request.scope,
                request.session_id,
                request.tool_name,
                request.arguments,
            ),
            _ => {}
        }

        let status = match outcome {
            ApprovalOutcome::Approved
            | ApprovalOutcome::ApprovedSession
            | ApprovalOutcome::ApprovedAlways
            | ApprovalOutcome::Answered(_) => ApprovalStatus::Approved,
            ApprovalOutcome::Denied => ApprovalStatus::Denied,
            ApprovalOutcome::Timeout | ApprovalOutcome::Cancelled => ApprovalStatus::Expired,
        };
        drop(
            self.db
                .resolve_approval(request.approval_id, status, Utc::now())
                .await,
        );
        outcome
    }

    fn remembered(&self, request: &ApprovalRequest<'_>) -> bool {
        self.memory.remembered(
            request.scope,
            request.session_id,
            request.tool_name,
            request.arguments,
        )
    }
}

/// Shared, mutable approval policy: the global mode, the process-start YOLO pin and the
/// per-session YOLO cache the gate consults.
pub struct ApprovalControl {
    db: Db,
    registry: Arc<ApprovalRegistry>,
    memory: Arc<ApprovalMemory>,
    timeout: Duration,
    aux_model: Option<Arc<dyn Model>>,
    mode: std::sync::RwLock<ApprovalMode>,
    frozen: bool,
    config_path: Option<PathBuf>,
    session_yolo: std::sync::RwLock<HashSet<SessionId>>,
}

impl ApprovalControl {
    /// Build the control. A frozen process pins the mode to off for its whole lifetime.
    pub fn new(
        config: &Arc<Config>,
        db: Db,
        registry: Arc<ApprovalRegistry>,
        memory: Arc<ApprovalMemory>,
        aux_model: Option<Arc<dyn Model>>,
        frozen: bool,
        config_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let timeout = Duration::from_secs(config.tools.approval_timeout_seconds.max(1));
        let mode = if frozen {
            ApprovalMode::Off
        } else {
            config.tools.approval_mode
        };
        Arc::new(Self {
            db,
            registry,
            memory,
            timeout,
            aux_model,
            mode: std::sync::RwLock::new(mode),
            frozen,
            config_path,
            session_yolo: std::sync::RwLock::new(HashSet::new()),
        })
    }

    /// The effective global mode as a wire value.
    pub fn mode(&self) -> silver_protocol::ApprovalMode {
        wire_mode(*self.mode.read().expect("approval mode lock"))
    }

    /// Whether --yolo / SILVER_YOLO_MODE pinned the mode for this process.
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// The status payload served by GET /v1/approvals.
    pub fn status(&self) -> silver_protocol::ApprovalStatus {
        silver_protocol::ApprovalStatus {
            mode: self.mode(),
            frozen: self.frozen,
        }
    }

    /// Persist a new global mode. Refused while the process is pinned to off.
    pub fn set_mode(&self, mode: silver_protocol::ApprovalMode) -> anyhow::Result<()> {
        if self.frozen {
            anyhow::bail!(
                "approval mode is pinned by --yolo / SILVER_YOLO_MODE; restart without it to change modes"
            );
        }
        let next = config_mode(mode);
        if let Some(path) = &self.config_path {
            persist_approval_mode(path, mode)?;
        }
        *self.mode.write().expect("approval mode lock") = next;
        Ok(())
    }

    /// Persist the per-session YOLO bypass and update the cache the gate reads.
    pub async fn set_session_yolo(&self, session_id: SessionId, enabled: bool) -> CoreResult<()> {
        self.db.set_session_yolo_mode(session_id, enabled).await?;
        let mut cache = self.session_yolo.write().expect("session yolo lock");
        if enabled {
            cache.insert(session_id);
        } else {
            cache.remove(&session_id);
        }
        Ok(())
    }

    /// Reload one session's YOLO flag from the database into the in-memory cache.
    pub async fn refresh_session_yolo(&self, session_id: SessionId) {
        let enabled = self.db.session_yolo_mode(session_id).await.unwrap_or(false);
        let mut cache = self.session_yolo.write().expect("session yolo lock");
        if enabled {
            cache.insert(session_id);
        } else {
            cache.remove(&session_id);
        }
    }

    fn session_yolo_cached(&self, session_id: SessionId) -> bool {
        self.session_yolo
            .read()
            .expect("session yolo lock")
            .contains(&session_id)
    }

    /// The mode that applies to one request: the frozen pin and the per-session bypass both
    /// force off, otherwise the global mode stands.
    fn effective_mode(&self, session_id: SessionId) -> ApprovalMode {
        if self.frozen || self.session_yolo_cached(session_id) {
            return ApprovalMode::Off;
        }
        *self.mode.read().expect("approval mode lock")
    }

    /// A manual gate sharing this control's registry, memory and timeout.
    fn manual_gate(&self) -> ChannelApprovalGate {
        ChannelApprovalGate::with_timeout(
            Arc::clone(&self.registry),
            Arc::clone(&self.memory),
            Db::clone(&self.db),
            self.timeout,
        )
    }

    /// Ask the auxiliary model whether a gated call is safe to auto-approve. A missing
    /// model, a failure or any ambiguity means no.
    async fn smart_approves(&self, request: &ApprovalRequest<'_>) -> bool {
        let Some(aux) = self.aux_model.as_ref() else {
            return false;
        };
        let payload = serde_json::json!({
            "tool": request.tool_name,
            "risk": serde_json::to_value(request.risk).unwrap_or(serde_json::Value::Null),
            "description": request.description,
            "arguments": request.arguments,
        });
        let model_request = ModelRequest {
            model: String::new(),
            messages: vec![
                ModelMessage::system(SMART_APPROVAL_SYSTEM_PROMPT),
                ModelMessage::user(payload.to_string()),
            ],
            tools: Vec::new(),
            temperature: Some(0.0),
            max_tokens: Some(SMART_APPROVAL_MAX_TOKENS),
            cache_key: None,
            reasoning_effort: None,
            run_id: None,
            workspace: None,
        };
        match call_aux_text(aux, model_request, SMART_APPROVAL_TIMEOUT).await {
            Some(text) => parse_smart_decision(&text),
            None => false,
        }
    }
}

/// Patch only tools.approval_mode in the on-disk configuration.
fn persist_approval_mode(
    path: &std::path::Path,
    mode: silver_protocol::ApprovalMode,
) -> anyhow::Result<()> {
    let mode_str = match mode {
        silver_protocol::ApprovalMode::Manual => "manual",
        silver_protocol::ApprovalMode::Smart => "smart",
        silver_protocol::ApprovalMode::Off => "off",
    };
    crate::config::persist_setting(path, "tools", "approval_mode", mode_str.into())
}

/// Convert the config mode to its wire form.
fn wire_mode(mode: ApprovalMode) -> silver_protocol::ApprovalMode {
    match mode {
        ApprovalMode::Manual => silver_protocol::ApprovalMode::Manual,
        ApprovalMode::Smart => silver_protocol::ApprovalMode::Smart,
        ApprovalMode::Off => silver_protocol::ApprovalMode::Off,
    }
}

/// Convert a wire mode back to the config form.
fn config_mode(mode: silver_protocol::ApprovalMode) -> ApprovalMode {
    match mode {
        silver_protocol::ApprovalMode::Manual => ApprovalMode::Manual,
        silver_protocol::ApprovalMode::Smart => ApprovalMode::Smart,
        silver_protocol::ApprovalMode::Off => ApprovalMode::Off,
    }
}

/// System prompt for the smart-mode reviewer.
const SMART_APPROVAL_SYSTEM_PROMPT: &str = r#"You are a security reviewer for a coding agent. Decide whether the proposed tool call is safe to run WITHOUT asking the user. Approve only clearly low-risk, reversible actions. Reject anything that deletes or overwrites data, touches credentials or secrets, changes permissions, installs software, pushes to a remote, spends money, or reaches the network destructively. Reply with strict JSON only: {"approve": true} or {"approve": false}."#;

/// Hard bound on smart-mode review latency.
const SMART_APPROVAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Token cap for the smart-mode reviewer.
const SMART_APPROVAL_MAX_TOKENS: u32 = 64;

/// Drive one auxiliary model request to completion, returning its trimmed text or None.
async fn call_aux_text(
    model: &Arc<dyn Model>,
    request: ModelRequest,
    timeout: Duration,
) -> Option<String> {
    let cancel = CancellationToken::new();
    let collect = async {
        let mut stream = model.stream(request, cancel).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let ModelStreamEvent::TextDelta(delta) = item? {
                text.push_str(&delta);
            }
        }
        Ok::<String, CoreError>(text)
    };
    match tokio::time::timeout(timeout, collect).await {
        Ok(Ok(text)) => {
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Ok(Err(err)) => {
            tracing::warn!(
                error_code = err.code().as_str(),
                "smart approval review failed; falling back to a prompt"
            );
            None
        }
        Err(_) => {
            tracing::warn!("smart approval review timed out; falling back to a prompt");
            None
        }
    }
}

/// Parse the reviewer's decision. Only an explicit true approves.
fn parse_smart_decision(text: &str) -> bool {
    fn approve(value: &serde_json::Value) -> bool {
        value
            .get("approve")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return approve(&value);
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end > start {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
                return approve(&value);
            }
        }
    }
    false
}

/// Approval gate that resolves the effective mode per request. The frozen pin and the
/// per-session YOLO bypass force off; smart mode consults the auxiliary reviewer and
/// otherwise falls back to the manual channel gate.
pub struct ControlApprovalGate {
    control: Arc<ApprovalControl>,
}

impl ControlApprovalGate {
    /// Wrap a control.
    pub fn new(control: Arc<ApprovalControl>) -> Self {
        Self { control }
    }
}

#[async_trait::async_trait]
impl ApprovalGate for ControlApprovalGate {
    async fn request(
        &self,
        request: ApprovalRequest<'_>,
        cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        match self.control.effective_mode(request.session_id) {
            ApprovalMode::Off => ApprovalOutcome::Approved,
            ApprovalMode::Manual => self.control.manual_gate().request(request, cancel).await,
            ApprovalMode::Smart => {
                if self.control.smart_approves(&request).await {
                    ApprovalOutcome::Approved
                } else {
                    self.control.manual_gate().request(request, cancel).await
                }
            }
        }
    }

    /// Plans and questions wait for the user whatever the approval mode.
    async fn ask_user(
        &self,
        request: ApprovalRequest<'_>,
        cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        self.control.manual_gate().request(request, cancel).await
    }

    fn remembered(&self, request: &ApprovalRequest<'_>) -> bool {
        match self.control.effective_mode(request.session_id) {
            ApprovalMode::Off => true,
            _ => self.control.manual_gate().remembered(request),
        }
    }
}

struct DbTranscript {
    db: Db,
    /// Plain text of the run input, offered as the session title once the opening user
    /// message lands. None when the caller supplies no seed.
    title_seed: Option<String>,
    titled: std::sync::atomic::AtomicBool,
}

impl DbTranscript {
    fn new(db: Db, title_seed: Option<String>) -> Self {
        Self {
            db,
            title_seed,
            titled: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl TranscriptSink for DbTranscript {
    async fn append(&self, message: Message) -> CoreResult<()> {
        let (session_id, role) = (message.session_id, message.role);
        self.db
            .append_message(message)
            .await
            .map_err(|err| CoreError::Internal(err.to_string()))?;
        self.db
            .touch_session(session_id)
            .await
            .map_err(|err| CoreError::Internal(err.to_string()))?;
        // The opening user row is the run input. Title an untitled session from it; a
        // user-set title is protected inside ensure_session_title.
        if role == MessageRole::User
            && !self.titled.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            if let Some(seed) = &self.title_seed {
                drop(self.db.ensure_session_title(session_id, seed).await);
            }
        }
        Ok(())
    }

    async fn set_model_title(&self, session_id: SessionId, title: &str) -> CoreResult<()> {
        let seed = self.title_seed.as_deref().unwrap_or_default();
        self.db
            .upgrade_session_title(session_id, seed, title)
            .await
            .map_err(|err| CoreError::Internal(err.to_string()))
    }
}

struct ActiveRun {
    events: broadcast::Sender<silver_protocol::RunEvent>,
    control: RunControl,
}

/// A subscriber's view of one run: buffered replay followed by live events.
pub struct EventSubscription {
    run_id: RunId,
    replay: VecDeque<silver_protocol::RunEvent>,
    live: Option<broadcast::Receiver<silver_protocol::RunEvent>>,
    last_seq: u64,
}

impl EventSubscription {
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub async fn next(&mut self) -> Option<silver_protocol::RunEvent> {
        if let Some(event) = self.replay.pop_front() {
            self.last_seq = event.event_id.0;
            return Some(event);
        }
        let receiver = self.live.as_mut()?;
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if event.event_id.0 <= self.last_seq {
                        continue;
                    }
                    self.last_seq = event.event_id.0;
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let event = silver_protocol::RunEvent {
                        run_id: self.run_id,
                        event_id: EventId(self.last_seq),
                        created_at: Utc::now(),
                        payload: EventPayload::ReplayGap {
                            from_event_id: EventId(self.last_seq),
                        },
                    };
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Wiring for the daemon-side approval policy.
#[derive(Default)]
pub struct ApprovalSetup {
    /// Auxiliary model used by smart mode. None disables auto-approval.
    pub aux_model: Option<Arc<dyn Model>>,
    /// Process-start YOLO pin (--yolo / SILVER_YOLO_MODE=1).
    pub frozen: bool,
    /// Configuration file a runtime mode change is persisted to.
    pub config_path: Option<PathBuf>,
}

pub struct RunManager {
    db: Db,
    agent: Arc<Agent>,
    config: Arc<Config>,
    /// models.dev registry resolver, so a run clamps its reasoning effort to the model's
    /// supported levels. None when the daemon has no resolver.
    context_resolver: Option<Arc<crate::context_length::ModelContextResolver>>,
    /// Opt-in spend guard: refuses a run before it starts and accumulates the
    /// completed run's cost.
    cost_guard: CostGuard,
    registry: Arc<ApprovalRegistry>,
    approval_memory: Arc<ApprovalMemory>,
    gate: Arc<dyn ApprovalGate>,
    /// Mutable approval policy behind the gate (mode, --yolo pin, per-session yolo).
    approval: Arc<ApprovalControl>,
    active: RwLock<HashMap<RunId, ActiveRun>>,
    session_locks: tokio::sync::Mutex<HashSet<SessionId>>,
    permits: Semaphore,
    event_lock: tokio::sync::Mutex<()>,
    services: ToolServices,
    /// Path of the persistent emergency-stop sentinel. While it exists no NEW run is
    /// admitted; in-flight runs are untouched.
    estop_path: PathBuf,
    /// Where each session's plan file lives, beside the sentinel.
    plans_dir: PathBuf,
    /// Credential store, when the daemon has one. It decides the default model: a provider
    /// signed in through `/login` serves its own model, not config.toml's.
    auth: std::sync::OnceLock<Arc<crate::auth::AuthStore>>,
    /// Where runs on a native provider report to ai-memory, when memory is on.
    memory: std::sync::OnceLock<MemoryHooks>,
}

/// Opens every /goal continuation run.
const GOAL_PREFIX: &str = "[GOAL CONTINUATION] keep working toward: ";

/// Continuations a goal gets when the request names none (0).
const DEFAULT_GOAL_BUDGET: u32 = 20;

fn goal_budget(requested: u32) -> u32 {
    if requested == 0 {
        DEFAULT_GOAL_BUDGET
    } else {
        requested
    }
}

/// Build the session goal a request asks for, refusing a budget with no objective.
fn run_goal(req: &CreateRunRequest) -> CoreResult<Option<SessionGoal>> {
    match req.goal_budget {
        Some(_) if req.message.plain_text().trim().is_empty() => Err(CoreError::InvalidRequest(
            "a goal needs an objective: /goal <what to work toward>".into(),
        )),
        Some(budget) => Ok(Some(SessionGoal {
            objective: req.message.plain_text().trim().to_string(),
            status: GoalStatus::Active,
            used: 0,
            max: goal_budget(budget),
        })),
        None => Ok(None),
    }
}

/// A run admitted to the queue and prepared for its background task.
struct RunTask {
    run_id: RunId,
    session_id: SessionId,
    session: Session,
    workspace_id: Option<WorkspaceId>,
    workspace: Option<Workspace>,
    model: String,
    provider: String,
    agent: Arc<Agent>,
    services: ToolServices,
    plan: Plan,
    input: MessageInput,
    /// Ambient information a client wants in the run's grounding, injected into the prompt.
    external_context: Option<String>,
    broadcast_tx: tokio::sync::broadcast::Sender<silver_protocol::RunEvent>,
    control: RunControl,
}

/// A resolved session and its request-named setup, before the run row exists.
struct Admission {
    session: Session,
    workspace: Option<Workspace>,
    run_effort: Option<String>,
    run_preset: Option<Preset>,
}

/// The outcome of inserting a run row.
enum CreatedRun {
    New(Run),
    Replay(RunCreatedResponse),
}

impl RunManager {
    pub fn new(
        db: Db,
        agent: Arc<Agent>,
        config: Arc<Config>,
        context_resolver: Option<Arc<crate::context_length::ModelContextResolver>>,
        services: ToolServices,
        approval_setup: ApprovalSetup,
    ) -> Arc<Self> {
        let registry = Arc::new(ApprovalRegistry::default());
        let approval_memory = Arc::new(ApprovalMemory::load(&config.data_dir()));
        let approval = ApprovalControl::new(
            &config,
            Db::clone(&db),
            Arc::clone(&registry),
            Arc::clone(&approval_memory),
            approval_setup.aux_model,
            approval_setup.frozen,
            approval_setup.config_path,
        );
        let gate: Arc<dyn ApprovalGate> = Arc::new(ControlApprovalGate::new(Arc::clone(&approval)));
        // The sentinel lives beside the state database: an explicit data directory wins,
        // otherwise the database directory, then the platform default. In production all
        // three resolve to the same directory.
        let (estop_path, plans_dir) = {
            let dir = match config.data.directory.as_deref().or_else(|| db.data_dir()) {
                Some(dir) => Cow::Borrowed(dir),
                None => Cow::Owned(config.data_dir()),
            };
            (dir.join(ESTOP_FILE), dir.join("plans"))
        };
        let permits = Semaphore::new(config.server.max_concurrent_runs.max(1) as usize);
        let cost_guard = CostGuard::new(CostGuardConfig::new(
            config.cost.enabled,
            config.cost.max_usd_per_day,
            config.cost.max_usd_per_run,
            config.cost.warn_ratio,
        ));
        Arc::new(Self {
            db,
            agent,
            config,
            context_resolver,
            cost_guard,
            registry,
            approval_memory,
            gate,
            approval,
            active: RwLock::new(HashMap::new()),
            session_locks: tokio::sync::Mutex::new(HashSet::new()),
            permits,
            event_lock: tokio::sync::Mutex::new(()),
            services,
            estop_path,
            plans_dir,
            auth: std::sync::OnceLock::new(),
            memory: std::sync::OnceLock::new(),
        })
    }

    /// Attach the credential store `/login` writes. Called once at startup; a second call is
    /// ignored so the manager can never swap stores under an in-flight run.
    pub fn with_auth(self: Arc<Self>, auth: Arc<crate::auth::AuthStore>) -> Arc<Self> {
        drop(self.auth.set(auth));
        self
    }

    /// Attach where runs report to ai-memory. Called once at startup, like [`Self::with_auth`].
    pub fn with_memory(self: Arc<Self>, memory: Option<MemoryHooks>) -> Arc<Self> {
        if let Some(memory) = memory {
            drop(self.memory.set(memory));
        }
        self
    }

    /// The provider new runs route through: the one `/login` activated, else config.toml's.
    pub fn current_provider(&self) -> String {
        self.auth
            .get()
            .and_then(|auth| auth.active())
            .unwrap_or_else(|| String::clone(&self.config.model.provider))
    }

    /// The model when neither request nor session names one: the one last picked for the current
    /// provider (auth.json), else config.toml's model for the configured provider (a preset default
    /// such as LM Studio's `local-model` is a placeholder), else the preset default.
    fn default_model(&self) -> String {
        let named = |model: Option<&str>| {
            model
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
        };
        self.auth
            .get()
            .and_then(|auth| {
                let provider = self.current_provider();
                named(
                    auth.credential(&provider)
                        .and_then(|credential| credential.model)
                        .as_deref(),
                )
                .or_else(|| {
                    named(
                        provider
                            .eq_ignore_ascii_case(self.config.model.provider.trim())
                            .then_some(self.config.model.name.as_str()),
                    )
                })
                .or_else(|| {
                    named(
                        silver_protocol::providers::preset(&provider)
                            .map(|preset| preset.default_model),
                    )
                })
            })
            .unwrap_or_else(|| String::clone(&self.config.model.name))
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Session and persisted "always allow" approvals shared with the approval gate.
    pub fn approval_memory(&self) -> &Arc<ApprovalMemory> {
        &self.approval_memory
    }

    /// Mutable approval policy behind the gate.
    pub fn approval(&self) -> &Arc<ApprovalControl> {
        &self.approval
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The opt-in spend guard shared with the run tasks.
    pub fn cost_guard(&self) -> &CostGuard {
        &self.cost_guard
    }

    pub fn agent(&self) -> &Arc<Agent> {
        &self.agent
    }

    /// The skills backend a run reads, when one is configured. Powers `GET /v1/skills`.
    pub fn skills(&self) -> Option<&Arc<dyn silver_core::services::SkillsBackend>> {
        self.services.skills.as_ref()
    }

    /// Every preset: Minimal, Pi, then custom by name.
    pub async fn presets(&self) -> CoreResult<Vec<Preset>> {
        let minimal_tools: Vec<String> = self
            .agent
            .tools()
            .all()
            .iter()
            .filter(|tool| self.agent.tool_enabled(tool.name(), tool.toolset()))
            .map(|tool| tool.name().to_string())
            .collect();
        let mut out = crate::presets::builtins(minimal_tools);
        let mut custom = self.db.list_presets().await?;
        out.append(&mut custom);
        Ok(out)
    }

    async fn checked_preset(&self, id: &str) -> CoreResult<Option<String>> {
        let trimmed = id.trim();
        if trimmed.is_empty() || trimmed == crate::presets::MINIMAL {
            return Ok(None);
        }
        if self
            .presets()
            .await?
            .iter()
            .any(|preset| preset.id == trimmed)
        {
            return Ok(Some(trimmed.to_string()));
        }
        Err(CoreError::InvalidRequest(format!(
            "unknown preset '{trimmed}'; GET /v1/presets lists the presets"
        )))
    }

    pub async fn set_session_preset(&self, session: SessionId, id: &str) -> CoreResult<()> {
        let checked = self.checked_preset(id).await?;
        self.db.set_session_preset(session, checked).await?;
        Ok(())
    }

    async fn run_preset(&self, session: SessionId) -> CoreResult<Option<Preset>> {
        let stored = self.db.session_presets(&[session]).await?;
        let Some(id) = stored.get(&session) else {
            return Ok(None);
        };
        Ok(self
            .presets()
            .await?
            .into_iter()
            .find(|preset| &preset.id == id))
    }

    /// The path of the persistent emergency-stop sentinel.
    pub fn estop_path(&self) -> &std::path::Path {
        &self.estop_path
    }

    /// The session's plan file, whether or not it was written yet.
    pub fn plan_file(&self, session: SessionId) -> PathBuf {
        self.plans_dir.join(format!("{session}.md"))
    }

    /// Enter or leave plan mode from the session's next run.
    pub async fn set_plan_mode(&self, session: &Session, on: bool) -> CoreResult<()> {
        if on && session.workspace_id.is_none() {
            return Err(CoreError::InvalidRequest(
                "plan mode needs a workspace: the plan comes from reading its code. Open a workspace and try /plan again.".into(),
            ));
        }
        self.db
            .set_session_plan_mode(session.id, on)
            .await
            .map_err(CoreError::from)
    }

    /// Whether the emergency stop is engaged. The sentinel file is the truth, so a new process
    /// starts paused; a stat error other than not-found counts as engaged.
    pub fn is_paused(&self) -> bool {
        match std::fs::metadata(&self.estop_path) {
            Ok(_) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => {
                tracing::warn!(
                    path = %self.estop_path.display(),
                    error = %err,
                    "could not inspect the ESTOP sentinel; holding new runs paused"
                );
                true
            }
        }
    }

    /// Engage the emergency stop, writing the sentinel a restart reads. Idempotent.
    pub fn pause(&self) {
        self.pause_with_reason(None);
    }

    /// Engage the emergency stop and record why. A body that cannot be written still leaves
    /// an empty sentinel behind, so a partial write never silently unpauses.
    pub fn pause_with_reason(&self, reason: Option<&str>) {
        if let Some(parent) = self.estop_path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    path = %parent.display(),
                    error = %err,
                    "could not create the ESTOP sentinel directory"
                );
            }
        }
        let body = serde_json::json!({
            "reason": reason,
            "engaged_at": Utc::now().to_rfc3339(),
        });
        if let Err(err) = std::fs::write(&self.estop_path, format!("{body}\n")) {
            tracing::warn!(
                path = %self.estop_path.display(),
                error = %err,
                "could not write the ESTOP sentinel body; falling back to an empty sentinel"
            );
            if let Err(err) = std::fs::File::create(&self.estop_path) {
                tracing::error!(
                    path = %self.estop_path.display(),
                    error = %err,
                    "emergency stop could not be persisted"
                );
            }
        }
    }

    /// Release the emergency stop, removing the sentinel. Idempotent.
    pub fn resume(&self) {
        match std::fs::remove_file(&self.estop_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                path = %self.estop_path.display(),
                error = %err,
                "could not remove the ESTOP sentinel"
            ),
        }
    }

    /// Release any per-scope resources held by the terminal backend (for example when a
    /// workspace is deleted).
    pub async fn cleanup_scope(&self, scope: &Scope) {
        if let Some(terminal) = &self.services.terminal {
            terminal.cleanup(scope).await;
        }
    }

    /// Validate a session or create one bound to the requested scope (INV-3, INV-5).
    pub(crate) async fn resolve_session(
        &self,
        session_id: Option<SessionId>,
        workspace_id: Option<WorkspaceId>,
        source: String,
        external_key: Option<String>,
    ) -> CoreResult<Session> {
        if let Some(session_id) = session_id {
            let session = self
                .db
                .get_session(session_id)
                .await?
                .ok_or(CoreError::SessionNotFound(session_id))?;
            if session.workspace_id != workspace_id {
                return Err(CoreError::SessionWorkspaceMismatch);
            }
            return Ok(session);
        }

        if let Some(external_key) = &external_key {
            if let Some(existing) = self
                .db
                .find_session_by_external_key(&source, external_key, workspace_id)
                .await?
            {
                return Ok(existing);
            }
        }

        let now = Utc::now();
        let session = Session {
            id: SessionId::new(),
            workspace_id,
            source,
            external_key,
            title: None,
            created_at: now,
            updated_at: now,
        };
        Ok(self.db.create_session(session).await?)
    }

    async fn resolve_workspace(&self, id: Option<WorkspaceId>) -> CoreResult<Option<Workspace>> {
        let Some(id) = id else {
            return Ok(None);
        };
        let workspace = self
            .db
            .get_workspace(id)
            .await?
            .ok_or(CoreError::WorkspaceNotFound(id))?;
        let canonical = Workspace::canonicalize_root(&workspace.canonical_root)?;
        if canonical != workspace.canonical_root {
            return Err(CoreError::WorkspaceUnavailable(format!(
                "{} moved since registration",
                workspace.root.display()
            )));
        }
        Ok(Some(workspace))
    }

    pub async fn create_run(
        self: &Arc<Self>,
        req: CreateRunRequest,
    ) -> CoreResult<RunCreatedResponse> {
        self.create_run_idempotent(req, None).await
    }

    /// Admit a run, honoring an optional per-session idempotency key. When the key was
    /// already used in the resolved session the original run is returned unchanged; when
    /// two inserts race, the unique index makes the loser replay the winner's run.
    pub async fn create_run_idempotent(
        self: &Arc<Self>,
        req: CreateRunRequest,
        idempotency_key: Option<String>,
    ) -> CoreResult<RunCreatedResponse> {
        self.validate_run_request(&req)?;
        let goal = run_goal(&req)?;
        let mut req = req;
        let Admission {
            session,
            workspace,
            run_effort,
            run_preset,
        } = self.admit(&mut req).await?;
        let session_id = session.id;

        // A replay is answered before the busy check: the original run may still be active.
        if let Some(key) = idempotency_key.as_deref() {
            if let Some(existing) = self.db.find_run_by_idempotency(session.id, key).await? {
                return Ok(run_created_response(&existing));
            }
        }
        self.lock_session(session_id).await?;

        let (model, provider, pin) = match self.resolve_served_model(&mut req, session_id).await {
            Ok(route) => route,
            Err(err) => {
                self.session_locks.lock().await.remove(&session_id);
                return Err(err);
            }
        };
        let plan = match self.start_plan(session_id).await {
            Ok(plan) => plan,
            Err(err) => {
                self.session_locks.lock().await.remove(&session_id);
                return Err(err);
            }
        };
        let run = match self
            .create_run_row(&req, session_id, &model, idempotency_key.as_deref())
            .await
        {
            Ok(CreatedRun::New(run)) => run,
            Ok(CreatedRun::Replay(response)) => {
                self.session_locks.lock().await.remove(&session_id);
                return Ok(response);
            }
            Err(err) => {
                self.session_locks.lock().await.remove(&session_id);
                return Err(err);
            }
        };
        self.pin_session_route(
            session_id,
            Some(run.model),
            Some(String::clone(&provider)),
            goal.as_ref(),
        )
        .await;

        let run_id = run.id;
        let (broadcast_tx, control) = self.register_active_run(run_id).await;
        let agent = self
            .build_run_agent(
                &run_preset,
                run_effort,
                &provider,
                &model,
                session.source == crate::chat::SESSION_SOURCE,
            )
            .await;
        let services = self.run_services(run_preset);

        let task = RunTask {
            run_id,
            session_id,
            session,
            workspace_id: req.workspace_id,
            workspace,
            model,
            provider,
            agent,
            services,
            plan,
            input: req.message,
            external_context: req.external_context,
            broadcast_tx,
            control,
        };
        let manager = Arc::clone(self);
        tokio::spawn(crate::routed::with_session_provider(pin, async move {
            manager.execute_run(task).await;
        }));

        Ok(RunCreatedResponse {
            run_id,
            session_id,
            status: RunStatus::Queued,
            events_url: format!("/v1/runs/{run_id}/events"),
        })
    }

    /// Reject a request before any session is created.
    fn validate_run_request(&self, req: &CreateRunRequest) -> CoreResult<()> {
        let message_bytes = serde_json::to_vec(&req.message)
            .map_err(|e| CoreError::InvalidRequest(e.to_string()))?
            .len() as u64;
        if message_bytes > self.config.server.max_message_bytes {
            return Err(CoreError::ContextTooLarge(format!(
                "message is {message_bytes} bytes, limit is {}",
                self.config.server.max_message_bytes
            )));
        }
        if let Some(effort) = req.reasoning_effort.as_deref() {
            let effort = effort.trim();
            if !effort.is_empty() && !crate::config::is_valid_reasoning_effort(effort) {
                return Err(CoreError::InvalidRequest(format!(
                    "reasoning_effort {effort:?} must be one of {}",
                    crate::config::REASONING_EFFORT_LADDER.join(", ")
                )));
            }
        }
        Ok(())
    }

    /// Resolve the session and persist a request-named preset, effort, YOLO or plan mode before
    /// the run can prompt. The stored effort and preset it returns are the run's defaults.
    async fn admit(&self, req: &mut CreateRunRequest) -> CoreResult<Admission> {
        let workspace = self.resolve_workspace(req.workspace_id).await?;
        let checked = match req.preset.as_deref() {
            Some(id) => self.checked_preset(id).await?,
            None => None,
        };
        // When the request names no preset, keep the session's stored one. Validating before
        // resolve_session means an unknown id creates no empty session.
        let preset_was_pinned = req.preset.is_some();
        let effort_was_pinned = req
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .is_some_and(|effort| !effort.is_empty());
        let session = self
            .resolve_session(
                req.session_id,
                req.workspace_id,
                std::mem::take(&mut req.source),
                std::mem::take(&mut req.external_key),
            )
            .await?;
        // Hydrate the per-session YOLO cache so a flag persisted by an earlier process (resume)
        // is honored before the first approval can prompt.
        self.approval.refresh_session_yolo(session.id).await;
        if let Some(yolo) = req.yolo {
            self.approval.set_session_yolo(session.id, yolo).await?;
        }
        if let Some(on) = req.plan_mode {
            self.set_plan_mode(&session, on).await?;
        }
        if preset_was_pinned {
            self.db.set_session_preset(session.id, checked).await?;
        }
        if effort_was_pinned {
            self.db
                .set_session_reasoning_effort(
                    session.id,
                    req.reasoning_effort
                        .as_deref()
                        .map(str::trim)
                        .map(str::to_string),
                )
                .await?;
        }
        // A request-named level wins over the session's stored one.
        let stored_effort = self.db.session_reasoning_effort(session.id).await?;
        let run_effort = req
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .or(stored_effort.as_deref())
            .map(str::to_string);
        let run_preset = self.run_preset(session.id).await?;
        Ok(Admission {
            session,
            workspace,
            run_effort,
            run_preset,
        })
    }

    /// Take the per-session admission lock, refusing while another run is active.
    async fn lock_session(&self, session_id: SessionId) -> CoreResult<()> {
        {
            let mut locks = self.session_locks.lock().await;
            if !locks.insert(session_id) {
                return Err(CoreError::SessionBusy(session_id));
            }
        }
        if self.db.has_active_run(session_id).await? {
            self.session_locks.lock().await.remove(&session_id);
            return Err(CoreError::SessionBusy(session_id));
        }
        Ok(())
    }

    /// Resolve the model that will answer and pin its route. The served model and price come
    /// from the session's route, and a guarded run is refused before the run row is written.
    async fn resolve_served_model(
        &self,
        req: &mut CreateRunRequest,
        session_id: SessionId,
    ) -> CoreResult<(String, String, Option<String>)> {
        let current = self.current_provider();
        let (model, provider) = match std::mem::take(&mut req.model) {
            Some(model) => (model, current),
            None => match self.db.session_route(session_id).await? {
                Some((model, provider)) => (model, provider.unwrap_or(current)),
                None => (self.default_model(), current),
            },
        };
        // With nobody signed in, config.toml's provider is the configured model, not a route.
        let signed_in = self.auth.get().and_then(|auth| auth.active()).is_some();
        let pin =
            (signed_in || provider != self.config.model.provider).then(|| String::clone(&provider));
        let guarded = self.cost_guard.enabled();
        let (model, price) = crate::routed::with_session_provider(Option::clone(&pin), async {
            let model = self.agent.served_model(&model).await.unwrap_or(model);
            let price = if guarded {
                self.agent.model_price(&model).await
            } else {
                None
            };
            (model, price)
        })
        .await;
        if guarded {
            let estimate = estimate_run_cost_usd(price, &req.message);
            let today = Utc::now().format("%Y-%m-%d").to_string();
            let decision = self
                .db
                .spend_since(&today)
                .await
                .map_err(CoreError::from)
                .and_then(|daily_spend| {
                    self.cost_guard
                        .check_before_run(daily_spend, estimate)
                        .map_err(|denied| CoreError::InvalidRequest(denied.message()))
                });
            decision?;
        }
        Ok((model, provider, pin))
    }

    /// Start the run's plan, creating the plans directory a shell-written plan needs.
    async fn start_plan(&self, session_id: SessionId) -> CoreResult<Plan> {
        let mode = self
            .db
            .start_plan_run(session_id)
            .await
            .map_err(CoreError::from)?;
        if mode.is_on() {
            if let Err(err) = std::fs::create_dir_all(&self.plans_dir) {
                tracing::warn!(
                    path = %self.plans_dir.display(),
                    error = %err,
                    "could not create the plan directory"
                );
            }
        }
        Ok(Plan::new(mode, self.plan_file(session_id)))
    }

    /// Insert the queued run, replaying the original on a duplicate idempotency key.
    async fn create_run_row(
        &self,
        req: &CreateRunRequest,
        session_id: SessionId,
        model: &str,
        idempotency_key: Option<&str>,
    ) -> CoreResult<CreatedRun> {
        let run = Run {
            id: RunId::new(),
            session_id,
            workspace_id: req.workspace_id,
            status: RunStatus::Queued,
            model: model.to_string(),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error_code: None,
            error_message: None,
        };
        match self.db.create_run_with_key(run, idempotency_key).await {
            Ok(run) => Ok(CreatedRun::New(run)),
            Err(DbError::Conflict(_)) => {
                if let Some(key) = idempotency_key {
                    if let Some(existing) = self.db.find_run_by_idempotency(session_id, key).await?
                    {
                        return Ok(CreatedRun::Replay(run_created_response(&existing)));
                    }
                }
                Err(CoreError::Conflict("run already exists".into()))
            }
            Err(err) => Err(CoreError::from(err)),
        }
    }

    /// Persist the route and goal so resuming the session later keeps them.
    async fn pin_session_route(
        &self,
        session_id: SessionId,
        model: Option<String>,
        provider: Option<String>,
        goal: Option<&SessionGoal>,
    ) {
        if let Err(error) = self
            .db
            .set_session_model_override(session_id, model, provider)
            .await
        {
            tracing::warn!(%error, "could not pin the session's model");
        }
        if let Some(goal) = goal {
            if let Err(error) = self.db.set_session_goal(session_id, Some(goal)).await {
                tracing::warn!(%error, "could not set the session's goal");
            }
        }
    }

    /// Register the live run so a subscriber can attach before the turn starts.
    async fn register_active_run(
        &self,
        run_id: RunId,
    ) -> (
        tokio::sync::broadcast::Sender<silver_protocol::RunEvent>,
        RunControl,
    ) {
        let (broadcast_tx, _) = broadcast::channel(2048);
        let control = RunControl::new();
        self.active.write().await.insert(
            run_id,
            ActiveRun {
                events: tokio::sync::broadcast::Sender::clone(&broadcast_tx),
                control: RunControl::clone(&control),
            },
        );
        (broadcast_tx, control)
    }

    /// The run's agent: the base agent with the preset's tools and the clamped effort. The team
    /// tools are denied on the base agent; only a bot in the chat gets them back.
    async fn build_run_agent(
        &self,
        run_preset: &Option<Preset>,
        run_effort: Option<String>,
        provider: &str,
        model: &str,
        chat: bool,
    ) -> Arc<Agent> {
        let run_effort = if let Some(effort) = run_effort {
            let supported = match &self.context_resolver {
                Some(resolver) => crate::context_length::supported_reasoning_efforts(
                    resolver.registry().await.as_deref(),
                    Some(provider),
                    model,
                ),
                None => crate::config::REASONING_EFFORTS
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            };
            crate::context_length::clamp_to_supported(&effort, &supported)
        } else {
            None
        };
        // `with_only_tools` clears the base deny-list, so the team tools must be hidden again
        // for any run outside the chat (and shown again for one inside it).
        let team = |agent: Agent| {
            if chat {
                agent.with_tools(&silver_core::tools::team::TEAM_TOOLS)
            } else {
                agent.without_tools(&silver_core::tools::team::TEAM_TOOLS)
            }
        };
        match (run_preset, run_effort) {
            (Some(preset), Some(level)) => Arc::new(team(
                Agent::clone(&self.agent)
                    .with_only_tools(&preset.tools)
                    .with_reasoning_effort(Some(level.as_str())),
            )),
            (Some(preset), None) => Arc::new(team(
                Agent::clone(&self.agent).with_only_tools(&preset.tools),
            )),
            (None, Some(level)) => Arc::new(team(
                Agent::clone(&self.agent).with_reasoning_effort(Some(level.as_str())),
            )),
            (None, None) if chat => Arc::new(team(Agent::clone(&self.agent))),
            (None, None) => Arc::clone(&self.agent),
        }
    }

    /// The run's services, with a preset skills filter when one is set.
    fn run_services(&self, run_preset: Option<Preset>) -> ToolServices {
        let mut services = ToolServices::clone(&self.services);
        if let Some(preset) = run_preset {
            if let Some(inner) = services.skills.take() {
                services.skills = Some(Arc::new(crate::presets::PresetSkills {
                    inner,
                    filter: preset.skills,
                }));
            }
        }
        services
    }

    /// Run the admitted turn to completion and record its outcome.
    async fn execute_run(self: Arc<Self>, mut task: RunTask) {
        let _permit = self.permits.acquire().await.ok();
        let started = Utc::now();
        drop(self.db.start_run(task.run_id, started).await);
        let run_id = task.run_id;
        let session_id = task.session_id;
        let workspace_id = task.workspace_id;

        let scope = match workspace_id {
            Some(id) => Scope::Workspace(id),
            None => Scope::Global,
        };
        let project_instructions = match &task.workspace {
            Some(workspace) => {
                load_project_instructions(&workspace.canonical_root).unwrap_or_default()
            }
            None => Vec::new(),
        };
        let platform = match task.session.source.as_str() {
            "local" => "cli".to_string(),
            source => source.to_string(),
        };
        let (agents_index, skills_index) = self
            .prompt_indices(
                &task.services,
                &task.agent,
                task.workspace.as_ref(),
                &platform,
            )
            .await;
        let history = sanitize_replay_history(
            self.db
                .recent_messages(session_id, REPLAY_HISTORY_BYTES)
                .await
                .unwrap_or_default(),
        );
        let title_seed = task.input.plain_text();
        let capture = self
            .open_capture(&mut task, history.is_empty(), &title_seed)
            .await;
        let ctx = Arc::new(RunContext {
            run_id,
            scope,
            workspace: task.workspace,
            session: task.session,
            model: String::clone(&task.model),
            project_instructions,
            base_system_prompt: DEFAULT_BASE_PROMPT.to_string(),
            services: task.services,
            skills_index,
            agents_index,
            provider: task.provider,
            platform,
            session_started: started,
            plan: task.plan,
            external_context: task.external_context,
        });

        let (emitter, receiver) = EventEmitter::channel(run_id);
        emitter.emit(EventPayload::RunQueued {
            session_id,
            workspace_id,
        });
        let drain = self.pump_events(receiver, task.broadcast_tx, session_id, capture);

        let transcript: Arc<dyn TranscriptSink> =
            Arc::new(DbTranscript::new(Db::clone(&self.db), Some(title_seed)));
        let outcome = task
            .agent
            .run_turn(
                ctx,
                task.input,
                history,
                emitter,
                task.control,
                Arc::clone(&self.gate),
                transcript,
            )
            .await;
        // Queued before the run leaves the registry, so a shutdown that waits for runs sees it.
        if let Some(capture) = drain.await.ok().flatten() {
            capture.finish();
        }

        let status = self
            .finalize_run(run_id, session_id, &task.model, outcome)
            .await;
        drop(_permit);
        match status {
            RunStatus::Completed => self.continue_goal(session_id).await,
            // A stopped run stops its goal too; /goal resume picks it up again.
            RunStatus::Cancelled => {
                drop(self.update_goal(session_id, GoalUpdate::Pause).await);
            }
            _ => {}
        }
    }

    /// Persist and publish the run's events as they arrive, showing each to the capture. The
    /// task hands the capture back once the run's events end.
    fn pump_events(
        self: &Arc<Self>,
        mut receiver: mpsc::UnboundedReceiver<silver_protocol::RunEvent>,
        broadcast_tx: broadcast::Sender<silver_protocol::RunEvent>,
        session_id: SessionId,
        mut capture: Option<RunCapture>,
    ) -> tokio::task::JoinHandle<Option<RunCapture>> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let _guard = manager.event_lock.lock().await;
                if event.payload.is_replayable() {
                    drop(manager.db.append_event(&event).await);
                }
                if matches!(event.payload, EventPayload::PlanModeExited) {
                    drop(manager.db.set_session_plan_mode(session_id, false).await);
                }
                if let Some(capture) = capture.as_mut() {
                    capture.observe(&event.payload);
                }
                drop(broadcast_tx.send(event));
            }
            capture
        })
    }

    /// Start reporting the run to ai-memory: its prompt, and the handoff the session claims. Only
    /// a workspace run on a native provider is reported; an ACP agent has its own hooks, and
    /// capturing it here too would store every event twice.
    async fn open_capture(
        &self,
        task: &mut RunTask,
        new_session: bool,
        prompt: &str,
    ) -> Option<RunCapture> {
        let memory = self.memory.get().filter(|_| native_loop(&task.provider))?;
        let workspace = task.workspace.as_ref()?;
        let capture = memory.run(task.session_id, task.run_id, &workspace.canonical_root);
        if new_session {
            if let Some(handoff) = memory.handoff(&capture).await {
                task.external_context = Some(match task.external_context.take() {
                    Some(context) => format!("{context}\n\n{handoff}"),
                    None => handoff,
                });
            }
        }
        capture.begin(new_session, prompt);
        Some(capture)
    }

    /// The subagent catalogue and skills index rendered into the prompt for this run.
    async fn prompt_indices(
        &self,
        services: &ToolServices,
        agent: &Agent,
        workspace: Option<&Workspace>,
        platform: &str,
    ) -> (Option<String>, Option<String>) {
        let project_root = workspace.map(|workspace| workspace.canonical_root.as_path());
        let agents_index = services.subagents.as_ref().and_then(|runner| {
            runner.prompt_index(project_root, &agent.tool_names(workspace.is_some()))
        });
        let skills_index = if let Some(backend) = &services.skills {
            // Overlay the workspace's .agents/skills on the global store and gate by platform,
            // matching what the skills_list tool sees.
            let skills = backend
                .list_for_platform(Some(platform), project_root)
                .await
                .unwrap_or_default();
            silver_core::prompt::render_skills_index(&skills)
        } else {
            None
        };
        (agents_index, skills_index)
    }

    /// Record the finished run and release its admission state.
    async fn finalize_run(
        &self,
        run_id: RunId,
        session_id: SessionId,
        model: &str,
        outcome: TurnOutcome,
    ) -> RunStatus {
        let (status, error_code, error_message, usage) = match outcome {
            TurnOutcome::Completed {
                usage, cost_usd, ..
            } => (
                RunStatus::Completed,
                None,
                None,
                usage.map(|u| (u, cost_usd)),
            ),
            TurnOutcome::Failed { code, message } => (
                RunStatus::Failed,
                Some(code.as_str().to_string()),
                Some(message),
                None,
            ),
            TurnOutcome::Cancelled { .. } => (RunStatus::Cancelled, None, None, None),
        };
        drop(
            self.db
                .finish_run(run_id, status, error_code, error_message)
                .await,
        );
        if let Some((usage, cost)) = usage {
            drop(self.db.record_run_usage(run_id, usage, cost).await);
            if let Some(cost) = cost {
                // Record the advisory cost against the run and feed the guard's in-process
                // total. The ledger is the authoritative daily source.
                drop(self.db.record_spend(run_id, session_id, model, cost).await);
                self.cost_guard.record(cost);
            }
        }
        self.registry.cancel_run(run_id);
        self.active.write().await.remove(&run_id);
        self.session_locks.lock().await.remove(&session_id);
        status
    }

    pub async fn get_run_view(&self, run_id: RunId) -> CoreResult<RunView> {
        let run = self
            .db
            .get_run(run_id)
            .await?
            .ok_or(CoreError::RunNotFound(run_id))?;
        Ok(run_view(run))
    }

    pub async fn stop_run(&self, run_id: RunId) -> CoreResult<RunView> {
        let run = self
            .db
            .get_run(run_id)
            .await?
            .ok_or(CoreError::RunNotFound(run_id))?;
        if let Some(active) = self.active.read().await.get(&run_id) {
            active.control.cancel_with("client");
        }
        Ok(run_view(run))
    }

    pub async fn steer(&self, run_id: RunId, message: String) -> CoreResult<()> {
        let active = self.active.read().await;
        let run = active.get(&run_id).ok_or(CoreError::RunNotActive(run_id))?;
        run.control.steer(message);
        Ok(())
    }

    pub fn decide_approval(
        &self,
        run_id: RunId,
        request: silver_protocol::ApprovalDecisionRequest,
    ) -> CoreResult<()> {
        let outcome = match request.decision {
            ApprovalDecision::Approve => match request.answer {
                Some(answer) => ApprovalOutcome::Answered(answer),
                None => ApprovalOutcome::Approved,
            },
            ApprovalDecision::ApproveSession => ApprovalOutcome::ApprovedSession,
            ApprovalDecision::ApproveAlways => ApprovalOutcome::ApprovedAlways,
            ApprovalDecision::Deny => ApprovalOutcome::Denied,
        };
        if self
            .registry
            .resolve_for_run(run_id, request.approval_id, outcome)
        {
            Ok(())
        } else {
            Err(CoreError::ApprovalNotFound(request.approval_id))
        }
    }

    /// The approvals a run is waiting on, in any order.
    pub fn pending_approvals(&self, run_id: RunId) -> Vec<ApprovalId> {
        self.registry.pending(run_id)
    }

    pub async fn subscribe(
        &self,
        run_id: RunId,
        last_event_id: Option<u64>,
    ) -> CoreResult<EventSubscription> {
        if self.db.get_run(run_id).await?.is_none() {
            return Err(CoreError::RunNotFound(run_id));
        }
        let last = last_event_id.unwrap_or(0);
        let guard = self.event_lock.lock().await;
        let live = self
            .active
            .read()
            .await
            .get(&run_id)
            .map(|active| active.events.subscribe());
        let mut replay = self.db.list_events_after(run_id, last).await?;
        drop(guard);

        let mut queue: VecDeque<_> = VecDeque::new();
        if last > 0 {
            if let Some(first) = replay.first() {
                if first.event_id.0 > last + 1 {
                    queue.push_back(silver_protocol::RunEvent {
                        run_id,
                        event_id: EventId(last),
                        created_at: Utc::now(),
                        payload: EventPayload::ReplayGap {
                            from_event_id: EventId(last),
                        },
                    });
                }
            }
        }
        let last_seq = replay.last().map(|event| event.event_id.0).unwrap_or(last);
        queue.extend(replay.drain(..));
        Ok(EventSubscription {
            run_id,
            replay: queue,
            live,
            last_seq,
        })
    }

    /// Pause, resume, clear or re-budget a session's goal. Resuming continues at once when
    /// the session is idle.
    pub async fn update_goal(
        self: &Arc<Self>,
        session: SessionId,
        update: GoalUpdate,
    ) -> CoreResult<()> {
        if update == GoalUpdate::Clear {
            return self
                .db
                .set_session_goal(session, None)
                .await
                .map_err(CoreError::from);
        }
        let Some(mut goal) = self.db.session_goals(&[session]).await?.remove(&session) else {
            return Err(CoreError::InvalidRequest(
                "this session has no goal; set one with /goal <objective>".into(),
            ));
        };
        match update {
            GoalUpdate::Pause if goal.status == GoalStatus::Active => {
                goal.status = GoalStatus::Paused
            }
            GoalUpdate::Resume => {
                if goal.status == GoalStatus::Exhausted {
                    goal.used = 0;
                }
                goal.status = GoalStatus::Active;
            }
            GoalUpdate::Budget(budget) => goal.max = goal_budget(budget),
            _ => {}
        }
        self.db.set_session_goal(session, Some(&goal)).await?;
        if update == GoalUpdate::Resume {
            Arc::clone(self).continue_goal(session).await;
        }
        Ok(())
    }

    /// Start the next continuation of an active goal, or mark it exhausted once its budget
    /// is spent. A busy session skips it: the run in progress continues the goal when it
    /// completes. Boxed because it runs inside the task `create_run` spawns.
    fn continue_goal(
        self: Arc<Self>,
        session_id: SessionId,
    ) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            if self.is_paused() {
                return;
            }
            let goal = self
                .db
                .session_goals(&[session_id])
                .await
                .ok()
                .and_then(|mut goals| goals.remove(&session_id));
            let Some(mut goal) = goal.filter(|goal| goal.status == GoalStatus::Active) else {
                return;
            };
            let Ok(Some(session)) = self.db.get_session(session_id).await else {
                return;
            };
            if goal.used >= goal.max {
                goal.status = GoalStatus::Exhausted;
            } else {
                goal.used += 1;
            }
            if let Err(error) = self.db.set_session_goal(session_id, Some(&goal)).await {
                tracing::warn!(%error, "could not update the session's goal");
                return;
            }
            if goal.status == GoalStatus::Exhausted {
                return;
            }
            let request = CreateRunRequest {
                workspace_id: session.workspace_id,
                session_id: Some(session_id),
                source: session.source,
                external_key: None,
                message: MessageInput::text(format!("{GOAL_PREFIX}{}", goal.objective)),
                model: None,
                reasoning_effort: None,
                yolo: None,
                preset: None,
                plan_mode: None,
                goal_budget: None,
                external_context: None,
            };
            if let Err(error) = self.create_run(request).await {
                tracing::info!(%error, "goal continuation not started");
            }
        })
    }

    pub async fn active_run_count(&self) -> usize {
        self.active.read().await.len()
    }

    /// Cancel every active run and wait briefly for them to leave the registry.
    pub async fn shutdown(&self) {
        for active in self.active.read().await.values() {
            active.control.cancel_with("shutdown");
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.active_run_count().await > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// Whether silver's own loop runs this provider's tools, rather than an external ACP agent that
/// runs its own.
fn native_loop(provider: &str) -> bool {
    silver_protocol::providers::preset(provider)
        .is_none_or(|preset| preset.kind != silver_protocol::providers::ProviderKind::Acp)
}

/// The 202 body for an admitted or replayed run.
fn run_created_response(run: &Run) -> RunCreatedResponse {
    RunCreatedResponse {
        run_id: run.id,
        session_id: run.session_id,
        status: run.status,
        events_url: format!("/v1/runs/{}/events", run.id),
    }
}

/// Pre-run USD estimate for the spend guard: input from the message text, output assumed about as
/// long. An unknown price estimates zero, so the cap never blocks an unpriced route.
fn estimate_run_cost_usd(
    price: Option<silver_core::pricing::ModelPrice>,
    message: &silver_protocol::MessageInput,
) -> f64 {
    let tokens = silver_core::model_metadata::estimate_tokens_rough(&message.plain_text()) as u64;
    price.map_or(0.0, |price| {
        silver_core::pricing::estimate_cost_usd(&price, tokens, tokens)
    })
}

fn run_view(run: Run) -> RunView {
    RunView {
        id: run.id,
        session_id: run.session_id,
        workspace_id: run.workspace_id,
        status: run.status,
        model: run.model,
        created_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
        error_code: run.error_code,
        error_message: run.error_message,
    }
}

/// Clean persisted history before replay: drop orphaned tool calls and results, and merge
/// same-role rows so the provider sees strict user/assistant alternation.
pub fn sanitize_replay_history(messages: Vec<Message>) -> Vec<Message> {
    merge_consecutive_roles(pair_tool_calls(strip_dangling_tool_call_tail(messages)))
}

/// Drop tool calls and results whose partner is outside the replay window (it can open mid-loop, or
/// a dead turn can leave the mirror hole); OpenAI-compatible providers reject either half alone.
fn pair_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    use silver_protocol::ContentPart;
    let answered: HashSet<&silver_protocol::ToolCallId> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { tool_call_id, .. } => Some(tool_call_id),
            _ => None,
        })
        .collect();
    // A result counts only after the call it answers, and a call only with its result.
    let mut announced = HashSet::new();
    let mut keep = Vec::new();
    for message in &messages {
        for part in &message.content {
            keep.push(match (message.role, part) {
                (MessageRole::Assistant, ContentPart::ToolCall { id, .. }) => {
                    let paired = answered.contains(id);
                    if paired {
                        announced.insert(id);
                    }
                    paired
                }
                (MessageRole::Tool, ContentPart::ToolResult { tool_call_id, .. }) => {
                    announced.contains(tool_call_id)
                }
                _ => true,
            });
        }
    }

    let mut keep = keep.into_iter();
    messages
        .into_iter()
        .filter_map(|mut message| {
            message.content.retain(|_| keep.next().unwrap_or(true));
            (!message.content.is_empty()).then_some(message)
        })
        .collect()
}

/// Drop a trailing assistant(tool_calls) row that no tool result answers. A partially answered
/// block ends with a tool row, so it is kept.
pub fn strip_dangling_tool_call_tail(mut messages: Vec<Message>) -> Vec<Message> {
    let dangling = messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Assistant && has_tool_call(message));
    if dangling {
        messages.pop();
    }
    messages
}

/// Whether the message announces at least one tool call.
fn has_tool_call(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|part| matches!(part, silver_protocol::ContentPart::ToolCall { .. }))
}

/// Merge consecutive rows with the same role into the first, dropping duplicate content
/// parts. Only user and assistant rows are merged; consecutive tool results are distinct
/// answers and consecutive system rows are left alone.
fn merge_consecutive_roles(messages: Vec<Message>) -> Vec<Message> {
    let mut merged: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        let mergeable = matches!(message.role, MessageRole::User | MessageRole::Assistant);
        if let Some(previous) = merged.last_mut() {
            if mergeable && previous.role == message.role {
                for part in message.content {
                    if !previous.content.contains(&part) {
                        previous.content.push(part);
                    }
                }
                continue;
            }
        }
        merged.push(message);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::native_loop;

    #[test]
    fn only_an_acp_agent_runs_its_own_loop() {
        assert!(native_loop("anthropic"));
        assert!(native_loop("a-provider-not-in-the-catalog"));
        assert!(!native_loop("claude"));
    }
}
