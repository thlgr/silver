//! Provider status: the last rate-limit headers seen for the configured provider.

use super::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};

/// Response body for `GET /v1/provider/status`; `rate_limit` is null until a response carried
/// recognized rate-limit headers.
pub fn status_body(
    provider: &str,
    model: &str,
    rate_limit: Option<&crate::provider::RateLimitState>,
) -> Value {
    json!({
        "provider": provider,
        "model": model,
        "rate_limit": rate_limit,
    })
}

/// `GET /v1/provider/status`: the configured provider and its last-seen rate-limit state.
pub async fn status(State(state): State<AppState>) -> Json<Value> {
    let provider = if state.config.model.provider.trim().is_empty() {
        state.config.kind().as_str().to_string()
    } else {
        String::clone(&state.config.model.provider)
    };
    Json(status_body(
        &provider,
        state.config.resolved_model(),
        crate::provider::last_rate_limit_state().as_ref(),
    ))
}
