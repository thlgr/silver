//! Provider fallback chain: each stream call tries the first route not cooling down; a synchronous
//! recoverable error cools it (60s doubling to 4h) and moves on. With every route cooling the
//! primary is tried anyway. Mid-stream errors are not retried: a replay would duplicate output.

use silver_core::error::{CoreError, CoreResult, RetryClass};
use silver_core::model::{Model, ModelRequest, ModelStream};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Default first cooldown: one minute.
pub(crate) const DEFAULT_BASE_COOLDOWN: Duration = Duration::from_secs(60);

/// Default cooldown ceiling: four hours.
pub(crate) const DEFAULT_MAX_COOLDOWN: Duration = Duration::from_secs(4 * 60 * 60);

/// In-memory cooldown for one route.
#[derive(Clone, Copy, Debug)]
struct Cooldown {
    /// Wall-clock instant at which the route becomes eligible again.
    until: Instant,
    /// Consecutive recoverable failures, used to escalate the backoff.
    failures: u32,
}

impl Cooldown {
    fn is_cooling(&self, now: Instant) -> bool {
        now < self.until
    }
}

/// An ordered model chain (primary first) with per-route exponential cooldowns.
pub struct FallbackModel {
    /// Route 0 is the primary; the rest are fallbacks in configuration order.
    models: Vec<Arc<dyn Model>>,
    /// Cooldown state keyed by route index.
    cooldowns: Mutex<HashMap<usize, Cooldown>>,
    base_cooldown: Duration,
    max_cooldown: Duration,
}

impl FallbackModel {
    /// Wrap models (primary first) with the default 60s to 4h backoff. Panics when models is empty.
    pub fn new(models: Vec<Arc<dyn Model>>) -> Self {
        assert!(
            !models.is_empty(),
            "fallback chain must contain the primary model"
        );
        Self {
            models,
            cooldowns: Mutex::new(HashMap::new()),
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

    /// Index of the route stream tries first: the lowest-numbered route not in cooldown, or
    /// the primary when every route is cooling down.
    fn preferred_index(&self) -> usize {
        let now = Instant::now();
        let cooldowns = self
            .cooldowns
            .lock()
            .expect("fallback cooldown mutex poisoned");
        (0..self.models.len())
            .find(|&index| !cooldowns.get(&index).is_some_and(|c| c.is_cooling(now)))
            .unwrap_or(0)
    }

    /// Arm (or escalate) the cooldown for index after a recoverable failure.
    fn arm_cooldown(&self, index: usize) {
        let backoff = {
            let mut cooldowns = self
                .cooldowns
                .lock()
                .expect("fallback cooldown mutex poisoned");
            let entry = cooldowns.entry(index).or_insert(Cooldown {
                until: Instant::now(),
                failures: 0,
            });
            let backoff = cooldown_for(self.base_cooldown, self.max_cooldown, entry.failures);
            entry.failures = entry.failures.saturating_add(1);
            entry.until = Instant::now() + backoff;
            backoff
        };
        tracing::warn!(
            route = index,
            cooldown_ms = backoff.as_millis() as u64,
            "model route failed; cooling down fallback route"
        );
    }

    /// Clear a route cooldown after it succeeds, so its next failure starts the ladder over.
    fn record_success(&self, index: usize) {
        let mut cooldowns = self
            .cooldowns
            .lock()
            .expect("fallback cooldown mutex poisoned");
        cooldowns.remove(&index);
    }
}

/// Cooldown for the next consecutive failure: base * 2^failures, capped at max.
pub(crate) fn cooldown_for(base: Duration, max: Duration, failures: u32) -> Duration {
    let factor = 1u128 << failures.min(63);
    let millis = base.as_millis().saturating_mul(factor).min(max.as_millis());
    Duration::from_millis(millis.min(u64::MAX as u128) as u64)
}

/// Whether another route could help: billing walls, missing models, auth failures, rate limits and
/// transient failures. Bad requests, context overflow and internal errors would fail anywhere.
fn is_failover_error(error: &CoreError) -> bool {
    error.is_billing_error()
        || error.is_model_not_found()
        || matches!(error.retry_class(), RetryClass::Transient { .. })
        || is_auth_error(error)
}

/// Whether the error is an auth failure, recognised by the message shapes transports emit for
/// HTTP 401/403 and common provider bodies.
pub(crate) fn is_auth_error(error: &CoreError) -> bool {
    let message = match error {
        CoreError::ProviderUnavailable(message) => message,
        CoreError::ProviderTransient { message, .. } => message,
        _ => return false,
    };
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication failed")
        || lower.contains("authentication_error")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("invalid x-api-key")
        || lower.contains("http 401")
        || lower.contains("http 403")
}

#[async_trait::async_trait]
impl Model for FallbackModel {
    fn name(&self) -> &str {
        if self.models.is_empty() {
            return "";
        }
        self.models[self.preferred_index()].name()
    }

    fn is_local(&self) -> bool {
        !self.models.is_empty() && self.models[self.preferred_index()].is_local()
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let preferred = self.preferred_index();
        let candidates = {
            let now = Instant::now();
            let cooldowns = self
                .cooldowns
                .lock()
                .expect("fallback cooldown mutex poisoned");
            let mut candidates: Vec<usize> = (preferred..self.models.len())
                .filter(|&index| !cooldowns.get(&index).is_some_and(|c| c.is_cooling(now)))
                .collect();
            if candidates.is_empty() {
                // Every route is cooling down. Attempt the primary anyway so a recovered
                // provider is picked up instead of failing without a request.
                candidates.push(preferred);
            }
            candidates
        };

        let mut last_error: Option<CoreError> = None;
        for index in candidates {
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
                    if !is_failover_error(&error) {
                        return Err(error);
                    }
                    self.arm_cooldown(index);
                    last_error = Some(error);
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| CoreError::Internal("fallback chain has no routes".to_string())))
    }
}
