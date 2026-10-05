//! The agent turn loop.

use crate::advisor::{Advisor, Step};
use crate::agent::verify::VerificationTracker;
use crate::context::{RunContext, SubdirectoryHintTracker};
use crate::error::{CoreError, CoreResult, RetryClass};
use crate::event::EventEmitter;
use crate::guard::empty_response::{EmptyAttempt, EmptyDecision, EmptyResponseGuard};
use crate::guard::iteration_budget::{normalize_budget_warning_ratio, IterationBudget};
use crate::guard::liveness::{watch_turn_liveness, ActivityClock};
use crate::guard::repetition::{is_repetition_dominated, REPETITION_LOOP_INTERRUPTED};
use crate::guard::tool_guardrails::{
    append_toolguard_guidance, toolguard_synthetic_result, ToolCallGuardrailConfig,
    ToolCallGuardrailController,
};
use crate::model::ContextLengthResolver;
use crate::model::{
    FinishReason, Model, ModelMessage, ModelRequest, ModelResponse, ModelStream, ModelStreamEvent,
    ToolCallRequest, PICTURE_DROPPED,
};
use crate::pricing::ModelPrice;
use crate::session::Message;
use crate::tool::{ApprovalPolicy, ToolContext, ToolOutcome, ToolRegistry};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use silver_protocol::{
    ApprovalDecision, ApprovalId, ContentPart, ErrorCode, EventPayload, MessageId, MessageInput,
    MessageRole, RiskLevel, RunId, SessionId, TokenUsage, ToolCallId, ToolStatus,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Outcome of one agent turn.
#[derive(Clone, Debug)]
pub enum TurnOutcome {
    Completed {
        text: String,
        usage: Option<TokenUsage>,
        /// Advisory USD estimate; None when the model's price is unknown.
        cost_usd: Option<f64>,
        iterations: u32,
    },
    Failed {
        code: ErrorCode,
        message: String,
    },
    Cancelled {
        origin: String,
    },
}

/// Shared, cloneable control handle for one run.
#[derive(Clone)]
pub struct RunControl {
    pub cancel: CancellationToken,
    origin: Arc<Mutex<Option<String>>>,
    steering: Arc<Mutex<Vec<String>>>,
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RunControl {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            origin: Arc::new(Mutex::new(None)),
            steering: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A handle that follows `parent`: its own origin and steering queue, but cancelling the run
    /// cancels this one, so a stop reaches a subagent in flight.
    pub fn child_of(parent: &CancellationToken) -> Self {
        Self {
            cancel: parent.child_token(),
            origin: Arc::new(Mutex::new(None)),
            steering: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Record the cancellation origin (first writer wins) and cancel.
    pub fn cancel_with(&self, origin: impl Into<String>) {
        {
            let mut slot = self.origin.lock().expect("origin lock");
            if slot.is_none() {
                *slot = Some(origin.into());
            }
        }
        self.cancel.cancel();
    }

    /// The recorded cancellation origin, consumed so the string moves out.
    pub fn take_origin(&self) -> String {
        std::mem::take(&mut *self.origin.lock().expect("origin lock"))
            .unwrap_or_else(|| "client".to_string())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Queue guidance to be injected as a user message at the next safe loop point.
    pub fn steer(&self, text: impl Into<String>) {
        self.steering.lock().expect("steer lock").push(text.into());
    }

    pub fn take_steering(&self) -> Vec<String> {
        std::mem::take(&mut *self.steering.lock().expect("steer lock"))
    }

    /// Whether a steer is queued, without consuming it.
    pub fn has_pending_steering(&self) -> bool {
        !self.steering.lock().expect("steer lock").is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct ApprovalRequest<'a> {
    pub approval_id: ApprovalId,
    pub run_id: RunId,
    /// Session the requested call belongs to; scopes a session-level approval.
    pub session_id: SessionId,
    /// Scope of the run; an "always" approval holds only inside it.
    pub scope: &'a silver_protocol::Scope,
    pub tool_call_id: &'a ToolCallId,
    pub tool_name: &'a str,
    pub risk: RiskLevel,
    pub arguments: &'a serde_json::Value,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    /// Approved and remembered for this session, same tool and arguments.
    ApprovedSession,
    /// Approved and remembered across runs and daemon restarts.
    ApprovedAlways,
    /// The user's reply to an ask_user_question call.
    Answered(String),
    Denied,
    Timeout,
    Cancelled,
}

/// Tool result for an approval nobody answered; unlike a denial, the model must not retry or
/// rephrase the call.
pub const APPROVAL_TIMEOUT_MESSAGE: &str = "approval request timed out without a decision. Do not retry or rephrase this call; if it is still needed, ask the user to approve it explicitly.";

#[async_trait::async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request(
        &self,
        request: ApprovalRequest<'_>,
        cancel: &CancellationToken,
    ) -> ApprovalOutcome;

    /// Put a plan or a question to the user. No approval mode or earlier decision answers
    /// for them; the default asks like any other request.
    async fn ask_user(
        &self,
        request: ApprovalRequest<'_>,
        cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        self.request(request, cancel).await
    }

    /// Whether an earlier decision covers this session/tool/arguments, so no prompt is needed.
    /// Gates without memory keep the default `false`.
    fn remembered(&self, _request: &ApprovalRequest<'_>) -> bool {
        false
    }
}

/// Test/development gate that approves everything.
pub struct AutoApproveGate;

#[async_trait::async_trait]
impl ApprovalGate for AutoApproveGate {
    async fn request(
        &self,
        _request: ApprovalRequest<'_>,
        _cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        ApprovalOutcome::Approved
    }
}

/// Test/development gate that denies everything.
pub struct AutoDenyGate;

#[async_trait::async_trait]
impl ApprovalGate for AutoDenyGate {
    async fn request(
        &self,
        _request: ApprovalRequest<'_>,
        _cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        ApprovalOutcome::Denied
    }
}

/// Test gate that reports cancellation (as if the run was stopped while waiting).
pub struct AutoCancelGate;

#[async_trait::async_trait]
impl ApprovalGate for AutoCancelGate {
    async fn request(
        &self,
        _request: ApprovalRequest<'_>,
        _cancel: &CancellationToken,
    ) -> ApprovalOutcome {
        ApprovalOutcome::Cancelled
    }
}

#[async_trait::async_trait]
pub trait TranscriptSink: Send + Sync {
    async fn append(&self, message: Message) -> CoreResult<()>;

    /// Best-effort model-generated session title; never overwrite a title the user set.
    async fn set_model_title(&self, _session_id: SessionId, _title: &str) -> CoreResult<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct InMemoryTranscript {
    messages: Mutex<Vec<Message>>,
}

impl InMemoryTranscript {
    pub fn messages(&self) -> std::sync::MutexGuard<'_, Vec<Message>> {
        self.messages.lock().expect("transcript lock")
    }
}

#[async_trait::async_trait]
impl TranscriptSink for InMemoryTranscript {
    async fn append(&self, message: Message) -> CoreResult<()> {
        self.messages.lock().expect("transcript lock").push(message);
        Ok(())
    }
}

/// A sink that keeps nothing: a subagent's turns are not session history, only its report is, and
/// keeping them would fill `session_search` with a second conversation.
pub struct DiscardTranscript;

#[async_trait::async_trait]
impl TranscriptSink for DiscardTranscript {
    async fn append(&self, _message: Message) -> CoreResult<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub max_tool_calls_per_run: u32,
    /// Fraction of the iteration budget consumed before a one-shot wrap-up notice is
    /// injected into the newest tool result. Normalized by
    /// [`normalize_budget_warning_ratio`]; `None` (or any out-of-range value) disables it.
    pub budget_warning_ratio: Option<f64>,
    /// Longest silence tolerated from a hosted model, both before the stream opens and
    /// between two stream events.
    pub model_timeout: Duration,
    /// The same budget for a model whose endpoint is local (see [`Model::is_local`]): a
    /// 4B model on a laptop streams tool-call arguments at ~100 B/s, and LM Studio only
    /// emits them once the whole call is generated, so minutes of silence are routine.
    pub local_model_timeout: Duration,
    pub tool_timeout: Duration,
    pub run_timeout: Duration,
    pub guardrails: ToolCallGuardrailConfig,
    pub turn_liveness_timeout_s: Option<f64>,
    pub turn_liveness_poll_s: f64,
    pub empty_guard_enabled: bool,
    pub empty_cost_threshold_usd: f64,
    pub max_context_bytes: usize,
    /// Whether to compact the request context before it reaches `max_context_bytes`.
    pub context_compaction_enabled: bool,
    /// How many recent tool results are always kept verbatim during compaction.
    pub context_keep_recent_tool_results: usize,
    /// How many recent user turns are always kept verbatim during compaction.
    pub context_keep_recent_turns: usize,
    /// Whether to summarize dropped context with the main model when no auxiliary model is
    /// configured. Off by default, so compaction stays deterministic unless the operator opts in.
    pub summarize_with_main_model: bool,
    /// Requested reasoning effort (none, minimal, low, medium or high) sent on every request.
    /// The transport clamps unusual labels onto its own wire vocabulary.
    pub reasoning_effort: Option<String>,
    /// Resolved context window in tokens for the configured model, wired from the daemon
    /// (config override, disk cache/probe, static table). None leaves the static family table
    /// as the last resort before the byte default.
    pub context_length: Option<usize>,
    /// Reasoning tokens one model call may spend before it is cut off and told to act.
    /// None applies LOCAL_REASONING_TOKENS to a local model and no limit to a hosted one;
    /// Some(0) turns the limit off.
    pub reasoning_budget: Option<usize>,
    /// Maximum extra model calls used to continue an output-limit-truncated reply.
    pub max_truncation_continuations: u32,
    /// Maximum retries of a single provider request after a transient failure.
    pub retry_max_attempts: u32,
    /// Base delay for exponential backoff between retries.
    pub retry_base_delay: Duration,
    /// Upper bound for a single backoff wait (also caps a server Retry-After).
    pub retry_max_delay: Duration,
    /// Whether to nudge the model to verify after editing files without a fresh passing
    /// verification command. On by default.
    pub verify_on_stop: bool,
    /// Maximum verify-on-stop follow-up nudges injected per run.
    pub verify_max_nudges: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 500,
            max_tool_calls_per_run: 200,
            budget_warning_ratio: Some(0.8),
            model_timeout: Duration::from_secs(120),
            local_model_timeout: Duration::from_secs(900),
            tool_timeout: Duration::from_secs(120),
            run_timeout: Duration::from_secs(1800),
            guardrails: ToolCallGuardrailConfig::default(),
            turn_liveness_timeout_s: Some(600.0),
            turn_liveness_poll_s: 15.0,
            empty_guard_enabled: true,
            empty_cost_threshold_usd: 0.25,
            max_context_bytes: 350_000,
            context_compaction_enabled: true,
            context_keep_recent_tool_results: 6,
            context_keep_recent_turns: 8,
            summarize_with_main_model: false,
            reasoning_effort: None,
            context_length: None,
            reasoning_budget: None,
            max_truncation_continuations: 3,
            retry_max_attempts: 4,
            retry_base_delay: Duration::from_millis(500),
            retry_max_delay: Duration::from_secs(20),
            verify_on_stop: true,
            verify_max_nudges: 3,
        }
    }
}

/// Working window for a local model: inference slows as context grows (a 4B model on a laptop GPU
/// was a third slower at 70k than at 8k). At 32k, compaction cleared reads the answer needed; at
/// 64k typical runs never compact.
const LOCAL_CONTEXT_TOKENS: usize = 65_536;

/// Reasoning a local model may spend on one call before it is cut off and told to act. A 4B
/// model once spent 5.7k tokens (169 s) rewriting the same function in its head. Hosted models
/// enforce their own limits and bill for thinking they were asked for, so none applies there.
const LOCAL_REASONING_TOKENS: usize = 4_096;

/// Consecutive cut-offs before a call may think without limit: a model that cannot act without
/// longer reasoning should still finish the task.
const MAX_REASONING_CUTS: u32 = 2;

/// How much of cut-off reasoning is handed back, so the model acts on it instead of starting
/// over: chat templates drop earlier reasoning, so it has to come back as text.
const REASONING_CUT_TAIL_BYTES: usize = 1_500;

/// Byte budget the loop compacts at. A non-default `max_context_bytes` wins; otherwise this run's
/// resolved window (a quarter reserved for the reply), then the static table, then the startup
/// window, then the byte default. A local model works within LOCAL_CONTEXT_TOKENS of it.
fn compaction_budget(
    config: &AgentConfig,
    model: &str,
    resolved: Option<usize>,
    local: bool,
) -> usize {
    let default_context_bytes = AgentConfig::default().max_context_bytes;
    if config.max_context_bytes != default_context_bytes {
        return config.max_context_bytes;
    }
    resolved
        .filter(|tokens| *tokens > 0)
        .or_else(|| crate::model_metadata::context_length_for(model))
        .or(config.context_length)
        .map(|tokens| {
            if local {
                tokens.min(LOCAL_CONTEXT_TOKENS)
            } else {
                tokens
            }
        })
        // ~3 bytes per token with a quarter kept for the reply: without the reserve, messages that
        // fit alone overflow once the provider adds its output reservation.
        .map(|tokens| tokens.saturating_mul(9) / 4)
        .unwrap_or(default_context_bytes)
}

/// Add a finished answer segment to the text assembled for the client. Unlike a truncated
/// reply, which continues mid-sentence, the next segment is a new paragraph.
fn push_answer_segment(assembled: &mut String, segment: &str) {
    if !segment.is_empty() {
        assembled.push_str(segment);
        assembled.push_str("\n\n");
    }
}

/// How full the context window is for `request`, for the status display. The first message
/// is the system prompt (plus any compaction summary); the rest is the conversation.
fn context_usage(
    request: &ModelRequest,
    budget_bytes: usize,
    window_tokens: Option<u64>,
) -> silver_protocol::ContextUsage {
    let (system, conversation) = request.messages.split_at(request.messages.len().min(1));
    let system_prompt_bytes = crate::agent::compact::estimate_bytes(system) as u64;
    let tool_schema_bytes = estimate_tool_schema_bytes(&request.tools) as u64;
    let conversation_bytes = crate::agent::compact::estimate_bytes(conversation) as u64;
    silver_protocol::ContextUsage {
        system_prompt_bytes,
        tool_schema_bytes,
        conversation_bytes,
        total_bytes: system_prompt_bytes + tool_schema_bytes + conversation_bytes,
        budget_bytes: budget_bytes as u64,
        window_tokens,
        prompt_tokens: None,
    }
}

/// Rough bytes of the tool schemas every request carries; the message estimate leaves them out, and
/// a tool-heavy request would overflow without subtracting them.
fn estimate_tool_schema_bytes(specs: &[crate::model::ToolSpec]) -> usize {
    let mut total = 0usize;
    for spec in specs {
        total += spec.name.len() + spec.description.len() + spec.parameters.to_string().len() + 16;
    }
    total
}

#[derive(Clone)]
pub struct Agent {
    model: Arc<dyn Model>,
    /// Optional side model for context summarization and title generation. Never the main model
    /// unless the operator opted into summarizing with it.
    aux_model: Option<Arc<dyn Model>>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    approval_policy: ApprovalPolicy,
    /// Answers the context window of the model each run uses; None keeps the startup value.
    context_resolver: Option<Arc<dyn ContextLengthResolver>>,
    /// Toolsets visible to the model this run; the default is unrestricted.
    toolsets: crate::toolset::ToolsetSelection,
    /// Tool-name allow-list (None = every name). Applied after the toolset filter.
    tool_allow: Option<std::collections::BTreeSet<String>>,
    /// Tool-name deny-list, applied after the allow-list (deny wins).
    tool_deny: std::collections::BTreeSet<String>,
    /// Outside classifier that hands the model hints during a turn; None gives none.
    advisor: Option<Arc<dyn Advisor>>,
}

impl Agent {
    pub fn new(
        model: Arc<dyn Model>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
        approval_policy: ApprovalPolicy,
    ) -> Self {
        Self {
            model,
            aux_model: None,
            tools,
            config,
            approval_policy,
            context_resolver: None,
            toolsets: crate::toolset::ToolsetSelection::all(),
            tool_allow: None,
            tool_deny: std::collections::BTreeSet::new(),
            advisor: None,
        }
    }

    pub fn with_advisor(mut self, advisor: Option<Arc<dyn Advisor>>) -> Self {
        self.advisor = advisor;
        self
    }

    /// Restrict the tools this agent exposes to the model to `selection`.
    pub fn with_toolsets(mut self, selection: crate::toolset::ToolsetSelection) -> Self {
        self.toolsets = selection;
        self
    }

    /// Filter the model-facing tools by name, on top of the toolset filter: `enabled` is an
    /// allow-list (empty = every name) and `disabled` is a deny-list applied after it (deny wins).
    /// This is what the `[tools] enabled/disabled` config and `silver tools enable/disable` drive.
    pub fn with_tool_names(mut self, enabled: &[String], disabled: &[String]) -> Self {
        self.tool_allow = if enabled.is_empty() {
            None
        } else {
            Some(enabled.iter().cloned().collect())
        };
        self.tool_deny = disabled.iter().cloned().collect();
        self
    }

    /// Hide `tools` from this run, whatever the filters above allow.
    pub fn without_tools(mut self, tools: &[&str]) -> Self {
        self.tool_deny
            .extend(tools.iter().map(|name| (*name).to_string()));
        self
    }

    /// Show `tools` to this run again, undoing a deny-list entry. A chat bot lifts the team
    /// tools the base agent denies to everyone else.
    pub fn with_tools(mut self, tools: &[&str]) -> Self {
        for tool in tools {
            self.tool_deny.remove(*tool);
        }
        self
    }

    /// Restrict this run to exactly `tools`: the toolset filter is lifted and the name
    /// deny-list is cleared, so a deny-listed tool is visible once listed. An empty
    /// list means no tools.
    pub fn with_only_tools(mut self, tools: &[String]) -> Self {
        self.toolsets = crate::toolset::ToolsetSelection::all();
        self.tool_allow = Some(tools.iter().cloned().collect());
        self.tool_deny = std::collections::BTreeSet::new();
        self
    }

    /// Override the reasoning effort for this run; None keeps the configured value.
    /// The transport clamps unusual labels onto its own wire vocabulary.
    pub fn with_reasoning_effort(mut self, effort: Option<&str>) -> Self {
        if let Some(effort) = effort {
            self.config.reasoning_effort = Some(effort.to_string());
        }
        self
    }

    /// Whether a tool passes the toolset selection and the name filter. Workspace gating is
    /// separate, so a catalog can report config-level enablement.
    pub fn tool_enabled(&self, name: &str, toolset: &str) -> bool {
        self.toolsets.allows(toolset) && self.name_visible(name)
    }

    /// Whether a tool name survives the name-level allow/deny filter.
    fn name_visible(&self, name: &str) -> bool {
        if self.tool_deny.contains(name) {
            return false;
        }
        self.tool_allow
            .as_ref()
            .is_none_or(|allow| allow.contains(name))
    }

    /// Specs the model sees this run: the toolset filter, then the name allow/deny filter.
    fn visible_specs(&self, has_workspace: bool) -> Vec<crate::model::ToolSpec> {
        self.tools
            .specs_for(has_workspace, &self.toolsets)
            .into_iter()
            .filter(|spec| self.name_visible(&spec.name))
            .collect()
    }

    /// The tool names a run of this agent may call, so a subagent's definition can be resolved
    /// against them. Plan mode's own tools are not here: they belong to the run, not to the
    /// agent, and a subagent never gets them.
    pub fn tool_names(&self, has_workspace: bool) -> Vec<String> {
        self.visible_specs(has_workspace)
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    /// Replace the loop's configuration, for a run with a budget of its own (a subagent).
    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// The list price of a model, from the resolver, when known.
    pub async fn model_price(&self, model: &str) -> Option<ModelPrice> {
        self.context_resolver.as_ref()?.price(model).await
    }

    /// The model a local server runs in place of `model`, from the resolver, if another.
    pub async fn served_model(&self, model: &str) -> Option<String> {
        self.context_resolver.as_ref()?.served_model(model).await
    }

    /// Attach the resolver that sizes the compaction budget per run.
    pub fn with_context_resolver(mut self, resolver: Arc<dyn ContextLengthResolver>) -> Self {
        self.context_resolver = Some(resolver);
        self
    }

    /// Attach the optional auxiliary model route (context summaries and session titles).
    pub fn with_aux_model(mut self, aux_model: Option<Arc<dyn Model>>) -> Self {
        self.aux_model = aux_model;
        self
    }

    /// The configured auxiliary model, when one is present.
    pub fn aux_model(&self) -> Option<&Arc<dyn Model>> {
        self.aux_model.as_ref()
    }

    pub fn model_name(&self) -> &str {
        self.model.name()
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// The model that summarizes a dropped span: the auxiliary model when configured, else the
    /// main model only when the operator opted in.
    fn summarizer(&self) -> Option<&Arc<dyn Model>> {
        if let Some(aux) = &self.aux_model {
            return Some(aux);
        }
        if self.config.summarize_with_main_model {
            return Some(&self.model);
        }
        None
    }

    /// Best-effort summary of a dropped span; None on timeout, error or empty output.
    async fn summarize_span(
        &self,
        model: &Arc<dyn Model>,
        span: &[ModelMessage],
    ) -> Option<String> {
        let input = crate::agent::compact::serialize_span(span);
        if input.trim().is_empty() {
            return None;
        }
        let request = ModelRequest {
            model: String::new(),
            messages: vec![
                ModelMessage::system(crate::agent::compact::SUMMARY_SYSTEM_PROMPT),
                ModelMessage::user(input),
            ],
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(crate::agent::compact::SUMMARY_MAX_TOKENS),
            cache_key: None,
            reasoning_effort: None,
            run_id: None,
            workspace: None,
        };
        let text = call_aux_text(
            model,
            request,
            crate::agent::compact::SUMMARY_TIMEOUT,
            "context_summary",
        )
        .await?;
        Some(truncate_chars(
            &text,
            crate::agent::compact::SUMMARY_MAX_CHARS,
        ))
    }

    /// After the first assistant text response, upgrade the deterministic title with the
    /// auxiliary model. Best-effort: any failure leaves the existing title in place.
    async fn maybe_upgrade_title(
        &self,
        ctx: &Arc<RunContext>,
        user_text: &str,
        assistant_text: &str,
        transcript: &dyn TranscriptSink,
    ) {
        let Some(aux) = self.aux_model.as_ref() else {
            return;
        };
        if user_text.trim().is_empty() {
            return;
        }
        let request = ModelRequest {
            model: String::new(),
            messages: vec![
                ModelMessage::system(TITLE_SYSTEM_PROMPT),
                ModelMessage::user(format!(
                    "User message:\n{}\n\nAssistant reply:\n{}",
                    truncate_chars(user_text, TITLE_INPUT_MAX_CHARS),
                    truncate_chars(assistant_text, TITLE_INPUT_MAX_CHARS)
                )),
            ],
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(TITLE_MAX_TOKENS),
            cache_key: None,
            reasoning_effort: None,
            run_id: None,
            workspace: None,
        };
        let Some(raw) = call_aux_text(aux, request, TITLE_TIMEOUT, "session_title").await else {
            tracing::info!(
                aux_call = "session_title",
                outcome = "unavailable",
                "title upgrade skipped"
            );
            return;
        };
        let Some(title) = clean_generated_title(&raw) else {
            tracing::info!(
                aux_call = "session_title",
                outcome = "rejected",
                "title upgrade produced no usable title"
            );
            return;
        };
        match transcript.set_model_title(ctx.session.id, &title).await {
            Ok(()) => tracing::info!(
                aux_call = "session_title",
                outcome = "upgraded",
                char_len = title.chars().count(),
                "session title upgraded"
            ),
            Err(err) => tracing::warn!(
                aux_call = "session_title",
                outcome = "persist_failed",
                error = %err,
                "could not persist the upgraded session title"
            ),
        }
    }

    /// Run one turn to completion, streaming every event through the emitter.
    #[expect(
        clippy::too_many_arguments,
        reason = "agent turn orchestration needs all collaborators"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "agent turn orchestration is inherently long"
    )]
    pub async fn run_turn(
        &self,
        ctx: Arc<RunContext>,
        input: MessageInput,
        history: Vec<Message>,
        events: EventEmitter,
        control: RunControl,
        gate: Arc<dyn ApprovalGate>,
        transcript: Arc<dyn TranscriptSink>,
    ) -> TurnOutcome {
        let started_at = Utc::now();

        // Plan mode's own tools, outside every preset and filter. The set is fixed for the
        // run, so leaving plan mode mid-run keeps the cached prompt prefix.
        let plan_tools = crate::plan::tools(ctx.plan.mode.is_on());
        let mut specs = self.visible_specs(ctx.has_workspace());
        specs.extend(plan_tools.specs(ctx.has_workspace()));
        // An explicit byte budget always wins; on the default, the window of the model this
        // run actually uses is resolved now (it can differ from the startup model) and
        // converted conservatively to bytes, reserving a quarter for the reply.
        let resolved_context = match &self.context_resolver {
            Some(resolver) => resolver.context_length(&ctx.model).await,
            None => None,
        };
        let base_context_threshold = compaction_budget(
            &self.config,
            &ctx.model,
            resolved_context,
            self.model.is_local(),
        );

        // Scale the instruction-file cap with the model window rather than a fixed 64 KiB.
        let tool_names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        let system = crate::context::build_system_prompt_with_budget(
            &ctx,
            &tool_names,
            Some(base_context_threshold),
        );
        let schema_bytes = estimate_tool_schema_bytes(&specs);

        // The request is the run's working copy: each iteration extends it, and compaction
        // rewrites it in place, never the transcript.
        let mut messages: Vec<ModelMessage> = Vec::with_capacity(history.len() + 2);
        messages.push(ModelMessage::system(String::clone(&system)));
        messages.extend(history.into_iter().map(to_model_message));
        messages.push(ModelMessage {
            role: MessageRole::User,
            content: Vec::clone(&input.content),
        });
        let mut request = ModelRequest {
            model: String::clone(&ctx.model),
            messages,
            tools: specs,
            temperature: None,
            max_tokens: None,
            // Stable for the life of the conversation so every turn reuses the cached
            // prefix. Compaction may rewrite the request body, but the key must not move.
            cache_key: Some(ctx.session.id.to_string()),
            // Operator-configured reasoning effort; the transport clamps unusual labels.
            reasoning_effort: Option::clone(&self.config.reasoning_effort),
            run_id: Some(ctx.run_id),
            workspace: ctx
                .workspace
                .as_ref()
                .map(|workspace| std::path::PathBuf::clone(&workspace.canonical_root)),
        };

        let window_tokens = resolved_context
            .or_else(|| crate::model_metadata::context_length_for(&ctx.model))
            .or(self.config.context_length)
            .map(|tokens| tokens as u64);
        events.emit(EventPayload::RunStarted {
            model: String::clone(&ctx.model),
            started_at,
        });
        events.emit(EventPayload::ContextInjected {
            label: "System prompt".into(),
            text: system,
        });
        // Name each file the prompt holds, so a client shows what was loaded into it.
        let files = ctx
            .project_instructions
            .iter()
            .map(|file| (shown_path(&ctx, &file.path), file.content.as_str()));
        for (name, text) in files.filter(|(_, text)| !text.trim().is_empty()) {
            events.emit(EventPayload::ContextInjected {
                label: format!("Loaded {name}"),
                text: text.to_string(),
            });
        }
        // Plan mode rides the user message, on the working copy only, like the other notices.
        if let Some((label, notice)) = ctx.plan.notice() {
            if let Some(user) = request.messages.last_mut() {
                user.content.push(ContentPart::text(format!("\n{notice}")));
            }
            events.emit(EventPayload::ContextInjected {
                label: label.into(),
                text: notice,
            });
        }

        // The advisor sees the task and every tool call; each hint reaches the model once per
        // run, on the working copy only, like the other loop notices.
        let task = input.plain_text();
        if let Err(err) = transcript
            .append(Message {
                id: MessageId::new(),
                session_id: ctx.session.id,
                run_id: Some(ctx.run_id),
                role: MessageRole::User,
                content: input.content,
                created_at: started_at,
            })
            .await
        {
            return self.finish_failed(
                &events,
                ErrorCode::Internal,
                format!("persist user message: {err}"),
            );
        }
        let mut steps: Vec<Step> = Vec::new();
        let mut hints_given: HashSet<String> = HashSet::new();
        if let Some(hint) = self
            .advisor_hint(&task, &steps, None, &mut hints_given, &events)
            .await
        {
            if let Some(user) = request.messages.last_mut() {
                user.content.push(ContentPart::text(format!("\n{hint}")));
            }
        }

        let clock = Arc::new(ActivityClock::new());
        clock.set_turn_active(true);
        if let Some(timeout_s) = self.config.turn_liveness_timeout_s {
            let clock = Arc::clone(&clock);
            let token = control.cancel.child_token();
            let control = RunControl::clone(&control);
            let poll = self.config.turn_liveness_poll_s;
            tokio::spawn(async move {
                watch_turn_liveness(clock, timeout_s, poll, token, move |_snap, committed| {
                    if committed {
                        control.cancel_with("liveness");
                    }
                })
                .await;
            });
        }

        let budget = IterationBudget::new(self.config.max_iterations);
        let budget_warning_ratio = normalize_budget_warning_ratio(self.config.budget_warning_ratio);
        let mut guardrails = ToolCallGuardrailController::new(self.config.guardrails);
        let mut empty_guard = EmptyResponseGuard::new(
            self.config.empty_guard_enabled,
            self.config.empty_cost_threshold_usd,
        );
        // Only a hosted run has a wall-clock limit. A local model costs nothing and can need
        // hours for a long task at seconds per step; the iteration and tool-call budgets, the
        // stall budget and the liveness watchdog still bound it.
        let run_deadline =
            (!self.model.is_local()).then(|| Instant::now() + self.config.run_timeout);
        let mut iterations = 0u32;
        let mut tool_calls_used = 0u32;
        let mut grace_used = false;
        let mut total_usage: Option<TokenUsage> = None;
        let mut compaction_noted = false;
        // The auxiliary summarizer is called at most once per run, so a slow or failing
        // summary model cannot add a call to every iteration.
        let mut context_summarized = false;
        let mut context_summary: Option<String> = None;
        // Set when a guardrail stops a tool loop: the rest of the turn runs without tools so
        // the model answers with what it has instead of the run ending mid-task.
        // Why tools were turned off, once they are: the final reply names the first cause.
        let mut locked_reason: Option<String> = None;
        // Set when a provider rejects the request as too large; each recovery halves the
        // trigger so the next request is compacted harder.
        let mut context_threshold_override: Option<usize> = None;
        let mut overflow_recoveries = 0u32;
        let mut truncation_continuations = 0u32;
        let mut reasoning_cuts = 0u32;
        let mut continued_text = String::new();
        // The iteration-budget checkpoint notice is one-shot for the whole run.
        let mut budget_warning_injected = false;
        // Per-run verify-on-stop evidence and one-injection-per-directory context hints.
        let mut verify_tracker = VerificationTracker::new();
        let mut hint_tracker = ctx
            .cwd()
            .map(|root| SubdirectoryHintTracker::new(root.to_path_buf()));

        'turn: loop {
            if control.is_cancelled() {
                return self.finish_cancelled(&events, &clock, &control);
            }
            if run_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return self.finish_failed(
                    &events,
                    ErrorCode::ProviderUnavailable,
                    format!(
                        "run timeout: the run reached its {}-minute limit \
                         (server.run_timeout_seconds). Send a message to continue from here.",
                        self.config.run_timeout.as_secs().div_ceil(60)
                    ),
                );
            }

            // Steering enters as a standalone user row at a safe point: never in the middle
            // of a tool-call group, whose results must stay adjacent to their call.
            if let Some(failure) = self
                .deliver_steering(
                    &ctx,
                    &mut request.messages,
                    transcript.as_ref(),
                    &events,
                    &control,
                )
                .await
            {
                return failure;
            }

            if !budget.consume() {
                if grace_used {
                    return self.finish_failed(
                        &events,
                        ErrorCode::Internal,
                        "iteration budget exhausted",
                    );
                }
                // The grace iteration runs without tools, so it can only answer.
                grace_used = true;
                locked_reason = Some("Reached the iteration limit for one run.".into());
                if let Some(last) = request
                    .messages
                    .last_mut()
                    .filter(|m| m.role == MessageRole::Tool)
                {
                    let notice = "[Iteration limit reached. Tools are off: answer the user now \
                                  with what you have, and say what you could not finish.]";
                    append_tool_result_notice(last, notice);
                    events.emit(EventPayload::ContextInjected {
                        label: "Iteration limit".into(),
                        text: notice.into(),
                    });
                }
            }
            iterations += 1;

            if !budget_warning_injected {
                if let Some(notice) = iteration_budget_warning(
                    budget.used(),
                    budget.max_total(),
                    budget_warning_ratio,
                ) {
                    // Only the newest tool result is safe to mutate: an older row may already
                    // be part of a cached prefix. With no tool result yet, surface the notice
                    // to the user instead.
                    match request.messages.last_mut() {
                        Some(last) if last.role == MessageRole::Tool => {
                            append_tool_result_notice(last, &notice);
                            events.emit(EventPayload::ContextInjected {
                                label: "Iteration budget".into(),
                                text: notice,
                            });
                        }
                        _ => {
                            events.emit(EventPayload::RunWaiting { reason: notice });
                        }
                    }
                    budget_warning_injected = true;
                }
            }

            clock.touch();
            // Compaction rewrites the run's working copy in place, never the transcript, and only
            // at the budget, so other requests just extend the last one and a local server reuses
            // its prompt cache. Schema bytes are reserved since the estimate leaves them out.
            let threshold = context_threshold_override
                .unwrap_or(base_context_threshold)
                .saturating_sub(schema_bytes)
                .max(1024);
            if self.config.context_compaction_enabled
                && crate::agent::compact::estimate_bytes(&request.messages) >= threshold
            {
                // Read at each compaction: the note must track todo_list writes made after the
                // results that carried them were cleared.
                let open_todos = match &ctx.services.todos {
                    Some(store) => store
                        .list(ctx.session.id)
                        .await
                        .ok()
                        .and_then(|items| crate::tools::todo::open_items_note(&items)),
                    None => None,
                };
                let compact = |summary: Option<&str>| {
                    crate::agent::compact::compact_with_summary(
                        &request.messages,
                        threshold,
                        self.config.context_keep_recent_tool_results,
                        self.config.context_keep_recent_turns,
                        summary,
                        open_todos.as_deref(),
                    )
                };
                let mut outcome = compact(context_summary.as_deref());
                // Summarize the first dropped span with the auxiliary model, once per run; the
                // summary stays the note for the rest of the run. Any failure keeps the
                // deterministic note.
                if let (Some(span), Some(model)) = (&outcome.dropped_span, self.summarizer()) {
                    if !span.is_empty() && !context_summarized {
                        context_summarized = true;
                        context_summary = self
                            .summarize_span(model, &request.messages[span.start..span.end])
                            .await;
                        match &context_summary {
                            Some(summary) => {
                                tracing::info!(
                                    aux_call = "context_summary",
                                    outcome = "ok",
                                    summary_chars = summary.chars().count(),
                                    dropped_messages = outcome.dropped_messages,
                                    "summarized dropped context with the auxiliary model"
                                );
                                outcome = compact(Some(summary));
                            }
                            None => tracing::warn!(
                                aux_call = "context_summary",
                                outcome = "fallback",
                                dropped_messages = outcome.dropped_messages,
                                "auxiliary context summary unavailable; using deterministic marker"
                            ),
                        }
                    }
                }
                if let Some(note) = outcome.note() {
                    if !compaction_noted {
                        compaction_noted = true;
                        tracing::info!(%note, "compacted request context");
                        events.emit(EventPayload::RunWaiting { reason: note });
                        if let Some(summary) = outcome.summary.take() {
                            events.emit(EventPayload::ContextInjected {
                                label: "Context summary".into(),
                                text: summary,
                            });
                        }
                    }
                }
                request.messages = outcome.messages;
            }
            if locked_reason.is_some() {
                request.tools.clear();
            }
            let context = context_usage(
                &request,
                context_threshold_override.unwrap_or(base_context_threshold),
                window_tokens,
            );
            events.emit(EventPayload::ContextUpdated { context });

            let reasoning_budget = self
                .reasoning_budget()
                .filter(|_| reasoning_cuts < MAX_REASONING_CUTS);
            let mut retry_attempt = 0u32;
            let mut pictures_dropped = false;
            let mut response = 'attempts: loop {
                match self
                    .stream_response(
                        &request,
                        reasoning_budget,
                        &control,
                        &events,
                        &clock,
                        &mut total_usage,
                    )
                    .await
                {
                    Ok(response) => break response,
                    Err(StreamFailure::Cancelled { partial }) => {
                        // Keep what the user already saw stream in, so a reload and the next
                        // run both still have it. Best effort: the run is ending anyway.
                        if !partial.is_empty() {
                            drop(
                                transcript
                                    .append(assistant_text_message(&ctx, &partial, ""))
                                    .await,
                            );
                        }
                        return self.finish_cancelled(&events, &clock, &control);
                    }
                    Err(StreamFailure::Provider {
                        code,
                        message,
                        retry_after,
                        retryable,
                        emitted,
                        partial,
                    }) => {
                        // The provider refused the request because it is too large. Compact
                        // harder and rebuild the request; bounded so a pathological history
                        // cannot loop forever.
                        if code == ErrorCode::ContextTooLarge && !emitted && overflow_recoveries < 2
                        {
                            overflow_recoveries += 1;
                            let current =
                                context_threshold_override.unwrap_or(base_context_threshold);
                            // At least halve: the reported window can equal the one this run
                            // already budgeted for, when the byte estimate undercounts tokens.
                            // tokens -> conservative bytes (reserve a quarter for the reply)
                            let next =
                                crate::model_metadata::parse_context_limit_from_error(&message)
                                    .map_or(current, |tokens| tokens.saturating_mul(9) / 4)
                                    .min(current / 2)
                                    .max(1);
                            context_threshold_override = Some(next);
                            compaction_noted = false;
                            tracing::warn!(
                                threshold = context_threshold_override.unwrap_or(0),
                                "provider rejected the context as too large; compacting harder"
                            );
                            events.emit(EventPayload::RunWaiting {
                                reason: format!(
                                    "context too large; compacting harder ({}/{})",
                                    overflow_recoveries, 2
                                ),
                            });
                            continue 'turn;
                        }
                        // The route refused the request and it carried a picture: a surface may
                        // reject inline images with nothing but a bare 400, so send the receipt
                        // alone once more instead of failing the run.
                        if !retryable
                            && !emitted
                            && !pictures_dropped
                            && request.drop_pictures(PICTURE_DROPPED)
                        {
                            pictures_dropped = true;
                            tracing::warn!(
                                %message,
                                "provider refused the request; retrying without the picture"
                            );
                            events.emit(EventPayload::RunWaiting {
                                reason: PICTURE_DROPPED.to_string(),
                            });
                            continue 'attempts;
                        }
                        if retryable && !emitted && retry_attempt < self.config.retry_max_attempts {
                            retry_attempt += 1;
                            let delay = retry_delay(
                                retry_attempt,
                                retry_after,
                                self.config.retry_base_delay,
                                self.config.retry_max_delay,
                            );
                            tracing::warn!(
                                attempt = retry_attempt,
                                delay_ms = delay.as_millis() as u64,
                                "retrying provider request after transient failure: {message}"
                            );
                            events.emit(EventPayload::RunWaiting {
                                reason: format!(
                                    "provider retry {}/{} in {} ms",
                                    retry_attempt,
                                    self.config.retry_max_attempts,
                                    delay.as_millis()
                                ),
                            });
                            tokio::select! {
                                _ = control.cancel.cancelled() => {
                                    return self.finish_cancelled(&events, &clock, &control);
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                            continue 'attempts;
                        }
                        if !partial.is_empty() {
                            drop(
                                transcript
                                    .append(assistant_text_message(&ctx, &partial, ""))
                                    .await,
                            );
                        }
                        if code == ErrorCode::ContextTooLarge {
                            // Compaction cannot shrink the system prompt or the tool schemas;
                            // say how big a window they need.
                            let usage = context_usage(&request, 0, None);
                            let fixed = usage.system_prompt_bytes + usage.tool_schema_bytes;
                            return self.finish_failed(
                                &events,
                                code,
                                format!(
                                    "the model's context window is too small for this request. \
                                     Load it with a larger context length: the system prompt and \
                                     tools alone need about {} tokens. ({message})",
                                    fixed / 3
                                ),
                            );
                        }
                        return self.finish_failed(&events, code, message);
                    }
                }
            };
            // The provider counted what was sent: the real size of the context, where the bytes
            // above only estimate it.
            if let Some(usage) = response.usage.filter(|usage| usage.prompt_tokens > 0) {
                events.emit(EventPayload::ContextUpdated {
                    context: silver_protocol::ContextUsage {
                        prompt_tokens: Some(usage.prompt_tokens),
                        ..context
                    },
                });
            }

            if matches!(response.finish_reason, Some(FinishReason::Length))
                && is_repetition_dominated(&response.text)
            {
                return self.finish_failed(
                    &events,
                    ErrorCode::Internal,
                    REPETITION_LOOP_INTERRUPTED,
                );
            }

            if response.empty {
                let attempt = EmptyAttempt {
                    model: String::clone(&ctx.model),
                    provider: self.model.name().to_string(),
                    finish_reason: finish_reason_label(&response.finish_reason),
                    usage_present: response.usage.is_some(),
                    zero_output: response
                        .usage
                        .map(|u| u.completion_tokens == 0)
                        .unwrap_or(false),
                    observed_generation: response.observed_generation,
                    estimated_cost_usd: None,
                };
                match empty_guard.record(attempt) {
                    EmptyDecision::Retry => continue,
                    EmptyDecision::SkipToFallback => {
                        empty_guard.reset();
                        return self.finish_failed(
                            &events,
                            ErrorCode::ProviderUnavailable,
                            "model returned an empty completion",
                        );
                    }
                }
            }
            empty_guard.reset();
            if response.reasoning_cut {
                reasoning_cuts += 1;
                let budget = reasoning_budget.unwrap_or_default();
                tracing::info!(budget, cuts = reasoning_cuts, "reasoning cut at its budget");
                request
                    .messages
                    .push(reasoning_cut_message(&response.reasoning));
                request
                    .messages
                    .push(ModelMessage::user(REASONING_CUT_PROMPT));
                events.emit(EventPayload::ContextInjected {
                    label: format!("Reasoning passed {budget} tokens"),
                    text: REASONING_CUT_PROMPT.into(),
                });
                continue 'turn;
            }
            reasoning_cuts = 0;
            // With tools off, a small model may still write a tool call as text. It was
            // trying to act, not answering, so the user gets the reason the run stopped.
            if let Some(reason) = &locked_reason {
                if response.tool_calls.is_empty() && response.text.contains("<tool_call>") {
                    response.text = format!(
                        "{reason} I stopped without finishing the task; send a new message to continue."
                    );
                }
            }

            if response.tool_calls.is_empty() {
                // A steer queued while the model was composing a final answer must still be
                // delivered: persist this segment, inject the steer and let the model respond
                // before the turn is allowed to finish.
                if control.has_pending_steering() {
                    if !response.text.is_empty() {
                        let message =
                            assistant_text_message(&ctx, &response.text, &response.reasoning);
                        if let Err(err) = transcript.append(message).await {
                            return self.finish_failed(
                                &events,
                                ErrorCode::Internal,
                                format!("persist assistant: {err}"),
                            );
                        }
                    }
                    push_answer_segment(&mut continued_text, &response.text);
                    request
                        .messages
                        .push(to_model_message_from_response(&mut response));
                    if let Some(failure) = self
                        .deliver_steering(
                            &ctx,
                            &mut request.messages,
                            transcript.as_ref(),
                            &events,
                            &control,
                        )
                        .await
                    {
                        return failure;
                    }
                    continue 'turn;
                }
                // A reply cut off by the output limit is not a final answer: re-enter and ask
                // the model to continue, bounded so a pathological loop cannot run forever.
                if matches!(response.finish_reason, Some(FinishReason::Length))
                    && !response.text.trim().is_empty()
                    && truncation_continuations < self.config.max_truncation_continuations
                {
                    truncation_continuations += 1;
                    tracing::info!(
                        attempt = truncation_continuations,
                        "continuing a truncated reply"
                    );
                    if let Err(err) = transcript
                        .append(assistant_text_message(
                            &ctx,
                            &response.text,
                            &response.reasoning,
                        ))
                        .await
                    {
                        return self.finish_failed(
                            &events,
                            ErrorCode::Internal,
                            format!("persist partial assistant: {err}"),
                        );
                    }
                    continued_text.push_str(&response.text);
                    request
                        .messages
                        .push(to_model_message_from_response(&mut response));
                    request
                        .messages
                        .push(ModelMessage::user(TRUNCATION_CONTINUATION_PROMPT));
                    events.emit(EventPayload::ContextInjected {
                        label: "Reply cut off".into(),
                        text: TRUNCATION_CONTINUATION_PROMPT.into(),
                    });
                    continue 'turn;
                }
                // After file edits, an answer without a fresh passing check earns a bounded nudge.
                // Otherwise the advisor may send the model back once per hint and that premature
                // answer is left out of the final text.
                let (nudge, keep_answer) = if locked_reason.is_some() {
                    (None, true)
                } else if self.config.verify_on_stop
                    && ctx.has_workspace()
                    && verify_tracker.needs_verification()
                    && verify_tracker.nudges_sent() < self.config.verify_max_nudges
                {
                    verify_tracker.record_nudge();
                    tracing::info!(
                        nudge = verify_tracker.nudges_sent(),
                        max = self.config.verify_max_nudges,
                        "verify-on-stop nudge injected"
                    );
                    let nudge = verify_tracker.build_nudge();
                    events.emit(EventPayload::ContextInjected {
                        label: "Verification required".into(),
                        text: String::clone(&nudge),
                    });
                    (Some(nudge), true)
                } else {
                    let hint = self
                        .advisor_hint(
                            &task,
                            &steps,
                            Some(&response.text),
                            &mut hints_given,
                            &events,
                        )
                        .await;
                    (hint, false)
                };
                if let Some(nudge) = nudge {
                    // Persist this segment so the transcript keeps the model's own words, then
                    // send it back. The nudge itself is transient loop guidance.
                    if !response.text.is_empty() {
                        let message =
                            assistant_text_message(&ctx, &response.text, &response.reasoning);
                        if let Err(err) = transcript.append(message).await {
                            return self.finish_failed(
                                &events,
                                ErrorCode::Internal,
                                format!("persist assistant: {err}"),
                            );
                        }
                    }
                    if keep_answer {
                        push_answer_segment(&mut continued_text, &response.text);
                    } else {
                        continued_text.clear();
                    }
                    request
                        .messages
                        .push(to_model_message_from_response(&mut response));
                    request.messages.push(ModelMessage::user(nudge));
                    continue 'turn;
                }
                let text = format!("{continued_text}{}", response.text)
                    .trim_end()
                    .to_string();
                continued_text.clear();
                // Persist only this final segment: the continuation partials were already
                // appended, so persisting the concatenation would duplicate them. The
                // client-facing completion still carries the full assembled text.
                if !response.text.is_empty() {
                    let message = assistant_text_message(&ctx, &response.text, &response.reasoning);
                    if let Err(err) = transcript.append(message).await {
                        return self.finish_failed(
                            &events,
                            ErrorCode::Internal,
                            format!("persist assistant: {err}"),
                        );
                    }
                    events.emit(EventPayload::TextCompleted {
                        text: String::clone(&text),
                    });
                    // Upgrade the deterministic instant title after the first assistant reply.
                    // This branch is terminal, so the upgrade runs at most once per turn.
                    self.maybe_upgrade_title(&ctx, &task, &text, transcript.as_ref())
                        .await;
                }
                return self
                    .finish_completed(
                        &ctx,
                        &events,
                        &clock,
                        started_at,
                        text,
                        total_usage,
                        iterations,
                    )
                    .await;
            }

            let assistant = to_model_message_from_response(&mut response);
            if let Err(err) = transcript
                .append(Message {
                    id: MessageId::new(),
                    session_id: ctx.session.id,
                    run_id: Some(ctx.run_id),
                    role: MessageRole::Assistant,
                    content: Vec::clone(&assistant.content),
                    created_at: Utc::now(),
                })
                .await
            {
                return self.finish_failed(
                    &events,
                    ErrorCode::Internal,
                    format!("persist assistant tool calls: {err}"),
                );
            }
            request.messages.push(assistant);

            let mut halt_message: Option<String> = None;
            for index in 0..response.tool_calls.len() {
                // The arguments go to the tool; the assistant turn above already holds a copy.
                let mut args = std::mem::take(&mut response.tool_calls[index].arguments);
                let call = &response.tool_calls[index];
                if locked_reason.is_some() {
                    halt_message =
                        Some("the model kept calling tools after they were turned off.".into());
                    if let Some(failure) = self
                        .close_interrupted_tool_sequence(
                            &ctx,
                            &mut request.messages,
                            transcript.as_ref(),
                            &events,
                            &response.tool_calls,
                            index,
                        )
                        .await
                    {
                        return failure;
                    }
                    break;
                }
                if control.is_cancelled() {
                    if let Some(failure) = self
                        .close_interrupted_tool_sequence(
                            &ctx,
                            &mut request.messages,
                            transcript.as_ref(),
                            &events,
                            &response.tool_calls,
                            index,
                        )
                        .await
                    {
                        return failure;
                    }
                    return self.finish_cancelled(&events, &clock, &control);
                }
                tool_calls_used += 1;
                if tool_calls_used > self.config.max_tool_calls_per_run {
                    if let Some(failure) = self
                        .close_interrupted_tool_sequence(
                            &ctx,
                            &mut request.messages,
                            transcript.as_ref(),
                            &events,
                            &response.tool_calls,
                            index,
                        )
                        .await
                    {
                        return failure;
                    }
                    // Like a loop halt: tools go off and the model answers with what it has,
                    // so hitting the cap never ends a run without a reply.
                    halt_message = Some(format!(
                        "Reached the limit of {} tool calls for one run.",
                        self.config.max_tool_calls_per_run
                    ));
                    break;
                }
                // A call whose arguments never parsed is answered with an error result rather
                // than executed with empty arguments.
                if let Some(reason) = &call.malformed_arguments {
                    if let Some(failure) = Self::refuse_call(
                        &ctx,
                        &mut request.messages,
                        transcript.as_ref(),
                        &events,
                        &mut guardrails,
                        call,
                        &args,
                        ToolStatus::Failed,
                        format!("tool call rejected: {reason}"),
                    )
                    .await
                    {
                        return failure;
                    }
                    continue;
                }
                if call.name == crate::plan::ASK_USER_QUESTION {
                    crate::plan::options_as_text(&mut args);
                }
                let decision = guardrails.before_call(&call.name, &args);
                if !decision.allows_execution() {
                    if let Some(failure) = Self::finish_call(
                        &ctx,
                        &mut request.messages,
                        transcript.as_ref(),
                        &events,
                        &call.id,
                        ToolStatus::Blocked,
                        Some(String::clone(&decision.message)),
                        toolguard_synthetic_result(&decision),
                        None,
                    )
                    .await
                    {
                        return failure;
                    }
                    if decision.should_halt() {
                        halt_message = Some(decision.message);
                        if let Some(failure) = self
                            .close_interrupted_tool_sequence(
                                &ctx,
                                &mut request.messages,
                                transcript.as_ref(),
                                &events,
                                &response.tool_calls,
                                index + 1,
                            )
                            .await
                        {
                            return failure;
                        }
                        break;
                    }
                    continue;
                }

                let Some(tool) = self
                    .tools
                    .get(&call.name)
                    .or_else(|| plan_tools.get(&call.name))
                    .filter(|_| request.tools.iter().any(|spec| spec.name == call.name))
                else {
                    let content = if request.tools.is_empty() {
                        format!(
                            "unknown tool: {}. This chat has no tools; answer in text.",
                            call.name
                        )
                    } else {
                        format!(
                            "unknown tool: {}. The tools you can call are: {}.",
                            call.name,
                            request
                                .tools
                                .iter()
                                .map(|spec| spec.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    if let Some(failure) = Self::refuse_call(
                        &ctx,
                        &mut request.messages,
                        transcript.as_ref(),
                        &events,
                        &mut guardrails,
                        call,
                        &args,
                        ToolStatus::Failed,
                        content,
                    )
                    .await
                    {
                        return failure;
                    }
                    continue;
                };

                events.emit(EventPayload::ToolStarted {
                    tool_call_id: ToolCallId::clone(&call.id),
                    name: String::clone(&call.name),
                    preview: sanitize_preview(&args),
                });

                // A dangerous (recoverable) command is escalated to Destructive so it always
                // requires approval; a hardline command is still refused before the gate.
                // The plan file is silver's own, so writing it needs no approval.
                let risk = if ctx.plan.is_written_by(&call.name, &args) {
                    RiskLevel::Read
                } else {
                    self.approval_policy
                        .escalated_risk(&call.name, &args, tool.risk(&args))
                };
                // A hardline denial or an operator deny-glob sits above approval: no
                // decision can allow it, and neither can a disabled approval mode.
                let policy_denial = self
                    .approval_policy
                    .denied_command(&call.name, &args)
                    .map(|pattern| format!("command matches deny_commands pattern {pattern:?}"))
                    .or_else(|| self.approval_policy.hardline_denial(&call.name, &args))
                    .or_else(|| ctx.plan.refusal(&call.name, &args, risk));
                if let Some(reason) = policy_denial {
                    if let Some(failure) = Self::refuse_call(
                        &ctx,
                        &mut request.messages,
                        transcript.as_ref(),
                        &events,
                        &mut guardrails,
                        call,
                        &args,
                        ToolStatus::Denied,
                        format!("operation denied: {reason}"),
                    )
                    .await
                    {
                        return failure;
                    }
                    continue;
                }
                // A plan to approve or a question to answer always goes to the user. With
                // nothing to put to them, the tool itself says what is missing.
                let for_user = match call.name.as_str() {
                    crate::plan::EXIT_PLAN_MODE => ctx.plan.for_review().ok(),
                    crate::plan::ASK_USER_QUESTION => {
                        crate::plan::question(&args).map(str::to_string)
                    }
                    _ => None,
                };
                let asks_user = for_user.is_some();
                let mut answer: Option<String> = None;
                if asks_user || self.approval_policy.requires_approval(risk) {
                    let approval_id = ApprovalId::new();
                    // Surface the matched danger pattern (if any) in the approval prompt.
                    let description = match (
                        for_user,
                        self.approval_policy.approval_description(&call.name, &args),
                    ) {
                        (Some(text), _) => text,
                        (None, Some(reason)) => reason,
                        (None, None) => {
                            format!("{} needs your approval to proceed", call.name)
                        }
                    };
                    let approval = ApprovalRequest {
                        approval_id,
                        run_id: ctx.run_id,
                        session_id: ctx.session.id,
                        scope: &ctx.scope,
                        tool_call_id: &call.id,
                        tool_name: &call.name,
                        risk,
                        arguments: &args,
                        description,
                    };
                    // A remembered decision covers this exact session/tool/arguments
                    // combination, so run the call without emitting an approval event
                    // or asking the gate again.
                    if asks_user || !gate.remembered(&approval) {
                        events.emit(EventPayload::ApprovalRequired {
                            approval_id,
                            tool_call_id: ToolCallId::clone(&call.id),
                            name: String::clone(&call.name),
                            risk,
                            description: String::clone(&approval.description),
                            arguments_preview: sanitize_preview(&args),
                        });
                        let outcome = if asks_user {
                            gate.ask_user(approval, &control.cancel).await
                        } else {
                            gate.request(approval, &control.cancel).await
                        };
                        match outcome {
                            ApprovalOutcome::Approved
                            | ApprovalOutcome::ApprovedSession
                            | ApprovalOutcome::ApprovedAlways
                            | ApprovalOutcome::Answered(_) => {
                                let decision = match outcome {
                                    ApprovalOutcome::ApprovedSession => {
                                        ApprovalDecision::ApproveSession
                                    }
                                    ApprovalOutcome::ApprovedAlways => {
                                        ApprovalDecision::ApproveAlways
                                    }
                                    _ => ApprovalDecision::Approve,
                                };
                                events.emit(EventPayload::ApprovalResolved {
                                    approval_id,
                                    decision,
                                });
                                if let ApprovalOutcome::Answered(text) = outcome {
                                    answer = Some(text);
                                }
                            }
                            ApprovalOutcome::Cancelled => {
                                if let Some(failure) = self
                                    .close_interrupted_tool_sequence(
                                        &ctx,
                                        &mut request.messages,
                                        transcript.as_ref(),
                                        &events,
                                        &response.tool_calls,
                                        index,
                                    )
                                    .await
                                {
                                    return failure;
                                }
                                return self.finish_cancelled(&events, &clock, &control);
                            }
                            ApprovalOutcome::Denied | ApprovalOutcome::Timeout => {
                                events.emit(EventPayload::ApprovalResolved {
                                    approval_id,
                                    decision: ApprovalDecision::Deny,
                                });
                                // A timeout is not a user denial: the prompt was never
                                // answered, so the model is told not to silently retry or
                                // rephrase the call, and its event is distinguishable.
                                let (content, status) = match outcome {
                                    ApprovalOutcome::Timeout => {
                                        (APPROVAL_TIMEOUT_MESSAGE.to_string(), ToolStatus::Failed)
                                    }
                                    _ => (
                                        "operation denied by the user".to_string(),
                                        ToolStatus::Denied,
                                    ),
                                };
                                if let Some(failure) = Self::refuse_call(
                                    &ctx,
                                    &mut request.messages,
                                    transcript.as_ref(),
                                    &events,
                                    &mut guardrails,
                                    call,
                                    &args,
                                    status,
                                    content,
                                )
                                .await
                                {
                                    return failure;
                                }
                                // A small model answers a denial by trying the same change
                                // another way; tools go off so it replies to the user instead.
                                if let Some(failure) = self
                                    .close_interrupted_tool_sequence(
                                        &ctx,
                                        &mut request.messages,
                                        transcript.as_ref(),
                                        &events,
                                        &response.tool_calls,
                                        index + 1,
                                    )
                                    .await
                                {
                                    return failure;
                                }
                                halt_message = Some(match outcome {
                                    ApprovalOutcome::Timeout => {
                                        format!("Nobody answered the approval for {}.", call.name)
                                    }
                                    _ => format!("{} was declined.", call.name),
                                });
                                break;
                            }
                        }
                    }
                }

                let tool_ctx = ToolContext {
                    run: Arc::clone(&ctx),
                    events: Cow::Borrowed(&events),
                    cancel: control.cancel.child_token(),
                    call_id: Cow::Borrowed(&call.id),
                    gate: Arc::clone(&gate),
                };
                // The watchdog backs up a tool's own timeout; it must not cut a longer one the
                // model asked for. A tool that declares its own budget (delegation) gets the
                // larger of the two, so it is never cut short by the run's tool timeout.
                let asked = args
                    .get("timeout")
                    .and_then(serde_json::Value::as_u64)
                    .map(|secs| Duration::from_secs(secs.saturating_add(10)));
                let limit = [Some(self.config.tool_timeout), asked, tool.timeout_hint()]
                    .into_iter()
                    .flatten()
                    .max()
                    .unwrap_or(self.config.tool_timeout);
                // A stop interrupts a running tool instead of waiting for it to finish. The
                // user's answer is the result of the question that asked for it.
                let outcome = match answer {
                    Some(answer) => {
                        ToolOutcome::ok(crate::plan::answered(&args, &answer)).with_summary(answer)
                    }
                    None => tokio::select! {
                        result = tokio::time::timeout(limit, tool.execute(&tool_ctx, serde_json::Value::clone(&args))) => {
                            match result {
                                Err(_) => ToolOutcome::error(format!(
                                    "tool exceeded its {}s timeout",
                                    limit.as_secs()
                                )),
                                Ok(Ok(outcome)) => outcome,
                                Ok(Err(err)) => ToolOutcome::error(err.to_string()),
                            }
                        }
                        _ = control.cancel.cancelled() => ToolOutcome::error("[interrupted while the tool was running]"),
                    },
                };
                clock.touch();

                let status = if outcome.is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                };
                let decision = guardrails.after_call(
                    &call.name,
                    &args,
                    Some(&outcome.content),
                    Some(outcome.is_error),
                );
                let observation = guardrails.observe_call(
                    &call.name,
                    &args,
                    Some(&outcome.content),
                    &call.id.to_string(),
                    outcome.is_error,
                );
                // Read the raw output before it becomes the result the model sees.
                if !ctx.plan.is_on() {
                    verify_tracker.record_tool_result(&call.name, &args, outcome.is_error);
                }
                steps.push(Step::new(&call.name, &args, &outcome.content));
                let summary = outcome
                    .summary
                    .unwrap_or_else(|| summarize_tool_outcome(&outcome.content));
                let mut content = observation.stub.unwrap_or(outcome.content);
                let raw = content.len();
                append_toolguard_guidance(&mut content, &decision);
                if let Some(notice) = observation.notice {
                    content.push_str("\n\n");
                    content.push_str(&notice);
                }
                let mut injected = Vec::new();
                if content.len() > raw {
                    injected.push(("Tool guard".to_string(), content[raw..].trim().to_string()));
                }
                // Progressive subdirectory context rides the tool result (never the system
                // prompt, so prompt caching is preserved).
                if let Some(tracker) = hint_tracker.as_mut() {
                    for (path, hint) in tracker.hints_for_tool_call(&call.name, &args) {
                        content.push_str("\n\n");
                        content.push_str(&hint);
                        injected.push((format!("Loaded {}", shown_path(&ctx, &path)), hint));
                    }
                }
                if let Some(failure) = Self::finish_call(
                    &ctx,
                    &mut request.messages,
                    transcript.as_ref(),
                    &events,
                    &call.id,
                    status,
                    Some(summary),
                    content,
                    outcome
                        .image
                        .as_ref()
                        .map(|(media_type, bytes)| ContentPart::image(media_type, bytes)),
                )
                .await
                {
                    return failure;
                }
                for (label, text) in injected {
                    events.emit(EventPayload::ContextInjected { label, text });
                }
                // An approved plan ends the planning turn: the work starts as its own message,
                // so retrying it never loses the plan.
                if call.name == crate::plan::EXIT_PLAN_MODE && !outcome.is_error {
                    if let Some(failure) = self
                        .close_interrupted_tool_sequence(
                            &ctx,
                            &mut request.messages,
                            transcript.as_ref(),
                            &events,
                            &response.tool_calls,
                            index + 1,
                        )
                        .await
                    {
                        return failure;
                    }
                    return self
                        .finish_completed(
                            &ctx,
                            &events,
                            &clock,
                            started_at,
                            String::new(),
                            total_usage,
                            iterations,
                        )
                        .await;
                }
                // observe_call records its identical-streak and cycle halts on the controller
                // rather than returning them, so check both.
                let halt = if decision.should_halt() {
                    Some(decision.message)
                } else {
                    guardrails
                        .halt_decision()
                        .map(|halt| String::clone(&halt.message))
                };
                if let Some(message) = halt {
                    halt_message = Some(message);
                    if let Some(failure) = self
                        .close_interrupted_tool_sequence(
                            &ctx,
                            &mut request.messages,
                            transcript.as_ref(),
                            &events,
                            &response.tool_calls,
                            index + 1,
                        )
                        .await
                    {
                        return failure;
                    }
                    break;
                }
            }

            // A round whose only tool is execute_code is an RPC-style programmatic call;
            // refund the iteration so cheap programmatic work does not eat the budget.
            if !response.tool_calls.is_empty()
                && response
                    .tool_calls
                    .iter()
                    .all(|call| call.name == PROGRAMMATIC_TOOL_NAME)
            {
                budget.refund();
            }

            if let Some(reason) = halt_message {
                let Some(first) = &locked_reason else {
                    let notice = format!(
                        "{reason} Tools are off for the rest of this turn: answer the user now \
                         with what you have, and say what you could not do and what they can do \
                         next."
                    );
                    locked_reason = Some(reason);
                    if let Some(last) = request.messages.last_mut() {
                        append_tool_result_notice(last, &format!("[{notice}]"));
                    }
                    events.emit(EventPayload::ContextInjected {
                        label: "Tools turned off".into(),
                        text: notice,
                    });
                    continue 'turn;
                };
                let text = format!(
                    "Stopped to avoid a tool loop: {first} The model kept calling tools after \
                     they were turned off, so it gave no answer."
                );
                let message = assistant_text_message(&ctx, &text, "");
                if let Err(err) = transcript.append(message).await {
                    return self.finish_failed(
                        &events,
                        ErrorCode::Internal,
                        format!("persist halt message: {err}"),
                    );
                }
                events.emit(EventPayload::TextCompleted {
                    text: String::clone(&text),
                });
                return self
                    .finish_completed(
                        &ctx,
                        &events,
                        &clock,
                        started_at,
                        text,
                        total_usage,
                        iterations,
                    )
                    .await;
            }

            if locked_reason.is_none() {
                if let Some(hint) = self
                    .advisor_hint(&task, &steps, None, &mut hints_given, &events)
                    .await
                {
                    if let Some(last) = request
                        .messages
                        .last_mut()
                        .filter(|m| m.role == MessageRole::Tool)
                    {
                        append_tool_result_notice(last, &hint);
                    }
                }
            }
        }
    }

    /// The advisor's hints that the model has not had yet this run, joined. Every check is
    /// shown to the user with what the advisor answered and what it handed the model.
    async fn advisor_hint(
        &self,
        task: &str,
        steps: &[Step],
        answer: Option<&str>,
        given: &mut HashSet<String>,
        events: &EventEmitter,
    ) -> Option<String> {
        let advice = self.advisor.as_ref()?.advise(task, steps, answer).await?;
        let fresh: Vec<String> = advice
            .hints
            .into_iter()
            .filter(|hint| given.insert(String::clone(hint)))
            .collect();
        let point = match (answer, steps.is_empty()) {
            (Some(_), _) => "answer",
            (None, true) => "start",
            (None, false) => "tools",
        };
        let hint = (!fresh.is_empty()).then(|| fresh.join("\n"));
        events.emit(EventPayload::AdvisorChecked {
            point: point.to_string(),
            answers: advice.answers,
            hints: fresh,
        });
        let hint = hint?;
        tracing::info!(%hint, "advisor hint");
        Some(hint)
    }

    /// Deliver any queued mid-turn steering as standalone user rows.
    async fn deliver_steering(
        &self,
        ctx: &Arc<RunContext>,
        messages: &mut Vec<ModelMessage>,
        transcript: &dyn TranscriptSink,
        events: &EventEmitter,
        control: &RunControl,
    ) -> Option<TurnOutcome> {
        for text in control.take_steering() {
            messages.push(ModelMessage::user(String::clone(&text)));
            let message = Message {
                id: MessageId::new(),
                session_id: ctx.session.id,
                run_id: Some(ctx.run_id),
                role: MessageRole::User,
                content: vec![ContentPart::text(String::clone(&text))],
                created_at: Utc::now(),
            };
            if let Err(err) = transcript.append(message).await {
                return Some(self.finish_failed(
                    events,
                    ErrorCode::Internal,
                    format!("persist steer: {err}"),
                ));
            }
            events.emit(EventPayload::SteerDelivered { text });
        }
        None
    }

    /// Answer a call that does not run with an error result, recorded by the guardrails like any
    /// failed call.
    #[expect(
        clippy::too_many_arguments,
        reason = "a refused call reaches the guardrails, the client, the model and the transcript"
    )]
    async fn refuse_call(
        ctx: &Arc<RunContext>,
        messages: &mut Vec<ModelMessage>,
        transcript: &dyn TranscriptSink,
        events: &EventEmitter,
        guardrails: &mut ToolCallGuardrailController,
        call: &ToolCallRequest,
        args: &serde_json::Value,
        status: ToolStatus,
        content: String,
    ) -> Option<TurnOutcome> {
        drop(guardrails.after_call(&call.name, args, Some(&content), Some(true)));
        drop(guardrails.observe_call(&call.name, args, Some(&content), &call.id.to_string(), true));
        Self::finish_call(
            ctx, messages, transcript, events, &call.id, status, None, content, None,
        )
        .await
    }

    /// Tell the client how a call ended, then hand its result to the model and the transcript.
    /// Without a summary the client is shown the result itself.
    #[expect(
        clippy::too_many_arguments,
        reason = "a call result reaches the client, the model and the transcript"
    )]
    async fn finish_call(
        ctx: &Arc<RunContext>,
        messages: &mut Vec<ModelMessage>,
        transcript: &dyn TranscriptSink,
        events: &EventEmitter,
        tool_call_id: &ToolCallId,
        status: ToolStatus,
        summary: Option<String>,
        content: String,
        image: Option<ContentPart>,
    ) -> Option<TurnOutcome> {
        events.emit(EventPayload::ToolCompleted {
            tool_call_id: ToolCallId::clone(tool_call_id),
            status,
            summary: summary.unwrap_or_else(|| String::clone(&content)),
        });
        let result = vec![ContentPart::ToolResult {
            tool_call_id: ToolCallId::clone(tool_call_id),
            content,
            is_error: status != ToolStatus::Completed,
        }];
        let mut turn = Vec::clone(&result);
        turn.extend(image);
        messages.push(ModelMessage {
            role: MessageRole::Tool,
            content: turn,
        });
        transcript
            .append(Message {
                id: MessageId::new(),
                session_id: ctx.session.id,
                run_id: Some(ctx.run_id),
                role: MessageRole::Tool,
                content: result,
                created_at: Utc::now(),
            })
            .await
            .map_err(|err| TurnOutcome::Failed {
                code: ErrorCode::Internal,
                message: format!("persist tool result: {err}"),
            })
            .err()
    }

    /// Persist an error result for each call from `answered` on that has none, so an interrupted
    /// turn never ends the transcript on a tool-call row, which strict providers reject.
    async fn close_interrupted_tool_sequence(
        &self,
        ctx: &Arc<RunContext>,
        messages: &mut Vec<ModelMessage>,
        transcript: &dyn TranscriptSink,
        events: &EventEmitter,
        calls: &[ToolCallRequest],
        answered: usize,
    ) -> Option<TurnOutcome> {
        for call in calls.iter().skip(answered) {
            if let Some(failure) = Self::finish_call(
                ctx,
                messages,
                transcript,
                events,
                &call.id,
                ToolStatus::Failed,
                None,
                INTERRUPTED_TOOL_RESULT.to_string(),
                None,
            )
            .await
            {
                return Some(failure);
            }
        }
        None
    }

    /// The silence budget for this run's model: the local one when the endpoint is on this
    /// machine or its LAN, the hosted one otherwise.
    fn stream_idle_budget(&self) -> Duration {
        if self.model.is_local() {
            self.config.local_model_timeout
        } else {
            self.config.model_timeout
        }
    }

    /// The reasoning budget for this run's model; see [`AgentConfig::reasoning_budget`].
    fn reasoning_budget(&self) -> Option<usize> {
        match self.config.reasoning_budget {
            Some(0) => None,
            Some(tokens) => Some(tokens),
            None => self.model.is_local().then_some(LOCAL_REASONING_TOKENS),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "streaming response handling is inherently long"
    )]
    async fn stream_response(
        &self,
        request: &ModelRequest,
        reasoning_budget: Option<usize>,
        control: &RunControl,
        events: &EventEmitter,
        clock: &Arc<ActivityClock>,
        total_usage: &mut Option<TokenUsage>,
    ) -> Result<ModelResponse, StreamFailure> {
        let idle_budget = self.stream_idle_budget();
        // Name the model the transport actually sends: a request model wins over the
        // transport's own, which for a routed model is only the config.toml default.
        let model_name = if request.model.is_empty() {
            self.model.name()
        } else {
            request.model.as_str()
        };
        let mut stream = match tokio::time::timeout(
            idle_budget,
            self.model
                .stream(ModelRequest::clone(request), control.cancel.child_token()),
        )
        .await
        {
            Err(_) => {
                return Err(StreamFailure::from_error(
                    &CoreError::ProviderUnavailable(format!(
                        "model request timeout: no response from {} in {}s",
                        model_name,
                        idle_budget.as_secs()
                    )),
                    false,
                ));
            }
            Ok(Err(err)) => return Err(self.stream_error(&err, false, control, "")),
            Ok(Ok(stream)) => stream,
        };

        let mut response = ModelResponse::default();
        let mut tool_acc: BTreeMap<usize, (Option<ToolCallId>, String, String)> = BTreeMap::new();
        // Provider index -> accumulator slot, for providers that send every call as index 0.
        let mut slots: HashMap<usize, usize> = HashMap::new();
        // Once any content or usage has been observed a retry could duplicate output or
        // double-count tokens, so the failure is terminal from then on.
        let mut emitted = false;
        let mut reasoning_tokens = 0usize;

        loop {
            if control.is_cancelled() {
                return Err(StreamFailure::Cancelled {
                    partial: response.text,
                });
            }
            let item = match self
                .next_stream_event(&mut stream, model_name, idle_budget, events, clock)
                .await
            {
                // A cancelled token aborts the transport mid-read, so the wait fails with
                // whatever the provider reports as it tears the stream down. That is the
                // user's Ctrl+C, not a broken provider: check the token before classifying.
                Err(err) => return Err(self.stream_error(&err, emitted, control, &response.text)),
                Ok(None) => break,
                Ok(Some(item)) => item,
            };
            match item {
                Err(err) => return Err(self.stream_error(&err, emitted, control, &response.text)),
                Ok(ModelStreamEvent::TextStarted) => {
                    // An external agent's message boundary: what it said before is not this
                    // turn's reply, so it must not reach the transcript or the chat.
                    response.text.clear();
                    events.emit(EventPayload::TextStarted);
                    clock.touch();
                }
                Ok(ModelStreamEvent::TextDelta(delta)) => {
                    emitted = true;
                    response.text.push_str(&delta);
                    events.emit(EventPayload::TextDelta { delta });
                    clock.touch();
                }
                Ok(ModelStreamEvent::ReasoningDelta(delta)) => {
                    emitted = true;
                    reasoning_tokens += crate::model_metadata::estimate_tokens_rough(&delta);
                    response.reasoning.push_str(&delta);
                    events.emit(EventPayload::ReasoningDelta { delta });
                    clock.touch();
                    // Only a model that has not started acting is cut. Dropping the stream
                    // closes the request, which is what stops a local server generating.
                    if reasoning_budget.is_some_and(|budget| reasoning_tokens > budget)
                        && response.text.is_empty()
                        && tool_acc.is_empty()
                    {
                        response.reasoning_cut = true;
                        break;
                    }
                }
                Ok(ModelStreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                }) => {
                    emitted = true;
                    clock.touch();
                    // Endpoints that omit 'index' (Gemini's, older Ollama's) send each call as
                    // index 0: a named delta with arguments after a finished call is a new one.
                    let mut slot = slots.get(&index).copied().unwrap_or(index);
                    let finished = tool_acc.get(&slot).is_some_and(|(_, name, raw)| {
                        !name.is_empty() && serde_json::from_str::<serde_json::Value>(raw).is_ok()
                    });
                    if name.is_some() && !arguments_delta.is_empty() && finished {
                        slot = tool_acc.keys().max().map_or(0, |last| last + 1);
                        slots.insert(index, slot);
                    }
                    let entry = tool_acc.entry(slot).or_default();
                    if id.is_some() {
                        entry.0 = id;
                    }
                    // Some servers repeat the whole name on every delta; append only fragments.
                    if let Some(name) = name.filter(|name| *name != entry.1) {
                        entry.1.push_str(&name);
                    }
                    entry.2.push_str(&arguments_delta);
                }
                Ok(ModelStreamEvent::Usage(usage)) => {
                    emitted = true;
                    response.usage = Some(usage);
                    *total_usage = Some(merge_usage(*total_usage, usage));
                }
                Ok(ModelStreamEvent::Finish(reason)) => {
                    response.finish_reason = Some(reason);
                }
            }
        }

        // Repair tool ids: some providers omit the id on the first delta or repeat one across
        // parallel calls, and strict providers reject divergent tool/tool_call rows. A
        // provider's own id is kept otherwise: it is what the result will be matched on.
        for (_, (id, name, raw)) in tool_acc {
            let mut call_id = id.unwrap_or_default();
            if call_id == ToolCallId::default()
                || response.tool_calls.iter().any(|call| call.id == call_id)
            {
                call_id = ToolCallId::new();
            }
            let (arguments, malformed_arguments) = match serde_json::from_str(&raw) {
                Ok(value) => (value, None),
                Err(_) if raw.trim().is_empty() => {
                    (serde_json::Value::Object(Default::default()), None)
                }
                Err(error) => (
                    serde_json::Value::Object(Default::default()),
                    Some(format!("invalid tool arguments JSON: {error}")),
                ),
            };
            response.tool_calls.push(ToolCallRequest {
                id: call_id,
                name,
                arguments,
                raw_arguments: raw,
                malformed_arguments,
            });
        }
        response.empty = response.text.is_empty()
            && response.reasoning.is_empty()
            && response.tool_calls.is_empty();
        response.observed_generation = !response.empty;
        Ok(response)
    }

    /// Wait for the next stream event within `idle_budget` of silence, stamping the liveness clock
    /// every [`STREAM_WAIT_HEARTBEAT`] so the watchdog never pre-empts this wait. After
    /// [`STREAM_WAIT_NOTICE_AFTER`] the user is told what is awaited. `Err` is the stall itself.
    async fn next_stream_event(
        &self,
        stream: &mut ModelStream,
        model_name: &str,
        idle_budget: Duration,
        events: &EventEmitter,
        clock: &Arc<ActivityClock>,
    ) -> CoreResult<Option<CoreResult<ModelStreamEvent>>> {
        // Tokio's clock, so the budget follows the same time the timeouts below run on.
        let started = tokio::time::Instant::now();
        let mut last_notice: Option<tokio::time::Instant> = None;
        loop {
            let remaining = idle_budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(CoreError::ProviderUnavailable(format!(
                    "model stream stalled: no output from {} for {}s",
                    model_name,
                    idle_budget.as_secs()
                )));
            }
            // `StreamExt::next` is cancel-safe: dropping the timed-out future loses nothing.
            match tokio::time::timeout(remaining.min(STREAM_WAIT_HEARTBEAT), stream.next()).await {
                Ok(item) => return Ok(item),
                Err(_) => {
                    let waited = started.elapsed();
                    clock.touch();
                    let due = waited >= STREAM_WAIT_NOTICE_AFTER
                        && last_notice.is_none_or(|at| at.elapsed() >= STREAM_WAIT_NOTICE_EVERY);
                    if due {
                        last_notice = Some(tokio::time::Instant::now());
                        events.emit(EventPayload::RunWaiting {
                            reason: format!(
                                "no output from {} for {}s (the model may be thinking or \
                                 generating a long tool call; giving up at {}s)",
                                model_name,
                                waited.as_secs(),
                                idle_budget.as_secs()
                            ),
                        });
                    }
                }
            }
        }
    }

    /// Classify a streaming error, except when the run was cancelled: aborting the transport
    /// is how cancellation reaches an in-flight read, and the error it leaves behind
    /// describes the teardown, not a fault worth reporting or retrying.
    fn stream_error(
        &self,
        err: &CoreError,
        emitted: bool,
        control: &RunControl,
        partial: &str,
    ) -> StreamFailure {
        let partial = partial.to_string();
        if control.is_cancelled() {
            return StreamFailure::Cancelled { partial };
        }
        let mut failure = StreamFailure::from_error(err, emitted);
        if let StreamFailure::Provider { partial: slot, .. } = &mut failure {
            *slot = partial;
        }
        failure
    }

    fn finish_failed(
        &self,
        events: &EventEmitter,
        code: ErrorCode,
        message: impl Into<String>,
    ) -> TurnOutcome {
        let message = message.into();
        events.emit(EventPayload::RunFailed {
            code,
            message: String::clone(&message),
        });
        TurnOutcome::Failed { code, message }
    }

    /// Emit run.completed and end the turn with `text` as its answer.
    #[expect(
        clippy::too_many_arguments,
        reason = "agent turn orchestration needs all collaborators"
    )]
    async fn finish_completed(
        &self,
        ctx: &RunContext,
        events: &EventEmitter,
        clock: &Arc<ActivityClock>,
        started_at: DateTime<Utc>,
        text: String,
        usage: Option<TokenUsage>,
        iterations: u32,
    ) -> TurnOutcome {
        let cost_usd = run_cost_usd(self.model_price(&ctx.model).await, usage);
        events.emit(EventPayload::RunCompleted {
            usage,
            cost_usd,
            duration_ms: started_at
                .signed_duration_since(Utc::now())
                .num_milliseconds()
                .unsigned_abs(),
        });
        clock.deactivate_turn();
        TurnOutcome::Completed {
            text,
            usage,
            cost_usd,
            iterations,
        }
    }

    fn finish_cancelled(
        &self,
        events: &EventEmitter,
        clock: &Arc<ActivityClock>,
        control: &RunControl,
    ) -> TurnOutcome {
        let origin = control.take_origin();
        events.emit(EventPayload::RunCancelled {
            origin: String::clone(&origin),
        });
        clock.deactivate_turn();
        TurnOutcome::Cancelled { origin }
    }
}

enum StreamFailure {
    /// The reply text streamed before the stop.
    Cancelled { partial: String },
    Provider {
        code: ErrorCode,
        message: String,
        /// Server-provided `Retry-After` hint, if the transport saw one.
        retry_after: Option<Duration>,
        /// Whether the failure is transient and may be retried.
        retryable: bool,
        /// Whether any content or usage was already observed (retry would duplicate).
        emitted: bool,
        /// The reply text streamed before the failure.
        partial: String,
    },
}

impl StreamFailure {
    /// Classify a provider error and attach whether the stream was already dirty.
    fn from_error(err: &CoreError, emitted: bool) -> Self {
        let overflow = err.is_context_overflow();
        let (retryable, retry_after) = match err.retry_class() {
            RetryClass::Transient { retry_after } => (true, retry_after),
            RetryClass::Terminal => (false, None),
        };
        StreamFailure::Provider {
            // Normalise every overflow shape to one code so the loop has a single branch.
            code: if overflow {
                ErrorCode::ContextTooLarge
            } else {
                err.code()
            },
            message: err.to_string(),
            retry_after,
            retryable,
            emitted,
            partial: String::new(),
        }
    }
}

/// Delay before retry `attempt` (1-based): the server's `Retry-After` capped by `max`, else
/// exponential backoff with jitter in `[capped/2, capped]`.
fn retry_delay(
    attempt: u32,
    retry_after: Option<Duration>,
    base: Duration,
    max: Duration,
) -> Duration {
    if let Some(server) = retry_after {
        return server.min(max);
    }
    let base_ms = base.as_millis().max(1) as u64;
    let max_ms = max.as_millis().max(1) as u64;
    let factor = 1u64 << attempt.min(10);
    let capped = base_ms.saturating_mul(factor).min(max_ms);
    Duration::from_millis(jitter_between(capped / 2, capped))
}

/// Cheap pseudo-random value in `[low, high]` without pulling in an RNG crate.
fn jitter_between(low: u64, high: u64) -> u64 {
    if high <= low {
        return low;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64 ^ elapsed.subsec_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let mut x = seed | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    low + (x % (high - low + 1))
}

/// User-role prompt that asks the model to continue an output-limit-truncated reply.
const TRUNCATION_CONTINUATION_PROMPT: &str = "Your previous reply was cut off by the output token limit. Continue exactly where you left off. Do not repeat any text you already produced and do not add a preamble.";

const REASONING_CUT_PROMPT: &str = "You have thought long enough. Stop deliberating and act now: make one tool call, or give your answer.";

/// Error content persisted for a tool call the model announced but that never ran.
const INTERRUPTED_TOOL_RESULT: &str = "[interrupted before the tool ran]";

/// How often a silent model stream stamps the liveness clock while its stall budget runs.
const STREAM_WAIT_HEARTBEAT: Duration = Duration::from_secs(30);

/// Silence after which the user is told what the turn is waiting on.
const STREAM_WAIT_NOTICE_AFTER: Duration = Duration::from_secs(60);

/// Minimum spacing between two such notices.
const STREAM_WAIT_NOTICE_EVERY: Duration = Duration::from_secs(60);

/// The only programmatic (RPC-style) tool whose round is refunded from the iteration budget.
const PROGRAMMATIC_TOOL_NAME: &str = "execute_code";

/// The one-time budget notice once `used` reaches `ratio` of `max`; `ratio` comes from
/// [`normalize_budget_warning_ratio`], and None disables it.
fn iteration_budget_warning(used: u32, max: u32, ratio: Option<f64>) -> Option<String> {
    let ratio = ratio?;
    if max == 0 {
        return None;
    }
    let threshold = ((ratio * f64::from(max)).ceil() as u32).max(1);
    if used < threshold {
        return None;
    }
    Some(format!(
        "[SYSTEM NOTICE — iteration budget checkpoint] You have used {used} of {max} iterations. \
         Checkpoint durable progress now, then continue the task; do not stop solely because of \
         this warning."
    ))
}

/// `path` relative to the workspace when inside it, for a short label.
fn shown_path(ctx: &RunContext, path: &std::path::Path) -> String {
    ctx.cwd()
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Append `notice` to the tool-result part of the (tail) tool message.
fn append_tool_result_notice(message: &mut ModelMessage, notice: &str) {
    for part in &mut message.content {
        if let ContentPart::ToolResult { content, .. } = part {
            content.push_str("\n\n");
            content.push_str(notice);
            return;
        }
    }
}

/// System prompt for the one-shot session title upgrade.
const TITLE_SYSTEM_PROMPT: &str = "You name chat sessions. Given the user's opening message and the assistant's first reply, write a short session title of 3 to 7 words that names what the user wants done. No quotes, no trailing punctuation, no tool names, and never answer the message. Reply with the title only.";

/// Output token budget for the title call.
const TITLE_MAX_TOKENS: u32 = 64;

/// Wall-clock bound on the title call.
const TITLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest user/assistant excerpt handed to the title model.
const TITLE_INPUT_MAX_CHARS: usize = 1000;

/// Longest accepted model title.
const TITLE_MAX_CHARS: usize = 80;

/// Upper bound on title words; a longer output is an answer, not a title.
const TITLE_MAX_WORDS: usize = 12;

/// Drive one auxiliary model request to completion, returning its text or None on failure.
async fn call_aux_text(
    model: &Arc<dyn Model>,
    request: ModelRequest,
    timeout: Duration,
    label: &'static str,
) -> Option<String> {
    let cancel = CancellationToken::new();
    // One total deadline covers opening the stream and draining it, so a provider that dribbles
    // events cannot hold the turn past its bound.
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
    let text = match tokio::time::timeout(timeout, collect).await {
        Err(_) => {
            tracing::warn!(
                aux_call = label,
                outcome = "timeout",
                "auxiliary model call timed out"
            );
            return None;
        }
        Ok(Err(err)) => {
            // Log only the classified code: a provider error body can echo request content.
            tracing::warn!(
                aux_call = label,
                outcome = "error",
                error_code = err.code().as_str(),
                "auxiliary model call failed"
            );
            return None;
        }
        Ok(Ok(text)) => text.trim().to_string(),
    };
    if text.is_empty() {
        tracing::warn!(
            aux_call = label,
            outcome = "empty",
            "auxiliary model returned no text"
        );
        return None;
    }
    Some(text)
}

/// Normalize a model-produced title; None when nothing usable remains.
fn clean_generated_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let without_prefix = trimmed
        .strip_prefix("Title:")
        .or_else(|| trimmed.strip_prefix("title:"))
        .unwrap_or(trimmed);
    let line = without_prefix
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = line.trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == '`');
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.split_whitespace().count() > TITLE_MAX_WORDS {
        return None;
    }
    Some(truncate_chars(&collapsed, TITLE_MAX_CHARS))
}

fn to_model_message(message: Message) -> ModelMessage {
    ModelMessage {
        role: message.role,
        content: message.content,
    }
}

/// The reply as an assistant turn. Takes its text and reasoning; the tool calls stay with the
/// response for the loop that runs them.
fn to_model_message_from_response(response: &mut ModelResponse) -> ModelMessage {
    let mut content: Vec<ContentPart> = Vec::new();
    if !response.reasoning.is_empty() {
        content.push(ContentPart::reasoning(std::mem::take(
            &mut response.reasoning,
        )));
    }
    if !response.text.is_empty() {
        content.push(ContentPart::text(std::mem::take(&mut response.text)));
    }
    for call in &response.tool_calls {
        content.push(ContentPart::ToolCall {
            id: ToolCallId::clone(&call.id),
            name: String::clone(&call.name),
            arguments: serde_json::Value::clone(&call.arguments),
        });
    }
    ModelMessage {
        role: MessageRole::Assistant,
        content,
    }
}

/// The end of cut-off reasoning as the model's own words (see [`REASONING_CUT_TAIL_BYTES`]).
fn reasoning_cut_message(reasoning: &str) -> ModelMessage {
    let mut start = reasoning.len().saturating_sub(REASONING_CUT_TAIL_BYTES);
    while !reasoning.is_char_boundary(start) {
        start += 1;
    }
    ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ContentPart::text(format!(
            "(My thinking so far, cut off:) …{}",
            &reasoning[start..]
        ))],
    }
}

fn assistant_text_message(ctx: &Arc<RunContext>, text: &str, reasoning: &str) -> Message {
    let mut content: Vec<ContentPart> = Vec::new();
    if !reasoning.is_empty() {
        content.push(ContentPart::reasoning(reasoning));
    }
    content.push(ContentPart::text(text));
    Message {
        id: MessageId::new(),
        session_id: ctx.session.id,
        run_id: Some(ctx.run_id),
        role: MessageRole::Assistant,
        content,
        created_at: Utc::now(),
    }
}

/// Approximate USD list-price estimate for the accumulated usage of the model.
///
/// Returns None for an unknown price or when the provider reported no usage.
fn run_cost_usd(price: Option<ModelPrice>, usage: Option<TokenUsage>) -> Option<f64> {
    Some(crate::pricing::estimate_cost_usd_for_usage(
        &price?,
        crate::pricing::UsageCostInput {
            prompt_tokens: usage?.prompt_tokens,
            completion_tokens: usage?.completion_tokens,
            cached_tokens: usage?.cached_tokens,
            // The transport reports cache reads only; cache-creation tokens ride in the
            // prompt total and bill at the base input rate.
            cache_write_tokens: 0,
            reasoning_tokens: usage?.reasoning_tokens,
        },
    ))
}

fn merge_usage(existing: Option<TokenUsage>, addition: TokenUsage) -> TokenUsage {
    match existing {
        None => addition,
        Some(prev) => TokenUsage {
            prompt_tokens: prev.prompt_tokens.saturating_add(addition.prompt_tokens),
            completion_tokens: prev
                .completion_tokens
                .saturating_add(addition.completion_tokens),
            total_tokens: prev.total_tokens.saturating_add(addition.total_tokens),
            cached_tokens: prev.cached_tokens.saturating_add(addition.cached_tokens),
            reasoning_tokens: prev
                .reasoning_tokens
                .saturating_add(addition.reasoning_tokens),
        },
    }
}

fn finish_reason_label(reason: &Option<FinishReason>) -> String {
    match reason {
        None => "unknown".to_string(),
        Some(FinishReason::Stop) => "stop".to_string(),
        Some(FinishReason::ToolCalls) => "tool_calls".to_string(),
        Some(FinishReason::Length) => "length".to_string(),
        Some(FinishReason::ContentFilter) => "content_filter".to_string(),
        Some(FinishReason::Other(other)) => String::clone(other),
    }
}

/// The persisted tool-result summary: the envelope verbatim with long strings cut in the middle,
/// because clients parse `output`/`content`/`text`/`result` from it live and after reload. A
/// non-object is cut as a whole.
pub fn summarize_tool_outcome(content: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(content.trim()) {
        Ok(serde_json::Value::Object(mut map)) => {
            for key in ["output", "content", "text", "result"] {
                if let Some(serde_json::Value::String(text)) = map.get(key) {
                    let cut = middle_cut(text, PREVIEW_CHARS);
                    map.insert(key.to_string(), serde_json::Value::String(cut));
                }
            }
            serde_json::Value::Object(map).to_string()
        }
        _ => middle_cut(content, PREVIEW_CHARS),
    }
}

/// Cut `text` to `max` characters, keeping whole lines from both ends and naming how many were
/// dropped. The tail gets three times the head's budget: output ends in its verdict.
fn middle_cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 1 {
        return truncate_chars(text, max);
    }

    fn take_within<'a>(order: impl Iterator<Item = &'a str>, budget: usize) -> Vec<&'a str> {
        let mut kept = Vec::new();
        let mut used = 0;
        for line in order {
            let cost = line.chars().count() + 1;
            if used + cost > budget {
                break;
            }
            used += cost;
            kept.push(line);
        }
        kept
    }
    let head = take_within(lines.iter().copied(), max / 4);
    let mut tail = take_within(lines.iter().copied().rev(), max - max / 4);
    tail.reverse();

    let cut = lines.len().saturating_sub(head.len() + tail.len());
    if cut == 0 {
        return truncate_chars(text, max);
    }
    let mut out = head.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!(
        "[… {cut} line{} cut]",
        if cut == 1 { "" } else { "s" }
    ));
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(&tail.join("\n"));
    }
    out
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("...");
    out
}

/// How much of a string a transcript preview or tool summary keeps. An approval must show enough of
/// the diff to decide on: at 200 characters a 58-line write showed three lines.
const PREVIEW_CHARS: usize = 4_000;

/// Redact likely secrets and truncate long strings before putting arguments in an event.
pub fn sanitize_preview(value: &serde_json::Value) -> serde_json::Value {
    sanitize_value(value, 0, PREVIEW_CHARS)
}

fn sanitize_value(value: &serde_json::Value, depth: usize, max: usize) -> serde_json::Value {
    if depth > 6 {
        return serde_json::Value::String("[depth-limit]".into());
    }
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(crate::redact::redact(&truncate_chars(s, max)))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| sanitize_value(v, depth + 1, max))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                let lower = key.to_ascii_lowercase();
                let shown = if lower.contains("api_key")
                    || lower.contains("apikey")
                    || lower.contains("token")
                    || lower.contains("secret")
                    || lower.contains("password")
                {
                    serde_json::Value::String("[redacted]".into())
                } else {
                    sanitize_value(val, depth + 1, max)
                };
                out.insert(key.clone(), shown);
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}
