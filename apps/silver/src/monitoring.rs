//! Opt-in monitoring: with `[monitoring] enabled` and an endpoint, JSON events are POSTed through a
//! bounded queue. emit never blocks or fails; a full queue drops and counts the newest event; a
//! failed POST is logged and dropped; disabled, every entry point is a no-op.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use silver_protocol::{EventPayload, RunId, SessionId};
use tokio::sync::mpsc;

use crate::config::MonitoringConfig;
use silver_core::redact::redact;

/// Bounded queue depth. Beyond this the newest event is dropped (and counted).
const QUEUE_CAPACITY: usize = 1_024;

/// One monitoring POST gets this long before it is abandoned.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the rate-limit watcher samples the process-wide provider state.
const RATE_LIMIT_POLL: Duration = Duration::from_secs(5);

/// A JSON health/metrics event. Serialized with an event tag and an RFC 3339 ts field.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum MonitorEvent {
    /// A run was admitted and its model request began.
    RunStarted {
        run_id: String,
        session_id: String,
        model: String,
    },
    /// A run reached a terminal non-failure state.
    RunCompleted {
        run_id: String,
        session_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_tokens: Option<u64>,
    },
    /// A run failed.
    RunFailed {
        run_id: String,
        session_id: String,
        code: String,
        message: String,
    },
    /// A provider response carried rate-limit headers.
    ProviderRateLimited {
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        requests_remaining: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_remaining: Option<u64>,
    },
    /// A generic health/metrics point.
    Health {
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl MonitorEvent {
    /// A run-started event.
    pub fn run_started(run_id: RunId, session_id: SessionId, model: impl Into<String>) -> Self {
        Self::RunStarted {
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            model: model.into(),
        }
    }

    /// A terminal (completed or cancelled) run event.
    pub fn run_completed(
        run_id: RunId,
        session_id: SessionId,
        status: impl Into<String>,
        duration_ms: Option<u64>,
        total_tokens: Option<u64>,
    ) -> Self {
        Self::RunCompleted {
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            status: status.into(),
            duration_ms,
            total_tokens,
        }
    }

    /// A failed-run event.
    pub fn run_failed(
        run_id: RunId,
        session_id: SessionId,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::RunFailed {
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            code: code.into(),
            message: message.into(),
        }
    }

    /// A provider rate-limit observation.
    pub fn provider_rate_limited(
        provider: impl Into<String>,
        requests_remaining: Option<u64>,
        tokens_remaining: Option<u64>,
    ) -> Self {
        Self::ProviderRateLimited {
            provider: provider.into(),
            requests_remaining,
            tokens_remaining,
        }
    }

    /// A generic health point.
    pub fn health(status: impl Into<String>, detail: Option<String>) -> Self {
        Self::Health {
            status: status.into(),
            detail,
        }
    }

    /// The stable event name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::RunCompleted { .. } => "run_completed",
            Self::RunFailed { .. } => "run_failed",
            Self::ProviderRateLimited { .. } => "provider_rate_limited",
            Self::Health { .. } => "health",
        }
    }
}

/// The configured emitter. Cloneable via Arc; a disabled instance is a no-op.
pub struct Monitor {
    endpoint: Option<String>,
    redact: bool,
    tx: Option<mpsc::Sender<Value>>,
    dropped: Arc<AtomicU64>,
    enabled: bool,
}

impl Monitor {
    /// Build a monitor from config. Active only with both enabled = true and an endpoint.
    pub fn from_config(config: &MonitoringConfig) -> Self {
        let endpoint = config
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| config.enabled && !value.is_empty());
        let Some(endpoint) = endpoint else {
            return Self::disabled();
        };
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        let (tx, mut rx) = mpsc::channel::<Value>(QUEUE_CAPACITY);
        let endpoint_for_task = endpoint.to_string();
        let redact_events = config.redact;
        // Monitoring must not panic a caller that happens to run outside a runtime.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    while let Some(payload) = rx.recv().await {
                        let body = if redact_events {
                            redact_json(payload)
                        } else {
                            payload
                        };
                        if let Err(err) = post_event(&client, &endpoint_for_task, &body).await {
                            tracing::warn!(error = %err, "monitoring event delivery failed");
                        }
                    }
                });
            }
            Err(_) => {
                tracing::warn!("monitoring enabled but no async runtime is active");
                return Self::disabled();
            }
        }
        Self {
            endpoint: Some(endpoint.to_string()),
            redact: config.redact,
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
            enabled: true,
        }
    }

    /// A monitor that drops every event.
    pub fn disabled() -> Self {
        Self {
            endpoint: None,
            redact: true,
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
            enabled: false,
        }
    }

    /// True when events are actually dispatched.
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// The configured endpoint, when active.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Whether payloads are scrubbed before dispatch.
    pub fn redacts(&self) -> bool {
        self.redact
    }

    /// Enqueue one event. Never blocks, never panics.
    pub fn emit(&self, event: &MonitorEvent) {
        if !self.enabled {
            return;
        }
        let Some(tx) = &self.tx else {
            return;
        };
        let payload = match event_payload(event) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!(error = %err, "monitoring event could not be serialized");
                return;
            }
        };
        if tx.try_send(payload).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("monitoring queue full; dropping event");
        }
    }

    /// Events dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Serialize an event and stamp it with the wall-clock time.
fn event_payload(event: &MonitorEvent) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(event)?;
    if let Value::Object(map) = &mut value {
        map.insert("ts".to_string(), Value::String(Utc::now().to_rfc3339()));
    }
    Ok(value)
}

/// Recursively scrub secret-shaped strings in a payload; non-strings are untouched.
fn redact_json(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_json).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_json(value)))
                .collect(),
        ),
        other => other,
    }
}

/// POST one event body. Any failure is the caller's to log; it never propagates outward.
async fn post_event(
    client: &reqwest::Client,
    endpoint: &str,
    body: &Value,
) -> Result<(), reqwest::Error> {
    client
        .post(endpoint)
        .json(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Translate the run lifecycle events a monitor cares about into (event, terminal) pairs.
/// Every other payload is ignored.
fn lifecycle_event(
    payload: &EventPayload,
    run_id: RunId,
    session_id: SessionId,
) -> Option<(MonitorEvent, bool)> {
    match payload {
        EventPayload::RunStarted { model, .. } => {
            Some((MonitorEvent::run_started(run_id, session_id, model), false))
        }
        EventPayload::RunCompleted {
            usage, duration_ms, ..
        } => Some((
            MonitorEvent::run_completed(
                run_id,
                session_id,
                "completed",
                Some(*duration_ms),
                usage.as_ref().map(|usage| usage.total_tokens),
            ),
            true,
        )),
        EventPayload::RunFailed { code, message } => Some((
            MonitorEvent::run_failed(run_id, session_id, code.as_str(), message),
            true,
        )),
        EventPayload::RunCancelled { .. } => Some((
            MonitorEvent::run_completed(run_id, session_id, "cancelled", None, None),
            true,
        )),
        _ => None,
    }
}

/// Emit started/completed/failed for one run. The subscription replays persisted events, so a run
/// that finished before the watcher started is still reported.
pub async fn watch_run(
    monitor: Arc<Monitor>,
    runs: Arc<crate::run_manager::RunManager>,
    run_id: RunId,
    session_id: SessionId,
) {
    let mut subscription = match runs.subscribe(run_id, None).await {
        Ok(subscription) => subscription,
        Err(err) => {
            tracing::debug!(error = %err, "monitoring could not subscribe to run events");
            return;
        }
    };
    while let Some(event) = subscription.next().await {
        if let Some((monitor_event, terminal)) = lifecycle_event(&event.payload, run_id, session_id)
        {
            monitor.emit(&monitor_event);
            if terminal {
                return;
            }
        }
    }
}

/// Whether two captures report the same limits. The capture time is deliberately left out: it
/// changes on every response and would defeat dedup.
fn same_limits(a: &crate::provider::RateLimitState, b: &crate::provider::RateLimitState) -> bool {
    a.provider == b.provider
        && a.requests.remaining == b.requests.remaining
        && a.tokens.remaining == b.tokens.remaining
        && a.requests.reset == b.requests.reset
        && a.tokens.reset == b.tokens.reset
}

/// Poll the provider's rate-limit slot, which has no callback, and emit an event per change. A
/// no-op without an installed monitor.
pub fn spawn_rate_limit_watcher() {
    if !is_active() || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut last: Option<crate::provider::RateLimitState> = None;
        let mut ticker = tokio::time::interval(RATE_LIMIT_POLL);
        // Skip the immediate first tick so a daemon with no provider traffic stays quiet.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(state) = crate::provider::last_rate_limit_state() else {
                continue;
            };
            if last.as_ref().is_some_and(|last| same_limits(last, &state)) {
                continue;
            }
            emit(&MonitorEvent::provider_rate_limited(
                String::clone(&state.provider),
                state.requests.remaining,
                state.tokens.remaining,
            ));
            last = Some(state);
        }
    });
}

/// The process-wide monitor slot. A write lock (rather than OnceLock) lets tests swap it.
static MONITOR: OnceLock<RwLock<Option<Arc<Monitor>>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<Arc<Monitor>>> {
    MONITOR.get_or_init(|| RwLock::new(None))
}

/// Install the process-wide monitor from config. The first install wins.
/// Returns whether monitoring is active.
pub fn install(config: &MonitoringConfig) -> bool {
    let monitor = Arc::new(Monitor::from_config(config));
    let active = monitor.is_active();
    if let Ok(mut guard) = slot().write() {
        if guard.is_none() {
            *guard = Some(monitor);
        }
    }
    active
}

/// The installed monitor, when any.
pub fn current() -> Option<Arc<Monitor>> {
    slot()
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(Arc::clone))
}

/// True when a process-wide monitor is installed and active.
pub fn is_active() -> bool {
    current().is_some_and(|monitor| monitor.is_active())
}

/// Emit through the process-wide monitor; a no-op when none is installed.
pub fn emit(event: &MonitorEvent) {
    if let Some(monitor) = current() {
        monitor.emit(event);
    }
}
