//! Empty-completion retry decisions; both guards fail open to plain retries. Two identical empty
//! attempts in a row skip the rest of the retries for the fallbacks; an empty attempt costing the
//! threshold or more cuts the streak's retries from three to one.

/// Retry budget for a normal empty streak.
pub const DEFAULT_EMPTY_RETRY_BUDGET: u32 = 3;
/// Retry budget once the cost-aware reduction applies.
pub const REDUCED_EMPTY_RETRY_BUDGET: u32 = 1;
/// Default per-attempt cost threshold, in USD.
pub const DEFAULT_COST_THRESHOLD_USD: f64 = 0.25;
/// Whether the guard is enabled when configuration does not override it.
pub const DEFAULT_GUARD_ENABLED: bool = true;

/// One observed empty completion within the current streak.
#[derive(Clone, Debug, PartialEq)]
pub struct EmptyAttempt {
    /// Model that produced the attempt.
    pub model: String,
    /// Provider that produced the attempt.
    pub provider: String,
    /// Provider finish reason for the attempt.
    pub finish_reason: String,
    /// Whether a usage object was present and usable.
    pub usage_present: bool,
    /// Whether usage proved zero generated output and reasoning tokens.
    pub zero_output: bool,
    /// Whether assembled content or reasoning was observed.
    pub observed_generation: bool,
    /// Best-effort estimated input cost in USD, or None when unknown.
    pub estimated_cost_usd: Option<f64>,
}

impl EmptyAttempt {
    /// Model, provider and finish reason that identify an attempt.
    pub fn signature(&self) -> (&str, &str, &str) {
        (&self.model, &self.provider, &self.finish_reason)
    }
}

/// What the empty-response loop should do after recording an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyDecision {
    /// Keep retrying within the current streak's budget.
    Retry,
    /// Skip the remaining retries and activate the fallback chain.
    SkipToFallback,
}

/// One empty streak; reset it wherever the loop resets its counters (turn start, tool success,
/// compaction, fallback).
#[derive(Debug)]
pub struct EmptyResponseGuard {
    enabled: bool,
    cost_threshold_usd: f64,
    attempts: Vec<EmptyAttempt>,
}

impl EmptyResponseGuard {
    /// Build a guard. Non-finite or non-positive thresholds fall back to the default.
    pub fn new(enabled: bool, cost_threshold_usd: f64) -> Self {
        Self {
            enabled,
            cost_threshold_usd: normalize_threshold(cost_threshold_usd),
            attempts: Vec::new(),
        }
    }

    /// Configured enabled flag; a disabled guard only applies the fixed budget.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Retries for this streak: 3, or 1 after an empty attempt whose input cost reached the
    /// threshold.
    pub fn retry_budget(&self) -> u32 {
        if !self.enabled {
            return DEFAULT_EMPTY_RETRY_BUDGET;
        }
        match self.attempts.last().and_then(|a| a.estimated_cost_usd) {
            Some(cost) if cost.is_finite() && cost >= self.cost_threshold_usd => {
                REDUCED_EMPTY_RETRY_BUDGET
            }
            _ => DEFAULT_EMPTY_RETRY_BUDGET,
        }
    }

    /// Record an empty attempt: SkipToFallback once the streak is deterministic or out of retries.
    pub fn record(&mut self, attempt: EmptyAttempt) -> EmptyDecision {
        self.attempts.push(attempt);
        if self.deterministic_empty() {
            return EmptyDecision::SkipToFallback;
        }
        if self.attempts.len() <= self.retry_budget() as usize {
            EmptyDecision::Retry
        } else {
            EmptyDecision::SkipToFallback
        }
    }

    /// Clear streak state (called on turn start, tool success, compaction, fallback activation).
    pub fn reset(&mut self) {
        self.attempts.clear();
    }

    /// Number of empty attempts in the current streak.
    pub fn attempts(&self) -> usize {
        self.attempts.len()
    }

    /// True when the current streak looks deterministic (two or more consecutive
    /// attempts with an identical signature and unanimous empty evidence).
    fn deterministic_empty(&self) -> bool {
        if !self.enabled || self.attempts.len() < 2 {
            return false;
        }
        let first = &self.attempts[0];
        let same_signature = self
            .attempts
            .iter()
            .all(|a| a.signature() == first.signature());
        let usage_proves_empty = self
            .attempts
            .iter()
            .all(|a| a.usage_present && a.zero_output);
        let response_proves_empty = self
            .attempts
            .iter()
            .all(|a| !a.usage_present && !a.observed_generation);
        same_signature && (usage_proves_empty || response_proves_empty)
    }
}

/// Reject NaN, infinities and non-positive thresholds; malformed values use the default.
fn normalize_threshold(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        DEFAULT_COST_THRESHOLD_USD
    }
}
