//! Domain errors with stable codes. Binary crates may wrap these with anyhow at the edge.

use silver_protocol::{ApprovalId, ErrorCode, RunId, SessionId, WorkspaceId};
use std::time::Duration;

pub type CoreResult<T> = Result<T, CoreError>;

/// Prefix on a terminal error from an account that cannot pay; the wire code stays
/// ProviderUnavailable, and the tag tells callers to stop retrying or switching credentials.
pub const BILLING_ERROR_TAG: &str = "billing_error:";

/// Prefix on a terminal error naming a model the provider lacks; the wire code stays
/// ProviderUnavailable, and the tag lets the loop fall back to another model.
pub const MODEL_NOT_FOUND_TAG: &str = "model_not_found:";

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("workspace {0} not found")]
    WorkspaceNotFound(WorkspaceId),
    #[error("workspace unavailable: {0}")]
    WorkspaceUnavailable(String),
    #[error("path outside workspace: {0}")]
    PathOutsideWorkspace(String),
    #[error("session {0} not found")]
    SessionNotFound(SessionId),
    #[error("session belongs to a different scope than the run request")]
    SessionWorkspaceMismatch,
    #[error("session {0} already has an active run")]
    SessionBusy(SessionId),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("run {0} not found")]
    RunNotFound(RunId),
    #[error("run {0} is not active")]
    RunNotActive(RunId),
    #[error("tool not allowed: {0}")]
    ToolNotAllowed(String),
    #[error("tool timed out: {0}")]
    ToolTimeout(String),
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("provider rate limited: {0}")]
    ProviderRateLimited(String),
    /// A retryable provider failure (5xx, 408/409/425, transport, overload), with the server's
    /// `Retry-After` and whether upstream called it a rate limit.
    #[error("provider transient error: {message}")]
    ProviderTransient {
        message: String,
        retry_after_ms: Option<u64>,
        rate_limited: bool,
    },
    #[error("context too large: {0}")]
    ContextTooLarge(String),
    #[error("approval {0} not found")]
    ApprovalNotFound(ApprovalId),
    #[error("approval {0} is stale")]
    ApprovalStale(ApprovalId),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl CoreError {
    /// Stable wire code for this error.
    pub fn code(&self) -> ErrorCode {
        match self {
            CoreError::WorkspaceNotFound(_) => ErrorCode::WorkspaceNotFound,
            CoreError::WorkspaceUnavailable(_) => ErrorCode::WorkspaceUnavailable,
            CoreError::PathOutsideWorkspace(_) => ErrorCode::PathOutsideWorkspace,
            CoreError::SessionNotFound(_) => ErrorCode::SessionNotFound,
            CoreError::SessionWorkspaceMismatch => ErrorCode::SessionWorkspaceMismatch,
            CoreError::SessionBusy(_) => ErrorCode::SessionBusy,
            CoreError::Conflict(_) => ErrorCode::Conflict,
            CoreError::RunNotFound(_) => ErrorCode::RunNotFound,
            CoreError::RunNotActive(_) => ErrorCode::RunNotActive,
            CoreError::ToolNotAllowed(_) => ErrorCode::ToolNotAllowed,
            CoreError::ToolTimeout(_) => ErrorCode::ToolTimeout,
            CoreError::ProviderUnavailable(_) => ErrorCode::ProviderUnavailable,
            CoreError::ProviderRateLimited(_) => ErrorCode::ProviderRateLimited,
            CoreError::ProviderTransient { rate_limited, .. } => {
                if *rate_limited {
                    ErrorCode::ProviderRateLimited
                } else {
                    ErrorCode::ProviderUnavailable
                }
            }
            CoreError::ContextTooLarge(_) => ErrorCode::ContextTooLarge,
            CoreError::ApprovalNotFound(_) => ErrorCode::ApprovalNotFound,
            CoreError::ApprovalStale(_) => ErrorCode::ApprovalStale,
            CoreError::InvalidRequest(_) => ErrorCode::InvalidRequest,
            CoreError::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Whether the request exceeded the context window: a dedicated status or a generic 400 naming
    /// the limit, both routed to compaction.
    pub fn is_context_overflow(&self) -> bool {
        match self {
            CoreError::ContextTooLarge(_) => true,
            CoreError::ProviderUnavailable(message) => {
                is_context_length_message(message) || is_payload_too_large_message(message)
            }
            CoreError::ProviderTransient { message, .. } => {
                is_context_length_message(message) || is_payload_too_large_message(message)
            }
            _ => false,
        }
    }

    /// Whether the account cannot serve paid traffic (402, exhausted credits or quota): retrying or
    /// switching models will not help, another credential might.
    pub fn is_billing_error(&self) -> bool {
        match self {
            CoreError::ProviderUnavailable(message) => is_billing_message(message),
            CoreError::ProviderTransient { message, .. } => is_billing_message(message),
            _ => false,
        }
    }

    /// Whether the provider lacks the model (404 or a body naming it unknown): fall back to another
    /// model rather than retry or rotate credentials.
    pub fn is_model_not_found(&self) -> bool {
        match self {
            CoreError::ProviderUnavailable(message) => is_model_not_found_message(message),
            CoreError::ProviderTransient { message, .. } => is_model_not_found_message(message),
            _ => false,
        }
    }

    /// How the loop should treat this failure; a transient delay is the server's `Retry-After`,
    /// which callers should prefer over computed backoff.
    pub fn retry_class(&self) -> RetryClass {
        match self {
            CoreError::ProviderTransient { retry_after_ms, .. } => RetryClass::Transient {
                retry_after: retry_after_ms.map(Duration::from_millis),
            },
            CoreError::ProviderRateLimited(_) => RetryClass::Transient { retry_after: None },
            CoreError::ProviderUnavailable(message)
                if !is_billing_message(message)
                    && !is_model_not_found_message(message)
                    && provider_message_is_transient(message) =>
            {
                RetryClass::Transient { retry_after: None }
            }
            _ => RetryClass::Terminal,
        }
    }
}

/// Retry verdict for a domain error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryClass {
    /// A transient failure the loop may retry, optionally after the server's delay.
    Transient { retry_after: Option<Duration> },
    /// A permanent failure for this turn; retrying will not help.
    Terminal,
}

/// Whether a provider error message reports that the request exceeded the context window.
pub fn is_context_length_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "context length",
        "context_length_exceeded",
        "maximum context",
        "max context",
        "too many tokens",
        "prompt is too long",
        "reduce the length",
        "input is too long",
        // llama.cpp: "request (N tokens) exceeds the available context size (M tokens)"
        "exceeds the available context size",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether a message names an oversized payload; proxies re-wrap 413 as a plain body or a 400 with
/// the byte cap. Routed to the same compaction as a context-length rejection.
pub fn is_payload_too_large_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "request entity too large",
        "payload too large",
        "error code: 413",
        "request_too_large",
        "request exceeds the maximum size",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether a message reports a billing wall; [`BILLING_ERROR_TAG`] is checked first so a truncated
/// tagged body still counts.
pub fn is_billing_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains(BILLING_ERROR_TAG)
        || [
            "insufficient credits",
            "insufficient_quota",
            "insufficient balance",
            "credit balance",
            "credits exhausted",
            "credits have been exhausted",
            "requires available credits",
            "account balance is too low",
            "no usable credits",
            "top up your credits",
            "payment required",
            "billing hard limit",
            "exceeded your current quota",
            "account is deactivated",
            "plan does not include",
            "budget limit exceeded",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Whether a message says the model does not exist: [`MODEL_NOT_FOUND_TAG`] first, then the
/// re-wrapped 400s providers send instead of a 404.
pub fn is_model_not_found_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains(MODEL_NOT_FOUND_TAG)
        || [
            "is not a valid model",
            "invalid model",
            "model not found",
            "model_not_found",
            "does not exist",
            "no such model",
            "unknown model",
            "unsupported model",
            "no endpoints found that support tool use",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// A conservative fallback for transport errors that never had an HTTP status; the provider that
/// saw a response should classify it.
fn provider_message_is_transient(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if let Some(index) = lower.find("http ") {
        let digits: String = lower[index + 5..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(status) = digits.parse::<u16>() {
            return matches!(status, 408 | 409 | 425 | 429) || (500..=599).contains(&status);
        }
    }
    [
        "timed out",
        "timeout",
        "connection",
        "stream stalled",
        "temporarily",
        "overloaded",
        "unavailable",
        "connection reset",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}
