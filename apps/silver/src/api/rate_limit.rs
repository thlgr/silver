//! Per-client token-bucket rate limiting: a full bucket of `rate_limit_per_minute` tokens (the
//! burst) refilling at that rate, one token per request.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Upper bound on tracked client keys. Past it, buckets that have fully refilled are dropped so a
/// stream of spoofed forwards cannot grow the map without bound.
const MAX_TRACKED_CLIENTS: usize = 10_000;

/// The outcome of a single rate-limit check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateDecision {
    /// The request fits in the client's budget.
    Allow,
    /// The bucket is empty; the client must wait this many seconds for one token.
    Deny { retry_after_seconds: u64 },
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Token-bucket limiter keyed by an opaque client identity string.
#[derive(Debug)]
pub struct RateLimiter {
    capacity: f64,
    refill_per_second: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Build a limiter admitting `per_minute` requests per client with a burst of the same size.
    ///
    /// A zero limit is disabled: every check is allowed and no state is kept.
    pub fn new(per_minute: u32) -> Self {
        let capacity = per_minute as f64;
        Self {
            capacity,
            refill_per_second: capacity / 60.0,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Check one request for `key` using the wall clock.
    pub fn check(&self, key: &str) -> RateDecision {
        self.check_at(key, Instant::now())
    }

    /// Check one request for `key` at an explicit instant.
    pub fn check_at(&self, key: &str, now: Instant) -> RateDecision {
        if self.capacity <= 0.0 {
            return RateDecision::Allow;
        }
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buckets.len() >= MAX_TRACKED_CLIENTS && !buckets.contains_key(key) {
            buckets.retain(|_, bucket| self.refill(bucket, now) < self.capacity);
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.capacity,
            last_refill: now,
        });
        let available = self.refill(bucket, now);
        if available >= 1.0 {
            bucket.tokens = available - 1.0;
            RateDecision::Allow
        } else {
            let missing = 1.0 - available;
            let seconds = (missing / self.refill_per_second).ceil().max(1.0);
            RateDecision::Deny {
                retry_after_seconds: seconds as u64,
            }
        }
    }

    /// Refill a bucket up to `now`, clamp it at capacity, and return the token count.
    fn refill(&self, bucket: &mut Bucket, now: Instant) -> f64 {
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_second).min(self.capacity);
        bucket.last_refill = now;
        bucket.tokens
    }
}

/// Identify the client behind a request: the peer address when the server supplies
/// [`ConnectInfo`], else the first `X-Forwarded-For` hop, else the shared `"unknown"` key.
pub fn client_key(request: &Request) -> String {
    if let Some(ConnectInfo(addr)) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        return addr.ip().to_string();
    }
    if let Some(value) = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
    {
        if let Some(first) = value
            .split(',')
            .map(str::trim)
            .find(|part| !part.is_empty())
        {
            return first.to_string();
        }
    }
    "unknown".to_string()
}

/// Axum middleware enforcing the shared limiter, answering HTTP 429 when a client's bucket is
/// empty.
pub async fn rate_limit_middleware(
    State(limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    match limiter.check(&client_key(&request)) {
        RateDecision::Allow => next.run(request).await,
        RateDecision::Deny {
            retry_after_seconds,
        } => too_many_requests(retry_after_seconds),
    }
}

fn too_many_requests(retry_after_seconds: u64) -> Response {
    let body = json!({
        "error": {
            "code": "rate_limited",
            "message": format!(
                "rate limit exceeded; retry in {retry_after_seconds} second(s)"
            ),
        }
    });
    let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    let retry_after = HeaderValue::from_str(&retry_after_seconds.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("1"));
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, retry_after);
    response
}
