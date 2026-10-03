//! Tool-call loop guardrails: a side-effect-free controller that tracks a turn's calls and returns
//! decisions the loop turns into guidance, a synthetic result or a halt.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};

use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};

/// Read-only tools whose repeated identical result is non-progress.
pub const IDEMPOTENT_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_files",
    "search_files",
    "view_image",
    "search_text",
    "web_search",
    "web_extract",
    "session_search",
    "search_documents",
    "skill_view",
    "skills_list",
    "browser_snapshot",
    "browser_console",
    "browser_get_images",
    "mcp_filesystem_read_file",
    "mcp_filesystem_read_text_file",
    "mcp_filesystem_read_multiple_files",
    "mcp_filesystem_list_directory",
    "mcp_filesystem_list_directory_with_sizes",
    "mcp_filesystem_directory_tree",
    "mcp_filesystem_get_file_info",
    "mcp_filesystem_search_files",
];

/// Tools that mutate state; a success resets failing streaks.
pub const MUTATING_TOOL_NAMES: &[&str] = &[
    "bash",
    "run_command",
    "execute_code",
    "write_file",
    "patch",
    "apply_patch",
    "todo_list",
    "memory",
    "skill_manage",
    "browser_click",
    "browser_type",
    "browser_press",
    "browser_scroll",
    "browser_navigate",
    "send_message",
    "cronjob_manage",
    "delegate_task",
    "process_manage",
];

// Pollers: legitimately re-invoked with identical args; the identical-call NOTICE never fires.
const STALL_GUARD_REPEATABLE_TOOLS: &[&str] = &["process_manage"];
const STALL_GUARD_REPEATABLE_SUFFIXES: &[&str] = &["_get_result", "_poll"];
/// Nth consecutive identical (tool, args, result) call that fires the notice; 3 tolerates one
/// double-check.
pub const STALL_GUARD_IDENTICAL_CALL_THRESHOLD: usize = 3;
// Longest cycle period detected; laps reuse the streak thresholds.
const STALL_GUARD_MAX_CYCLE_PERIOD: usize = 4;
// History window: enough for block_after laps of the longest cycle plus slack.
const STALL_GUARD_CYCLE_HISTORY: usize = 64;
/// From the 2nd byte-identical repeat the duplicate payload becomes a reference stub.
pub const IDENTICAL_RESULT_STUB_MIN_CHARS: usize = 512;
const RESULT_STUB_ARGS_PREVIEW_CHARS: usize = 120;

// Tools whose "failure" is normal work output (red test run, empty grep, page timeout, a
// path that does not exist). same_tool_failure (DIFFERENT commands) never halts these; only
// an exact-args replay with no intervening change, or an identical-result streak, can.
const FAILURE_TOLERANT_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_files",
    "search_files",
    "bash",
    "run_command",
    "execute_code",
    "process_manage",
    "process",
    "browser_navigate",
    "web_extract",
];

// A successful call to one of these marks progress for every failing signature still counted
// this turn: the next retry is a new experiment (edit -> re-run), not a replay.
const PROGRESS_RESET_TOOL_NAMES: &[&str] = &[
    "write_file",
    "patch",
    "apply_patch",
    "bash",
    "run_command",
    "execute_code",
    "browser_click",
    "browser_type",
    "browser_press",
    "browser_navigate",
    "process_manage",
    "process",
    "delegate_task",
    "send_message",
    "cronjob",
    "cronjob_manage",
    "todo",
    "todo_list",
    "memory",
    "skill_manage",
];

// Per-turn caps on runaway-prone tools (counters reset in reset_for_turn).
const DEFAULT_MAX_WEB_SEARCHES_PER_TURN: u32 = 50;
const DEFAULT_MAX_SUBAGENTS_PER_TURN: u32 = 50;

// Interactive surfaces plus bounded supervised task loops (subagent stopped by its parent;
// api_server has a live client) doing real edit -> re-run work keep the warn-only default.
const ATTENDED_PLATFORMS: &[&str] = &["cli", "tui", "desktop", "acp", "subagent", "api_server"];

/// Whether a tool is exempt from the identical-call loop notice.
pub fn is_stall_guard_repeatable(tool_name: &str) -> bool {
    STALL_GUARD_REPEATABLE_TOOLS.contains(&tool_name)
        || STALL_GUARD_REPEATABLE_SUFFIXES
            .iter()
            .any(|suffix| tool_name.ends_with(suffix))
}

/// True for gateway/cron sessions where tool loops are unattended.
fn is_non_interactive_platform(platform: Option<&str>) -> bool {
    match platform {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return false;
            }
            let lower = trimmed.to_lowercase();
            !ATTENDED_PLATFORMS.iter().any(|p| *p == lower)
        }
        None => false,
    }
}

/// Per-turn hard ceilings on web_search calls / subagent spawns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopCapConfig {
    pub max_web_searches: u32,
    pub max_subagents: u32,
}

impl Default for LoopCapConfig {
    fn default() -> Self {
        Self {
            max_web_searches: DEFAULT_MAX_WEB_SEARCHES_PER_TURN,
            max_subagents: DEFAULT_MAX_SUBAGENTS_PER_TURN,
        }
    }
}

impl LoopCapConfig {
    /// Build config from the tool_loop_guardrails.loop_caps section.
    pub fn from_mapping(v: &Value) -> Self {
        let fallback = LoopCapConfig::default();
        let Some(map) = v.as_object() else {
            return fallback;
        };
        let web = int_at_least(
            map.get("max_web_searches"),
            fallback.max_web_searches as i64,
            0,
        );
        let subs = int_at_least(map.get("max_subagents"), fallback.max_subagents as i64, 0);
        Self {
            max_web_searches: u32::try_from(web).unwrap_or(fallback.max_web_searches),
            max_subagents: u32::try_from(subs).unwrap_or(fallback.max_subagents),
        }
    }
}

/// Thresholds for per-turn tool-call loop detection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToolCallGuardrailConfig {
    pub warnings_enabled: bool,
    pub hard_stop_enabled: bool,
    pub non_interactive_hard_stop_enabled: bool,
    pub exact_failure_warn_after: usize,
    pub exact_failure_block_after: usize,
    pub same_tool_failure_warn_after: usize,
    pub same_tool_failure_halt_after: usize,
    pub no_progress_warn_after: usize,
    pub no_progress_block_after: usize,
    pub loop_caps: LoopCapConfig,
}

impl Default for ToolCallGuardrailConfig {
    fn default() -> Self {
        Self {
            warnings_enabled: true,
            hard_stop_enabled: false,
            non_interactive_hard_stop_enabled: true,
            exact_failure_warn_after: 2,
            exact_failure_block_after: 5,
            same_tool_failure_warn_after: 3,
            same_tool_failure_halt_after: 8,
            no_progress_warn_after: 2,
            no_progress_block_after: 5,
            loop_caps: LoopCapConfig::default(),
        }
    }
}

impl ToolCallGuardrailConfig {
    /// Build config from tool_loop_guardrails; nested warn_after / hard_stop_after win over
    /// flat legacy keys.
    pub fn from_mapping(v: &Value, platform: Option<&str>) -> Self {
        let fallback = ToolCallGuardrailConfig::default();
        let empty = Map::new();
        let map = v.as_object().unwrap_or(&empty);

        let warnings_enabled = as_bool(map.get("warnings_enabled"), fallback.warnings_enabled);
        let mut hard_stop_enabled =
            as_bool(map.get("hard_stop_enabled"), fallback.hard_stop_enabled);
        let non_interactive_hard_stop_enabled = as_bool(
            map.get("non_interactive_hard_stop_enabled"),
            fallback.non_interactive_hard_stop_enabled,
        );
        if non_interactive_hard_stop_enabled && is_non_interactive_platform(platform) {
            hard_stop_enabled = true;
        }

        let threshold = |name: &str, section_name: &str, key: &str, default: usize| -> usize {
            let nested = match map.get(section_name) {
                Some(Value::Object(section)) => section.get(key).or_else(|| map.get(name)),
                _ => map.get(name),
            };
            let parsed = int_at_least(nested, default as i64, 1);
            usize::try_from(parsed).unwrap_or(default)
        };

        let exact_failure_warn_after = threshold(
            "exact_failure_warn_after",
            "warn_after",
            "exact_failure",
            fallback.exact_failure_warn_after,
        );
        let same_tool_failure_warn_after = threshold(
            "same_tool_failure_warn_after",
            "warn_after",
            "same_tool_failure",
            fallback.same_tool_failure_warn_after,
        );
        let no_progress_warn_after = threshold(
            "no_progress_warn_after",
            "warn_after",
            "idempotent_no_progress",
            fallback.no_progress_warn_after,
        );
        let exact_failure_block_after = threshold(
            "exact_failure_block_after",
            "hard_stop_after",
            "exact_failure",
            fallback.exact_failure_block_after,
        );
        let same_tool_failure_halt_after = threshold(
            "same_tool_failure_halt_after",
            "hard_stop_after",
            "same_tool_failure",
            fallback.same_tool_failure_halt_after,
        );
        let no_progress_block_after = threshold(
            "no_progress_block_after",
            "hard_stop_after",
            "idempotent_no_progress",
            fallback.no_progress_block_after,
        );

        let loop_caps = match map.get("loop_caps") {
            Some(value) => LoopCapConfig::from_mapping(value),
            None => LoopCapConfig::default(),
        };

        Self {
            warnings_enabled,
            hard_stop_enabled,
            non_interactive_hard_stop_enabled,
            exact_failure_warn_after,
            exact_failure_block_after,
            same_tool_failure_warn_after,
            same_tool_failure_halt_after,
            no_progress_warn_after,
            no_progress_block_after,
            loop_caps,
        }
    }
}

/// Action attached to a guardrail decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardrailAction {
    Allow,
    Warn,
    Block,
    Halt,
}

impl GuardrailAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Block => "block",
            Self::Halt => "halt",
        }
    }
}

/// A SHA-256 digest; hex only where it is shown.
type Fingerprint = [u8; 32];

/// Stable, non-reversible identity for a tool name plus canonical args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolCallSignature {
    tool: Fingerprint,
    pub args_hash: Fingerprint,
}

impl ToolCallSignature {
    pub fn from_call(tool_name: &str, args: &Value) -> Self {
        let coerced = coerce_args(args);
        Self {
            tool: sha256(tool_name),
            args_hash: sha256(&canonical_tool_args(&coerced)),
        }
    }

    /// Public metadata without raw argument values.
    pub fn to_metadata(&self, tool_name: &str) -> Value {
        json!({ "tool_name": tool_name, "args_hash": hex::encode(self.args_hash) })
    }
}

/// Decision returned by the tool-call guardrail controller.
#[derive(Clone, Debug)]
pub struct ToolGuardrailDecision {
    pub action: GuardrailAction,
    pub code: String,
    pub message: String,
    pub tool_name: String,
    pub count: usize,
    pub signature: Option<ToolCallSignature>,
}

impl Default for ToolGuardrailDecision {
    fn default() -> Self {
        Self {
            action: GuardrailAction::Allow,
            code: "allow".to_string(),
            message: String::new(),
            tool_name: String::new(),
            count: 0,
            signature: None,
        }
    }
}

impl ToolGuardrailDecision {
    pub fn allows_execution(&self) -> bool {
        matches!(self.action, GuardrailAction::Allow | GuardrailAction::Warn)
    }

    pub fn should_halt(&self) -> bool {
        matches!(self.action, GuardrailAction::Block | GuardrailAction::Halt)
    }

    pub fn to_metadata(&self) -> Value {
        let mut meta = json!({
            "action": self.action.as_str(),
            "code": self.code,
            "message": self.message,
            "tool_name": self.tool_name,
            "count": self.count,
        });
        if let Some(signature) = &self.signature {
            meta["signature"] = signature.to_metadata(&self.tool_name);
        }
        meta
    }
}

/// Result of observing one tool call for byte-identical duplication.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdenticalCallObservation {
    pub notice: Option<String>,
    pub stub: Option<String>,
}

/// Per-turn controller for repeated failed or non-progressing tool calls.
#[derive(Debug)]
pub struct ToolCallGuardrailController {
    config: ToolCallGuardrailConfig,
    exact_failure_counts: HashMap<ToolCallSignature, usize>,
    same_tool_failure_counts: HashMap<String, usize>,
    // signature -> a mutating call succeeded since its last failure
    progress_since_failure: HashMap<ToolCallSignature, bool>,
    no_progress: HashMap<ToolCallSignature, (Fingerprint, usize)>,
    halt_decision: Option<ToolGuardrailDecision>,
    // Consecutive identical (tool, args, result) calls; any different call or result resets it.
    identical_streak_sig: Option<ToolCallSignature>,
    identical_streak_result_hash: Fingerprint,
    identical_streak_count: usize,
    identical_streak_first_call_id: String,
    // Sequence of (signature, result_hash, repeatable) for every observed call this turn.
    call_history: VecDeque<(ToolCallSignature, Fingerprint, bool)>,
    // tool_call_id -> spillover path, so a stub referencing a persisted preview cannot dangle.
    persisted_result_paths: HashMap<String, String>,
    turn_web_search_count: u32,
    turn_subagent_count: u32,
}

impl ToolCallGuardrailController {
    pub fn new(config: ToolCallGuardrailConfig) -> Self {
        let mut controller = Self {
            config,
            exact_failure_counts: HashMap::new(),
            same_tool_failure_counts: HashMap::new(),
            progress_since_failure: HashMap::new(),
            no_progress: HashMap::new(),
            halt_decision: None,
            identical_streak_sig: None,
            identical_streak_result_hash: Fingerprint::default(),
            identical_streak_count: 0,
            identical_streak_first_call_id: String::new(),
            call_history: VecDeque::new(),
            persisted_result_paths: HashMap::new(),
            turn_web_search_count: 0,
            turn_subagent_count: 0,
        };
        controller.reset_for_turn();
        controller
    }

    pub fn config(&self) -> &ToolCallGuardrailConfig {
        &self.config
    }

    pub fn reset_for_turn(&mut self) {
        self.exact_failure_counts.clear();
        self.same_tool_failure_counts.clear();
        self.progress_since_failure.clear();
        self.no_progress.clear();
        self.halt_decision = None;
        self.identical_streak_sig = None;
        self.identical_streak_result_hash = Fingerprint::default();
        self.identical_streak_count = 0;
        self.identical_streak_first_call_id = String::new();
        self.call_history.clear();
        self.persisted_result_paths.clear();
        self.turn_web_search_count = 0;
        self.turn_subagent_count = 0;
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "guardrail decision needs all context"
    )]
    fn decide(
        &mut self,
        action: GuardrailAction,
        code: &str,
        tool_name: &str,
        count: usize,
        signature: ToolCallSignature,
        message: Option<String>,
        period: Option<usize>,
        cap: Option<u32>,
    ) -> ToolGuardrailDecision {
        let message =
            message.unwrap_or_else(|| decision_message(code, tool_name, count, period, cap));
        let decision = ToolGuardrailDecision {
            action,
            code: code.to_string(),
            message,
            tool_name: tool_name.to_string(),
            count,
            signature: Some(signature),
        };
        if decision.should_halt() {
            self.halt_decision = Some(ToolGuardrailDecision::clone(&decision));
        }
        decision
    }

    pub fn before_call(&mut self, tool_name: &str, args: &Value) -> ToolGuardrailDecision {
        let args = coerce_args(args);
        let signature = ToolCallSignature::from_call(tool_name, &args);
        let allow = ToolGuardrailDecision {
            tool_name: tool_name.to_string(),
            signature: Some(signature),
            ..Default::default()
        };

        // Loop caps apply regardless of hard_stop_enabled (which only governs the detector).
        if let Some(cap_block) = self.check_loop_cap(tool_name, &args, signature) {
            return cap_block;
        }
        if !self.config.hard_stop_enabled {
            return allow;
        }
        // A mutation since this call last failed makes the retry a new experiment.
        let progress = self
            .progress_since_failure
            .get(&signature)
            .copied()
            .unwrap_or(false);
        let exact_count = if progress {
            0
        } else {
            self.exact_failure_counts
                .get(&signature)
                .copied()
                .unwrap_or(0)
        };
        if exact_count >= self.config.exact_failure_block_after {
            return self.decide(
                GuardrailAction::Block,
                "repeated_exact_failure_block",
                tool_name,
                exact_count,
                signature,
                None,
                None,
                None,
            );
        }
        let idempotent_repeat = if self.is_idempotent(tool_name) {
            self.no_progress.get(&signature).map(|(_, count)| *count)
        } else {
            None
        };
        if let Some(repeat) = idempotent_repeat {
            if repeat >= self.config.no_progress_block_after {
                return self.decide(
                    GuardrailAction::Block,
                    "idempotent_no_progress_block",
                    tool_name,
                    repeat,
                    signature,
                    None,
                    None,
                    None,
                );
            }
        }
        allow
    }

    #[expect(
        clippy::too_many_lines,
        reason = "guardrail after_call is inherently long"
    )]
    pub fn after_call(
        &mut self,
        tool_name: &str,
        args: &Value,
        result: Option<&str>,
        failed: Option<bool>,
    ) -> ToolGuardrailDecision {
        let args = coerce_args(args);
        let signature = ToolCallSignature::from_call(tool_name, &args);
        let failed = failed.unwrap_or_else(|| classify_tool_failure(tool_name, result).0);
        let warnings = self.config.warnings_enabled;

        if failed {
            // An identical failing call is only a REPLAY if nothing landed in between;
            // a mutation since the last identical failure restarts the exact-args streak.
            if self
                .progress_since_failure
                .remove(&signature)
                .unwrap_or(false)
            {
                self.exact_failure_counts.remove(&signature);
            }
            let exact_count = {
                let entry = self.exact_failure_counts.entry(signature).or_insert(0);
                *entry += 1;
                *entry
            };
            let same_count = {
                let entry = self
                    .same_tool_failure_counts
                    .entry(tool_name.to_string())
                    .or_insert(0);
                *entry += 1;
                *entry
            };
            self.no_progress.remove(&signature);

            // same_tool_failure counts DIFFERENT args on one tool; for failure-tolerant
            // tools a run of distinct red commands is diagnosis, not a loop.
            if self.config.hard_stop_enabled
                && !FAILURE_TOLERANT_TOOL_NAMES.contains(&tool_name)
                && same_count >= self.config.same_tool_failure_halt_after
            {
                return self.decide(
                    GuardrailAction::Halt,
                    "same_tool_failure_halt",
                    tool_name,
                    same_count,
                    signature,
                    None,
                    None,
                    None,
                );
            }
            if warnings && exact_count >= self.config.exact_failure_warn_after {
                return self.decide(
                    GuardrailAction::Warn,
                    "repeated_exact_failure_warning",
                    tool_name,
                    exact_count,
                    signature,
                    None,
                    None,
                    None,
                );
            }
            if warnings && same_count >= self.config.same_tool_failure_warn_after {
                let message = tool_failure_recovery_hint(tool_name, same_count);
                return self.decide(
                    GuardrailAction::Warn,
                    "same_tool_failure_warning",
                    tool_name,
                    same_count,
                    signature,
                    Some(message),
                    None,
                    None,
                );
            }
            return ToolGuardrailDecision {
                tool_name: tool_name.to_string(),
                count: exact_count,
                signature: Some(signature),
                ..Default::default()
            };
        }

        self.exact_failure_counts.remove(&signature);
        self.same_tool_failure_counts.remove(tool_name);
        // A successful mutation is progress for every failing signature still counted this turn.
        // Pure loops never mutate between attempts, so the replay detector keeps its teeth.
        if PROGRESS_RESET_TOOL_NAMES.contains(&tool_name)
            || file_mutation_result_landed(tool_name, result.unwrap_or(""))
        {
            let keys: Vec<ToolCallSignature> = self.exact_failure_counts.keys().copied().collect();
            for key in keys {
                self.progress_since_failure.insert(key, true);
            }
            self.same_tool_failure_counts.clear();
        }
        if !self.is_idempotent(tool_name) {
            self.no_progress.remove(&signature);
            return ToolGuardrailDecision {
                tool_name: tool_name.to_string(),
                signature: Some(signature),
                ..Default::default()
            };
        }

        let result_hash = hash_result(result);
        let repeat_count = match self.no_progress.get(&signature) {
            Some((previous_hash, previous_count)) if *previous_hash == result_hash => {
                *previous_count + 1
            }
            _ => 1,
        };
        self.no_progress
            .insert(signature, (result_hash, repeat_count));
        if warnings && repeat_count >= self.config.no_progress_warn_after {
            return self.decide(
                GuardrailAction::Warn,
                "idempotent_no_progress_warning",
                tool_name,
                repeat_count,
                signature,
                None,
                None,
                None,
            );
        }
        ToolGuardrailDecision {
            tool_name: tool_name.to_string(),
            count: repeat_count,
            signature: Some(signature),
            ..Default::default()
        }
    }

    fn is_idempotent(&self, tool_name: &str) -> bool {
        !MUTATING_TOOL_NAMES.contains(&tool_name) && IDEMPOTENT_TOOL_NAMES.contains(&tool_name)
    }

    pub fn observe_call(
        &mut self,
        tool_name: &str,
        args: &Value,
        result: Option<&str>,
        tool_call_id: &str,
        failed: bool,
    ) -> IdenticalCallObservation {
        let is_plain_str = result.is_some();
        let coerced = coerce_args(args);
        let signature = ToolCallSignature::from_call(tool_name, &coerced);
        let result_hash = if is_plain_str {
            hash_result(result)
        } else {
            Fingerprint::default()
        };

        if is_plain_str
            && self.identical_streak_sig.as_ref() == Some(&signature)
            && self.identical_streak_result_hash == result_hash
        {
            self.identical_streak_count += 1;
        } else {
            // New streak; non-string (multimodal) results never form one.
            self.identical_streak_sig = if is_plain_str { Some(signature) } else { None };
            self.identical_streak_result_hash = result_hash;
            self.identical_streak_count = if is_plain_str { 1 } else { 0 };
            self.identical_streak_first_call_id = tool_call_id.to_string();
        }
        let count = self.identical_streak_count;

        let mut notice = None;
        if !is_stall_guard_repeatable(tool_name) && count >= STALL_GUARD_IDENTICAL_CALL_THRESHOLD {
            notice = Some(identical_call_notice(count, tool_name));
            if self.config.hard_stop_enabled
                && count >= self.config.no_progress_block_after
                && self.halt_decision.is_none()
            {
                self.decide(
                    GuardrailAction::Halt,
                    "identical_call_streak_halt",
                    tool_name,
                    count,
                    signature,
                    None,
                    None,
                    None,
                );
            }
        }

        // Batch-cycle detection: a repeating multi-call cycle resets the consecutive
        // streak on every alternation, so check the call history for a period-p lap.
        if is_plain_str {
            self.call_history.push_back((
                signature,
                result_hash,
                is_stall_guard_repeatable(tool_name),
            ));
            while self.call_history.len() > STALL_GUARD_CYCLE_HISTORY {
                self.call_history.pop_front();
            }
        } else {
            self.call_history.clear();
        }

        if notice.is_none() && is_plain_str {
            if let Some((period, laps)) = self.detect_identical_cycle() {
                notice = Some(identical_cycle_notice(laps, period, tool_name));
                if self.config.hard_stop_enabled
                    && laps >= self.config.no_progress_block_after
                    && self.halt_decision.is_none()
                {
                    self.decide(
                        GuardrailAction::Halt,
                        "identical_cycle_halt",
                        tool_name,
                        laps,
                        signature,
                        None,
                        Some(period),
                        None,
                    );
                }
            }
        }

        let stub = if is_plain_str
            && count >= 2
            && !failed
            && result.map(|text| text.chars().count()).unwrap_or(0)
                >= IDENTICAL_RESULT_STUB_MIN_CHARS
        {
            Some(self.build_result_reference_stub(tool_name, &coerced))
        } else {
            None
        };

        IdenticalCallObservation { notice, stub }
    }

    fn detect_identical_cycle(&self) -> Option<(usize, usize)> {
        let history = &self.call_history;
        let len = history.len();
        for period in 2..=STALL_GUARD_MAX_CYCLE_PERIOD {
            if len < period * STALL_GUARD_IDENTICAL_CALL_THRESHOLD {
                continue;
            }
            let mut laps = 1usize;
            loop {
                let base = len as isize - (period * (laps + 1)) as isize;
                if base < 0 {
                    break;
                }
                let base = base as usize;
                let final_start = len - period;
                let lap_equal = (0..period).all(|i| {
                    let earlier = &history[base + i];
                    let latest = &history[final_start + i];
                    earlier.0 == latest.0 && earlier.1 == latest.1
                });
                if !lap_equal {
                    break;
                }
                laps += 1;
            }
            if laps >= STALL_GUARD_IDENTICAL_CALL_THRESHOLD {
                let final_start = len - period;
                let all_repeatable = (0..period).all(|i| history[final_start + i].2);
                if all_repeatable {
                    continue;
                }
                return Some((period, laps));
            }
        }
        None
    }

    /// Remember the spillover path a persisted result was saved to.
    pub fn record_persisted_result(&mut self, tool_call_id: &str, file_path: &str) {
        if !tool_call_id.is_empty() && !file_path.is_empty() {
            self.persisted_result_paths
                .insert(tool_call_id.to_string(), file_path.to_string());
        }
    }

    pub fn halt_decision(&self) -> Option<&ToolGuardrailDecision> {
        self.halt_decision.as_ref()
    }

    fn build_result_reference_stub(&self, tool_name: &str, args: &Value) -> String {
        let mut args_preview = canonical_tool_args(&coerce_args(args));
        if args_preview.chars().count() > RESULT_STUB_ARGS_PREVIEW_CHARS {
            args_preview = args_preview
                .chars()
                .take(RESULT_STUB_ARGS_PREVIEW_CHARS)
                .collect::<String>()
                + "\u{2026}";
        }
        let first_id = self.identical_streak_first_call_id.as_str();
        let reference = if first_id.is_empty() {
            String::new()
        } else {
            format!(" (tool_call_id {first_id})")
        };
        let mut stub = format!(
            "[hermes note: this result is byte-identical to the {tool_name} result earlier this turn{reference}. Refer to that result; it has not changed. Args: {args_preview}]"
        );
        if !first_id.is_empty() {
            if let Some(spill_path) = self.persisted_result_paths.get(first_id) {
                stub.push_str(&format!(
                    "\n[The referenced result was persisted to: {spill_path} \u{2014} page through it with read_file if you need the full content.]"
                ));
            }
        }
        stub
    }

    fn check_loop_cap(
        &mut self,
        tool_name: &str,
        args: &Value,
        signature: ToolCallSignature,
    ) -> Option<ToolGuardrailDecision> {
        let (is_web_search, code) = match tool_name {
            "web_search" => (true, "loop_web_search_cap"),
            "delegate_task" => (false, "loop_subagent_cap"),
            _ => return None,
        };
        let (cap, count) = if is_web_search {
            (
                self.config.loop_caps.max_web_searches,
                self.turn_web_search_count,
            )
        } else {
            (
                self.config.loop_caps.max_subagents,
                self.turn_subagent_count,
            )
        };
        let increment = if is_web_search {
            1
        } else if cap > 0 {
            subagent_spawn_count(args)
        } else {
            0
        };
        if increment > 0 && cap > 0 && count >= cap {
            return Some(self.decide(
                GuardrailAction::Block,
                code,
                tool_name,
                count as usize,
                signature,
                None,
                None,
                Some(cap),
            ));
        }
        if is_web_search {
            self.turn_web_search_count = count + increment;
        } else {
            self.turn_subagent_count = count + increment;
        }
        None
    }
}

/// Fallback classifier used only when callers do not pass failed.
pub fn classify_tool_failure(tool_name: &str, result: Option<&str>) -> (bool, String) {
    let Some(result) = result else {
        return (false, String::new());
    };
    if file_mutation_result_landed(tool_name, result) {
        return (false, String::new());
    }
    // A harness REFUSAL of a redundant call carries "error" for the model's benefit but
    // nothing failed; counting it would feed the streak that fires the next refusal.
    if is_guardrail_refusal(result) {
        return (false, String::new());
    }

    if tool_name == "bash" {
        let data = safe_json_loads(result);
        if let Some(Value::Object(map)) = data.as_ref() {
            if let Some(exit_code) = map.get("exit_code") {
                if !exit_code.is_null() && !is_json_zero(exit_code) {
                    return (true, format!(" [exit {}]", py_str(exit_code)));
                }
            }
        }
        return (false, String::new());
    }

    if tool_name == "memory" {
        let data = safe_json_loads(result);
        if let Some(Value::Object(map)) = data.as_ref() {
            if map.get("success") == Some(&Value::Bool(false)) {
                if let Some(Value::String(error)) = map.get("error") {
                    if error.contains("exceed the limit") {
                        return (true, " [full]".to_string());
                    }
                }
            }
        }
    }

    let lower: String = result.chars().take(500).collect::<String>().to_lowercase();
    if lower.contains("\"error\"") || lower.contains("\"failed\"") || result.starts_with("Error") {
        (true, " [error]".to_string())
    } else {
        (false, String::new())
    }
}

/// Build a synthetic role=tool content string for a blocked tool call.
pub fn toolguard_synthetic_result(decision: &ToolGuardrailDecision) -> String {
    json!({ "error": decision.message, "guardrail": decision.to_metadata() }).to_string()
}

/// Append runtime guidance to the current tool result content.
pub fn append_toolguard_guidance(result: &mut String, decision: &ToolGuardrailDecision) {
    if !matches!(
        decision.action,
        GuardrailAction::Warn | GuardrailAction::Halt
    ) || decision.message.is_empty()
    {
        return;
    }
    let label = if decision.action == GuardrailAction::Halt {
        "Tool loop hard stop"
    } else {
        "Tool loop warning"
    };
    result.push_str(&format!(
        "\n\n[{label}: {}; count={}; {}]",
        decision.code, decision.count, decision.message
    ));
}

/// Guardrail verdict text injected into the conversation, keyed by decision code.
const DECISION_MESSAGES: &[(&str, &str)] = &[
    (
        "repeated_exact_failure_block",
        "Blocked {tool_name}: the same tool call failed {count} times with identical arguments. Stop retrying it unchanged; change strategy or explain the blocker.",
    ),
    (
        "idempotent_no_progress_block",
        "Blocked {tool_name}: this read-only call returned the same result {count} times. Stop repeating it unchanged; use the result already provided or try a different query.",
    ),
    (
        "same_tool_failure_halt",
        "Stopped {tool_name}: it failed {count} times this turn. Stop retrying the same failing tool path and choose a different approach.",
    ),
    (
        "repeated_exact_failure_warning",
        "{tool_name} has failed {count} times with identical arguments. This looks like a loop; inspect the error and change strategy instead of retrying it unchanged.",
    ),
    (
        "idempotent_no_progress_warning",
        "{tool_name} returned the same result {count} times. Use the result already provided or change the query instead of repeating it unchanged.",
    ),
    (
        "identical_call_streak_halt",
        "Stopped {tool_name}: the same call with identical arguments returned the same result {count} times in a row. Stop repeating it unchanged; use the result already provided or change strategy.",
    ),
    (
        "identical_cycle_halt",
        "Stopped {tool_name}: the same repeating cycle of tool calls (period {period}) with identical arguments and identical results has run {count} times. Repeating the batch unchanged is not progress; use the results already provided or change strategy.",
    ),
    (
        "loop_web_search_cap",
        "Blocked web_search: this turn has already made {cap} web searches, the per-turn limit. This looks like a runaway search loop. Work with the results you already have and give the user your answer.",
    ),
    (
        "loop_subagent_cap",
        "Blocked delegate_task: this turn has already spawned {count} subagents (limit {cap}). This looks like a runaway delegation loop. Finish the work with the results you have and answer the user.",
    ),
];

fn decision_message(
    code: &str,
    tool_name: &str,
    count: usize,
    period: Option<usize>,
    cap: Option<u32>,
) -> String {
    let template = DECISION_MESSAGES
        .iter()
        .find(|(candidate, _)| *candidate == code)
        .map(|(_, message)| *message)
        .unwrap_or("");
    let mut message = template
        .replace("{tool_name}", tool_name)
        .replace("{count}", &count.to_string());
    if let Some(period) = period {
        message = message.replace("{period}", &period.to_string());
    }
    if let Some(cap) = cap {
        message = message.replace("{cap}", &cap.to_string());
    }
    message
}

fn identical_call_notice(ordinal: usize, tool_name: &str) -> String {
    format!(
        "[hermes note: this is the {} consecutive identical call to {tool_name} with identical arguments returning the same result. Do not repeat it \u{2014} change arguments, use a different tool, or proceed with what you have.]",
        ordinal_str(ordinal)
    )
}

fn identical_cycle_notice(count: usize, period: usize, tool_name: &str) -> String {
    format!(
        "[hermes note: the last {count} rounds repeated the same cycle of {period} tool calls (ending with {tool_name}) with identical arguments and identical results. Do not repeat the batch \u{2014} change arguments, use a different tool, or proceed with what you have.]"
    )
}

fn ordinal_str(count: usize) -> String {
    let suffix = if (11..=13).contains(&(count % 100)) {
        "th"
    } else {
        match count % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{count}{suffix}")
}

fn tool_failure_recovery_hint(tool_name: &str, count: usize) -> String {
    let common = format!(
        "{tool_name} has failed {count} times this turn. This looks like a loop: stop repeating it and read the latest error first. "
    );
    if tool_name == "bash" {
        return common + "For bash failures, run a small diagnostic such as `pwd && ls -la` in the same tool, then try an absolute path, a simpler command, a different working directory, or a different tool such as read_file/write_file/patch.";
    }
    // Never tell the model to keep calling tools: when no tool can do the job (a guessed
    // name, a missing workspace), the right move is to stop and tell the user.
    common + "If a different argument or tool can clearly make progress, try it once. Otherwise stop calling tools and tell the user what is blocking you."
}

/// Return sorted compact JSON for parsed tool arguments.
pub fn canonical_tool_args(args: &Value) -> String {
    serde_json::to_string(args).unwrap_or_else(|_| "null".to_string())
}

fn coerce_args(args: &Value) -> Cow<'_, Value> {
    if args.is_object() {
        Cow::Borrowed(args)
    } else {
        Cow::Owned(Value::Object(Map::new()))
    }
}

fn hash_result(result: Option<&str>) -> Fingerprint {
    match result {
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Null) | Err(_) => sha256(text),
            Ok(value) => sha256(&canonical_tool_args(&value)),
        },
        None => sha256(""),
    }
}

fn sha256(value: &str) -> Fingerprint {
    Sha256::digest(value.as_bytes()).into()
}

fn safe_json_loads(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text).ok()
}

fn is_guardrail_refusal(result: &str) -> bool {
    match serde_json::from_str::<Value>(result.trim()) {
        Ok(Value::Object(map)) => map.get("guardrail_refusal") == Some(&Value::Bool(true)),
        _ => false,
    }
}

fn file_mutation_result_landed(tool_name: &str, result: &str) -> bool {
    if tool_name != "write_file" && tool_name != "patch" {
        return false;
    }
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(result.trim()) else {
        return false;
    };
    if map.get("error").map(is_truthy).unwrap_or(false) {
        return false;
    }
    match tool_name {
        "write_file" => map.contains_key("bytes_written"),
        "patch" => map.get("success") == Some(&Value::Bool(true)),
        _ => false,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn is_json_zero(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::Bool(flag) => !*flag,
        _ => false,
    }
}

fn py_str(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(text) => text.into(),
        other => other.to_string().into(),
    }
}

fn subagent_spawn_count(args: &Value) -> u32 {
    let action = match args.get("action") {
        Some(Value::String(text)) => text.trim().to_lowercase(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string().trim().to_lowercase(),
    };
    if matches!(action.as_str(), "list" | "steer" | "stop") {
        return 0;
    }
    match args.get("tasks") {
        Some(Value::Array(tasks)) if !tasks.is_empty() => tasks.len() as u32,
        _ => 1,
    }
}

fn as_bool(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => true,
            "0" | "false" | "no" | "off" | "disabled" => false,
            _ => default,
        },
        _ => default,
    }
}

fn parse_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(flag) => Some(if *flag { 1 } else { 0 }),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Some(integer)
            } else {
                let float = number.as_f64()?;
                if float.is_finite() {
                    Some(float.trunc() as i64)
                } else {
                    None
                }
            }
        }
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// junk/None/below-minimum fall back to default (caps use minimum 0 so 0 = disabled).
fn int_at_least(value: Option<&Value>, default: i64, minimum: i64) -> i64 {
    let Some(value) = value else {
        return default;
    };
    match parse_int(value) {
        Some(parsed) if parsed >= minimum => parsed,
        _ => default,
    }
}
