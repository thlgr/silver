//! Several keys for one provider. Each stream call tries the preferred healthy key; a synchronous
//! rate-limit, billing or auth failure cools it down (60s doubling to 4h) and moves on. With every
//! key cooling the preferred one is tried anyway, so a recovered key is found.

use crate::fallback::{cooldown_for, is_auth_error, DEFAULT_BASE_COOLDOWN, DEFAULT_MAX_COOLDOWN};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{Model, ModelRequest, ModelStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How the pool picks the next key; fill_first is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PoolStrategy {
    /// Prefer the first healthy key in configuration order.
    #[default]
    FillFirst,
    /// Advance through the keys one per selection, wrapping at the end.
    RoundRobin,
    /// Prefer the healthy key that has served the fewest requests.
    LeastUsed,
}

impl PoolStrategy {
    /// Parse a strategy name case-insensitively; anything unknown is fill_first, so a typo cannot
    /// disable failover.
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "round_robin" => Self::RoundRobin,
            "least_used" => Self::LeastUsed,
            _ => Self::FillFirst,
        }
    }

    /// Stable strategy name, as written in configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FillFirst => "fill_first",
            Self::RoundRobin => "round_robin",
            Self::LeastUsed => "least_used",
        }
    }
}

/// In-memory state for one credential key.
#[derive(Clone, Debug)]
struct KeyState {
    /// Consecutive exhaustion failures, used to escalate the cooldown.
    failures: u32,
    /// Wall-clock instant at which the key becomes eligible again.
    cooldown_until: Option<Instant>,
    /// Requests this key has been selected for; the least_used baseline.
    request_count: u64,
}

impl KeyState {
    fn new() -> Self {
        Self {
            failures: 0,
            cooldown_until: None,
            request_count: 0,
        }
    }

    fn is_cooling(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| now < until)
    }
}

/// Mutable pool state guarded by one mutex.
struct PoolState {
    /// Index of the active key (the last one selected); reported by CredentialPoolModel::name.
    selected: usize,
    /// Round-robin cursor: the next key to prefer.
    cursor: usize,
    /// Per-key state, indexed like the model vector.
    keys: Vec<KeyState>,
}

/// Same-provider keys with strategy-based selection and rotation, one transport per key.
pub struct CredentialPoolModel {
    strategy: PoolStrategy,
    /// One transport per key, in configuration order (index 0 is the first credential).
    models: Vec<Arc<dyn Model>>,
    /// Selection cursor, request counts and per-key cooldowns.
    state: Mutex<PoolState>,
    base_cooldown: Duration,
    max_cooldown: Duration,
}

impl CredentialPoolModel {
    /// One model per key, in `config.credential_pool()` order, with the default 60s to 4h backoff.
    /// Panics when models is empty.
    pub fn new(strategy: PoolStrategy, models: Vec<Arc<dyn Model>>) -> Self {
        assert!(
            !models.is_empty(),
            "credential pool must contain at least one key"
        );
        let keys = models.iter().map(|_| KeyState::new()).collect();
        Self {
            strategy,
            models,
            state: Mutex::new(PoolState {
                selected: 0,
                cursor: 0,
                keys,
            }),
            base_cooldown: DEFAULT_BASE_COOLDOWN,
            max_cooldown: DEFAULT_MAX_COOLDOWN,
        }
    }

    /// Override the backoff bounds.
    ///
    /// Exposed so tests (and future policy wiring) can exercise expiry without waiting minutes.
    pub fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.base_cooldown = base;
        self.max_cooldown = max.max(base);
        self
    }

    /// The configured selection strategy.
    pub fn strategy(&self) -> PoolStrategy {
        self.strategy
    }

    /// The key the strategy prefers among the healthy ones, or its natural first key when all are
    /// cooling, so recovery can still be probed.
    fn preferred_index(&self, state: &PoolState, healthy: &[bool]) -> usize {
        let len = self.models.len();
        match self.strategy {
            PoolStrategy::FillFirst => (0..len).find(|&index| healthy[index]).unwrap_or(0),
            PoolStrategy::RoundRobin => (0..len)
                .map(|offset| (state.cursor + offset) % len)
                .find(|&index| healthy[index])
                .unwrap_or(state.cursor % len),
            PoolStrategy::LeastUsed => (0..len)
                .filter(|&index| healthy[index])
                .min_by_key(|&index| state.keys[index].request_count)
                .unwrap_or(0),
        }
    }

    /// Keys to try for one call: the preferred healthy key, then the other healthy ones; the
    /// preferred key alone when all are cooling. Request counts advance in mark_attempted.
    fn attempt_order(&self) -> Vec<usize> {
        let mut state = self.state.lock().expect("credential pool mutex poisoned");
        let now = Instant::now();
        let len = self.models.len();
        let healthy: Vec<bool> = state.keys.iter().map(|key| !key.is_cooling(now)).collect();
        let preferred = self.preferred_index(&state, &healthy);
        let mut order: Vec<usize> = (0..len)
            .map(|offset| (preferred + offset) % len)
            .filter(|&index| healthy[index])
            .collect();
        if order.is_empty() {
            order.push(preferred);
        }
        state.selected = order[0];
        order
    }

    /// Record that index is about to serve a request: bump its counter, advance the round-robin
    /// cursor and mark it active (mirrors the request_count bump and priority rotation in
    /// _select_unlocked).
    fn mark_attempted(&self, index: usize) {
        let mut state = self.state.lock().expect("credential pool mutex poisoned");
        let len = self.models.len();
        state.selected = index;
        state.keys[index].request_count = state.keys[index].request_count.saturating_add(1);
        if self.strategy == PoolStrategy::RoundRobin {
            state.cursor = (index + 1) % len;
        }
    }

    /// Arm (or escalate) the cooldown for a key after an exhaustion failure.
    fn arm_cooldown(&self, index: usize) {
        let backoff = {
            let mut state = self.state.lock().expect("credential pool mutex poisoned");
            let key = &mut state.keys[index];
            let backoff = cooldown_for(self.base_cooldown, self.max_cooldown, key.failures);
            key.failures = key.failures.saturating_add(1);
            key.cooldown_until = Some(Instant::now() + backoff);
            backoff
        };
        tracing::warn!(
            key = index,
            cooldown_ms = backoff.as_millis() as u64,
            "credential key failed; cooling down pool key"
        );
    }

    /// Clear a key's failure state after it succeeds, so its next failure starts the ladder over.
    fn record_success(&self, index: usize) {
        let mut state = self.state.lock().expect("credential pool mutex poisoned");
        state.selected = index;
        state.keys[index].failures = 0;
        state.keys[index].cooldown_until = None;
    }
}

/// Whether another key could clear the error: rate limits, billing walls and auth failures. Any
/// other failure would hit every key alike.
fn is_exhaustion_error(error: &CoreError) -> bool {
    is_rate_limit_error(error) || error.is_billing_error() || is_auth_error(error)
}

/// Whether the error reports that the provider throttled the request.
fn is_rate_limit_error(error: &CoreError) -> bool {
    match error {
        CoreError::ProviderRateLimited(_) => true,
        CoreError::ProviderTransient { rate_limited, .. } => *rate_limited,
        // A proxy may re-wrap a 429 as a plain provider-unavailable body.
        CoreError::ProviderUnavailable(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("http 429")
                || lower.contains("rate limit")
                || lower.contains("too many requests")
        }
        _ => false,
    }
}

#[async_trait::async_trait]
impl Model for CredentialPoolModel {
    fn name(&self) -> &str {
        let index = {
            let state = self.state.lock().expect("credential pool mutex poisoned");
            state.selected
        };
        match self.models.get(index) {
            Some(model) => model.name(),
            None => self.models.first().map(|model| model.name()).unwrap_or(""),
        }
    }

    /// Every key in a pool reaches the same endpoint, so the first transport answers.
    fn is_local(&self) -> bool {
        self.models.first().is_some_and(|model| model.is_local())
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let order = self.attempt_order();
        let mut last_error: Option<CoreError> = None;
        for index in order {
            self.mark_attempted(index);
            match self.models[index]
                .stream(
                    ModelRequest::clone(&request),
                    CancellationToken::clone(&cancel),
                )
                .await
            {
                Ok(stream) => {
                    self.record_success(index);
                    return Ok(stream);
                }
                Err(error) => {
                    if !is_exhaustion_error(&error) {
                        return Err(error);
                    }
                    self.arm_cooldown(index);
                    last_error = Some(error);
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| CoreError::Internal("credential pool has no keys".to_string())))
    }
}
