//! OpenAI-compatible chat completions transport: it translates the request and the SSE stream,
//! nothing more.

use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{
    FinishReason, Model, ModelMessage, ModelRequest, ModelStream, ModelStreamEvent, ToolSpec,
};
use silver_core::model_metadata::ModelsCache;
use silver_protocol::providers::ProviderKind;
use silver_protocol::{ContentPart, MessageRole, TokenUsage, ToolCallId};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Default base URL used when 'OPENAI_BASE_URL' is unset.
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Environment variable holding the API key.
const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Environment variable overriding the base URL.
const OPENAI_BASE_URL_ENV: &str = "OPENAI_BASE_URL";

/// Maximum number of characters of a provider error body that is surfaced. OpenRouter wraps the
/// upstream provider's message in ~230 characters of JSON, so a smaller cap hides the cause.
const ERROR_BODY_CHAR_LIMIT: usize = 500;

/// Hint appended when the transport error chain names a DNS or offline failure.
const OFFLINE_HINT: &str =
    "cannot reach the model provider; check your network connection (possible offline or DNS failure)";

/// Lowercase markers a transport error chain uses for an offline/DNS resolution failure.
const NETWORK_RESOLUTION_MARKERS: &[&str] = &[
    "temporary failure in name resolution",
    "name or service not known",
    "nodename nor servname provided",
    "getaddrinfo failed",
    "no address associated with hostname",
    "network is unreachable",
];

/// Streaming client for any OpenAI-compatible chat completions endpoint.
#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    rate_limit: Arc<Mutex<RateLimitState>>,
    /// Extra request headers a gateway demands on top of the bearer token (GitHub Copilot
    /// wants its editor and integration identifiers, for example). Empty for a plain endpoint.
    extra_headers: Vec<(String, String)>,
}

impl OpenAiCompatibleProvider {
    /// Create a provider for an OpenAI-compatible endpoint.
    ///
    /// The base URL is normalized by trimming whitespace and any trailing slash.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: normalize_base_url(&base_url.into()),
            api_key: api_key.into(),
            model: model.into(),
            http: reqwest::Client::new(),
            rate_limit: Arc::new(Mutex::new(RateLimitState::default())),
            extra_headers: Vec::new(),
        }
    }

    /// The most recent rate-limit state seen on this transport, if any.
    pub fn rate_limit_state(&self) -> Option<RateLimitState> {
        let state = RateLimitState::clone(&self.rate_limit.lock().expect("rate limit lock"));
        state.has_data().then_some(state)
    }

    /// Capture rate-limit headers from the most recent response.
    fn capture_rate_limit(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(state) = rate_limit_from_headers(headers, "openai_compatible") {
            store_rate_limit(&self.rate_limit, state);
        }
    }

    /// Configured model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Normalized base URL (never has a trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Send these headers with every request, in addition to the bearer token.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }
}

/// A provider from `OPENAI_API_KEY` (required) and `OPENAI_BASE_URL` (default
/// `https://api.openai.com/v1`); None without a key.
pub fn provider_from_env(model: &str) -> Option<OpenAiCompatibleProvider> {
    let api_key = std::env::var(OPENAI_API_KEY_ENV).ok()?;
    if api_key.trim().is_empty() {
        return None;
    }
    let base_url = std::env::var(OPENAI_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
    Some(OpenAiCompatibleProvider::new(base_url, api_key, model))
}

/// Timeout for the best-effort `/models` context probe.
const MODELS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// One rate-limit window parsed from response headers: `requests` or `tokens`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitBucket {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
    /// Seconds until the window resets, when the header could be reduced to a duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_seconds: Option<f64>,
    /// The raw reset header, preserved for absolute (Anthropic) timestamps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<String>,
}

/// Last-seen provider rate-limit state, captured from a response's headers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitState {
    /// Wire protocol that reported these headers ("openai_compatible" or "anthropic").
    pub provider: String,
    pub requests: RateLimitBucket,
    pub tokens: RateLimitBucket,
    /// RFC 3339 capture time; None means no response has carried rate-limit headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<DateTime<Utc>>,
}

impl RateLimitState {
    /// True when at least one response carried recognized rate-limit headers.
    pub fn has_data(&self) -> bool {
        self.captured_at.is_some()
    }
}

/// Process-wide last-seen rate-limit state, surfaced by `GET /v1/provider/status`.
static LAST_RATE_LIMIT: OnceLock<Mutex<Option<RateLimitState>>> = OnceLock::new();

fn last_rate_limit_slot() -> &'static Mutex<Option<RateLimitState>> {
    LAST_RATE_LIMIT.get_or_init(|| Mutex::new(None))
}

/// Record the most recent provider rate-limit state for the status endpoint.
pub fn record_rate_limit_state(state: RateLimitState) {
    *last_rate_limit_slot().lock().expect("rate limit lock") = Some(state);
}

/// The most recent provider rate-limit state across all transports, if any.
pub fn last_rate_limit_state() -> Option<RateLimitState> {
    let slot = last_rate_limit_slot().lock().expect("rate limit lock");
    Option::clone(&slot)
}

/// Store a captured state on one transport slot and the process-wide status slot.
pub(crate) fn store_rate_limit(slot: &Mutex<RateLimitState>, state: RateLimitState) {
    *slot.lock().expect("rate limit lock") = RateLimitState::clone(&state);
    record_rate_limit_state(state);
}

/// Parse rate-limit headers from a response into a state, or None when none are present.
pub(crate) fn rate_limit_from_headers(
    headers: &reqwest::header::HeaderMap,
    provider: &str,
) -> Option<RateLimitState> {
    let pairs = headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value)));
    parse_rate_limit_headers(pairs, provider, Utc::now())
}

/// Rate-limit state from OpenAI-style `x-ratelimit-{limit,remaining,reset}-{requests,tokens}` or
/// Anthropic `anthropic-ratelimit-{requests,tokens}-{limit,remaining,reset}` headers, if any.
pub fn parse_rate_limit_headers<'a, I>(
    headers: I,
    provider: &str,
    now: DateTime<Utc>,
) -> Option<RateLimitState>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let pairs: Vec<(String, String)> = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let (requests, requests_present) = rate_limit_bucket(&pairs, "requests", now);
    let (tokens, tokens_present) = rate_limit_bucket(&pairs, "tokens", now);
    if !requests_present && !tokens_present {
        return None;
    }
    Some(RateLimitState {
        provider: provider.to_string(),
        requests,
        tokens,
        captured_at: Some(now),
    })
}

/// Parse one window's limit/remaining/reset triple in either header spelling.
///
/// Returns the bucket and whether any of its headers were present.
fn rate_limit_bucket(
    pairs: &[(String, String)],
    tag: &str,
    now: DateTime<Utc>,
) -> (RateLimitBucket, bool) {
    let limit = header_value(pairs, &format!("anthropic-ratelimit-{tag}-limit"))
        .or_else(|| header_value(pairs, &format!("x-ratelimit-limit-{tag}")));
    let remaining = header_value(pairs, &format!("anthropic-ratelimit-{tag}-remaining"))
        .or_else(|| header_value(pairs, &format!("x-ratelimit-remaining-{tag}")));
    let reset = header_value(pairs, &format!("anthropic-ratelimit-{tag}-reset"))
        .or_else(|| header_value(pairs, &format!("x-ratelimit-reset-{tag}")));
    let present = limit.is_some() || remaining.is_some() || reset.is_some();
    (
        RateLimitBucket {
            limit: limit.and_then(|value| value.parse().ok()),
            remaining: remaining.and_then(|value| value.parse().ok()),
            reset_seconds: reset.and_then(|value| parse_reset_seconds(value, now)),
            reset: reset.map(str::to_string),
        },
        present,
    )
}

fn header_value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Seconds until a reset header: bare seconds, a Go duration (`6m0s`, `500ms`) or an RFC 3339 /
/// RFC 2822 timestamp (Anthropic). A past time is zero.
pub fn parse_reset_seconds(value: &str, now: DateTime<Utc>) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.parse::<f64>() {
        if seconds.is_finite() {
            return Some(seconds.max(0.0));
        }
        return None;
    }
    if let Some(seconds) = parse_duration_seconds(value) {
        return Some(seconds.max(0.0));
    }
    let absolute = DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_rfc2822(value))
        .ok()?;
    let seconds = (absolute.with_timezone(&Utc) - now).num_milliseconds() as f64 / 1000.0;
    Some(seconds.max(0.0))
}

/// Seconds from a Go-style duration such as `1h2m3s` or `500ms`.
fn parse_duration_seconds(value: &str) -> Option<f64> {
    let mut total = 0.0f64;
    let mut rest = value;
    let mut matched = false;
    while !rest.is_empty() {
        let number_len = rest
            .find(|character: char| !(character.is_ascii_digit() || character == '.'))
            .unwrap_or(rest.len());
        if number_len == 0 {
            return None;
        }
        let number: f64 = rest[..number_len].parse().ok()?;
        rest = &rest[number_len..];
        let (factor, consumed) = if rest.starts_with("ms") {
            (1e-3, 2)
        } else if rest.starts_with("us") {
            (1e-6, 2)
        } else if rest.starts_with("µs") {
            (1e-6, "µs".len())
        } else if rest.starts_with("ns") {
            (1e-9, 2)
        } else if rest.starts_with('h') {
            (3600.0, 1)
        } else if rest.starts_with('m') {
            (60.0, 1)
        } else if rest.starts_with('s') {
            (1.0, 1)
        } else {
            return None;
        };
        total += number * factor;
        rest = &rest[consumed..];
        matched = true;
    }
    matched.then_some(total)
}

/// The configured model's window: config override, disk cache, `/models` probe, a local
/// server's native API (set through `[model].base_url`, so only it reports the loaded window),
/// then the static table; probe failures fall through.
pub async fn resolve_model_context_length(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    data_dir: &Path,
    kind: ProviderKind,
    config_override: Option<usize>,
) -> Option<usize> {
    if config_override.is_some_and(|value| value > 0) {
        return config_override;
    }
    let cache_path = data_dir.join(silver_core::model_metadata::MODELS_CACHE_FILE);
    // A local server is asked live, before the cache: the window it loaded the model with
    // can change between starts, and a cached maximum learned while the model was not loaded
    // must not shadow it. Only a loaded window is remembered.
    if crate::context_length::is_local_base_url(base_url) || kind == ProviderKind::Ollama {
        if let Some(window) = crate::context_length::probe_local_window(http, base_url, model).await
        {
            if window.loaded {
                let mut cache = ModelsCache::load(&cache_path);
                cache.insert(model, window.tokens);
                if let Err(error) = cache.save(&cache_path) {
                    tracing::debug!(%error, "could not persist the models context cache");
                }
            }
            tracing::info!(
                %model,
                context_length = window.tokens,
                loaded = window.loaded,
                "local server reported the model's context window"
            );
            return Some(window.tokens);
        }
    }
    let learned = match silver_core::model_metadata::cached_context_length(data_dir, model) {
        Some(value) => Some(value),
        None => probe_context_length(http, base_url, api_key, model, &cache_path, kind).await,
    };
    silver_core::model_metadata::resolve_context_length(config_override, learned, model)
}

/// The context window `/models` reports for the exact model id, merged into the disk cache.
pub async fn probe_context_length(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    cache_path: &Path,
    kind: ProviderKind,
) -> Option<usize> {
    let body = fetch_models(http, base_url, api_key, kind).await?;
    let length = silver_core::model_metadata::parse_models_context_length(&body, model)?;
    let mut cache = ModelsCache::load(cache_path);
    cache.insert(model, length);
    if let Err(error) = cache.save(cache_path) {
        tracing::debug!(%error, "could not persist the models context cache");
    }
    Some(length)
}

fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Whether a server answers at `base_url`; any HTTP status counts as an answer.
pub async fn endpoint_reachable(http: &reqwest::Client, base_url: &str) -> bool {
    http.get(models_url(base_url))
        .timeout(MODELS_PROBE_TIMEOUT)
        .send()
        .await
        .is_ok()
}

/// The body of `<base_url>/models`, or None on any failure. Copilot's catalog rejects the
/// GitHub token, so its caller passes the minted bearer.
async fn fetch_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    kind: ProviderKind,
) -> Option<String> {
    let request = http.get(models_url(base_url)).timeout(MODELS_PROBE_TIMEOUT);
    let request = match kind {
        ProviderKind::Anthropic => request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        _ if api_key.is_empty() => request,
        _ => request.bearer_auth(api_key),
    };
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.text().await.ok()
}

/// The model ids `/models` advertises, so `/login` can pick one for a catalog-driven endpoint;
/// empty on any failure.
pub async fn list_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    kind: ProviderKind,
) -> Vec<String> {
    let body = fetch_models(http, base_url, api_key, kind).await;
    let body: Value = body
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default();
    body.get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("id")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
        })
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

#[async_trait::async_trait]
impl Model for OpenAiCompatibleProvider {
    fn name(&self) -> &str {
        &self.model
    }

    fn is_local(&self) -> bool {
        silver_protocol::providers::is_local_endpoint(&self.base_url)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let model = if request.model.is_empty() {
            self.model.as_str()
        } else {
            request.model.as_str()
        };
        let body = build_request_body(model, &request);
        let url = format!("{}/chat/completions", self.base_url);

        let mut http_request = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        for (name, value) in &self.extra_headers {
            http_request = http_request.header(name, value);
        }
        let http_request = http_request.json(&body);

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(CoreError::ProviderUnavailable(
                    "provider request cancelled".to_string(),
                ));
            }
            result = http_request.send() => result,
        };

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let detail = sanitize_body_with_hint(
                    &error_chain(&error),
                    &self.api_key,
                    offline_error_hint(&error),
                );
                return Err(CoreError::ProviderUnavailable(format!(
                    "provider request failed: {detail}"
                )));
            }
        };

        self.capture_rate_limit(response.headers());

        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            let body_text = response.text().await.unwrap_or_default();
            return Err(map_http_error(
                silver_protocol::providers::label_for_base_url(&self.base_url),
                status.as_u16(),
                &body_text,
                &self.api_key,
                retry_after,
            ));
        }

        let stream_key = String::clone(&self.api_key);
        let byte_stream = response.bytes_stream().map(move |item| {
            item.map(|bytes| bytes.to_vec()).map_err(|error| {
                CoreError::ProviderUnavailable(format!(
                    "provider stream failed: {}",
                    redact_secret(&error_chain(&error), &stream_key)
                ))
            })
        });

        Ok(Box::pin(sse_event_stream(byte_stream, cancel)))
    }
}

/// Mutable state carried through the SSE decoding stream.
struct SseState {
    bytes: Pin<Box<dyn futures::Stream<Item = CoreResult<Vec<u8>>> + Send>>,
    buffer: String,
    cancel: CancellationToken,
    pending: VecDeque<CoreResult<ModelStreamEvent>>,
    done: bool,
    /// Whether a finish_reason arrived; a body that ends without one or '[DONE]' was cut off.
    finished: bool,
    scrubber: StreamingThinkScrubber,
}

/// Decode a raw byte stream of 'text/event-stream' data into model events.
fn sse_event_stream<S>(
    bytes: S,
    cancel: CancellationToken,
) -> impl futures::Stream<Item = CoreResult<ModelStreamEvent>> + Send + 'static
where
    S: futures::Stream<Item = CoreResult<Vec<u8>>> + Send + 'static,
{
    let state = SseState {
        bytes: Box::pin(bytes),
        buffer: String::new(),
        cancel,
        pending: VecDeque::new(),
        done: false,
        finished: false,
        scrubber: StreamingThinkScrubber::default(),
    };

    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((event, state));
            }
            if state.done {
                return None;
            }

            let cancel = CancellationToken::clone(&state.cancel);
            let next = tokio::select! {
                _ = cancel.cancelled() => None,
                item = state.bytes.next() => Some(item),
            };

            match next {
                // The cancellation token fired; abort by dropping the response.
                None => {
                    state.done = true;
                    state.pending.push_back(Err(CoreError::ProviderUnavailable(
                        "provider stream cancelled".to_string(),
                    )));
                }
                // End of the HTTP body: flush any trailing frame, then release any held-back
                // visible prose (or drop an unterminated reasoning span).
                Some(None) => {
                    let frames = drain_sse_frames(&mut state.buffer, true);
                    for frame in frames {
                        push_frame(&mut state, &frame);
                    }
                    flush_scrubber(&mut state);
                    state.done = true;
                    // A crashed or restarted local server ends the body mid-reply; finishing
                    // quietly would pass the partial reply off as complete.
                    if !state.finished {
                        state.pending.push_back(Err(CoreError::ProviderUnavailable(
                            "the model server closed the stream before the reply finished"
                                .to_string(),
                        )));
                    }
                }
                Some(Some(Err(error))) => {
                    state.done = true;
                    state.pending.push_back(Err(error));
                }
                Some(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk).replace('\r', "");
                    state.buffer.push_str(&text);
                    let frames = drain_sse_frames(&mut state.buffer, false);
                    for frame in frames {
                        push_frame(&mut state, &frame);
                    }
                }
            }
        }
    })
}

/// Parse a frame and queue its events, marking the stream done on '[DONE]'.
fn push_frame(state: &mut SseState, frame: &str) {
    match parse_sse_frame(frame, &mut state.scrubber) {
        Ok((done, events)) => {
            state.finished |= events
                .iter()
                .any(|event| matches!(event, ModelStreamEvent::Finish(_)));
            state.pending.extend(events.into_iter().map(Ok));
            if done {
                // A terminal frame ends the stream: release held-back visible prose now so the
                // final delta is not lost.
                flush_scrubber(state);
                state.done = true;
            }
        }
        Err(error) => {
            state.done = true;
            state.pending.push_back(Err(error));
        }
    }
}

/// Release prose the scrubber held back, queueing it as a final text delta when non-empty.
fn flush_scrubber(state: &mut SseState) {
    let visible = state.scrubber.flush();
    if !visible.is_empty() {
        state
            .pending
            .push_back(Ok(ModelStreamEvent::TextDelta(visible)));
    }
}

/// The classification of a single SSE line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SseLine<'a> {
    /// A non-empty 'data:' payload.
    Data(&'a str),
    /// The '[DONE]' sentinel.
    Done,
    /// A comment, blank line or any non-data field.
    Ignore,
}

/// Classify one physical line of an SSE frame.
fn parse_sse_line(line: &str) -> SseLine<'_> {
    let line = line.trim_end_matches('\n');
    if line.is_empty() || line.starts_with(':') {
        return SseLine::Ignore;
    }
    let Some(rest) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };
    let payload = rest.strip_prefix(' ').unwrap_or(rest).trim();
    if payload == "[DONE]" {
        SseLine::Done
    } else if payload.is_empty() {
        SseLine::Ignore
    } else {
        SseLine::Data(payload)
    }
}

/// Split complete SSE frames (blank-line separated) off the buffer; at end of stream the trailing
/// partial frame is returned too. Carriage returns are stripped on ingest.
pub(crate) fn drain_sse_frames(buffer: &mut String, at_eof: bool) -> Vec<String> {
    let mut frames = Vec::new();
    loop {
        if let Some(index) = buffer.find("\n\n") {
            frames.push(buffer[..index].to_string());
            let remainder = buffer[index + 2..].to_string();
            *buffer = remainder;
        } else if at_eof {
            let remainder = std::mem::take(buffer);
            let trimmed = remainder.trim_end_matches('\n');
            if !trimmed.trim().is_empty() {
                frames.push(trimmed.to_string());
            }
            break;
        } else {
            break;
        }
    }
    frames
}

/// Literal opening reasoning tags the scrubber recognizes.
const THINK_OPEN_TAGS: [&str; 3] = ["<think>", "<thinking>", "<reasoning>"];
/// Literal closing reasoning tags the scrubber recognizes.
const THINK_CLOSE_TAGS: [&str; 3] = ["</think>", "</thinking>", "</reasoning>"];
/// Every recognized tag (open plus close), for partial-tag buffering.
const THINK_ALL_TAGS: [&str; 6] = [
    "<think>",
    "<thinking>",
    "<reasoning>",
    "</think>",
    "</thinking>",
    "</reasoning>",
];
/// Byte length of the longest recognized tag (used to bound partial-tag holds).
const THINK_MAX_TAG_LEN: usize = "</thinking>".len();

/// Moves `<think>…</think>` spans (and sibling tags, any case) out of streamed text into
/// [`Self::take_reasoning`], holding back a tag split across chunks. Call flush at end of stream:
/// held-back prose is released and an unterminated span dropped.
#[derive(Debug, Default)]
pub(crate) struct StreamingThinkScrubber {
    /// True while inside an open span whose text is diverted to `reasoning`.
    in_block: bool,
    /// Held-back tail that may be the prefix of a tag split across deltas.
    buffer: String,
    /// Span text diverted since the last `take_reasoning`.
    reasoning: String,
}

impl StreamingThinkScrubber {
    /// Feed one content delta and return the visible portion (empty when fully discarded).
    pub(crate) fn feed(&mut self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut buf = std::mem::take(&mut self.buffer);
        buf.push_str(text);
        let mut out = String::new();
        while !buf.is_empty() {
            if self.in_block {
                if let Some((index, length)) = find_first_tag(&buf, &THINK_CLOSE_TAGS) {
                    self.reasoning.push_str(&buf[..index]);
                    buf = buf[index + length..].to_string();
                    self.in_block = false;
                    continue;
                }
                // No close tag yet: hold a possible partial close-tag prefix and divert the
                // rest to the reasoning stream.
                let thought = self.hold_partial(buf, &THINK_CLOSE_TAGS);
                self.reasoning.push_str(&thought);
                break;
            }
            if let Some((index, length)) = find_first_tag(&buf, &THINK_OPEN_TAGS) {
                emit_visible(&mut out, &buf[..index]);
                self.in_block = true;
                buf = buf[index + length..].to_string();
                continue;
            }
            // No resolvable tag: hold a partial tag prefix so a split tag is not missed, then
            // emit the remainder with orphan close tags stripped.
            let remainder = self.hold_partial(buf, &THINK_ALL_TAGS);
            emit_visible(&mut out, &remainder);
            break;
        }
        out
    }

    /// Reasoning diverted from the visible stream since the last call.
    pub(crate) fn take_reasoning(&mut self) -> String {
        std::mem::take(&mut self.reasoning)
    }

    /// End-of-stream flush: held-back prose is released, unless the stream ended inside an
    /// unterminated span (whose tail is dropped). Partial tag prefixes are discarded.
    pub(crate) fn flush(&mut self) -> String {
        let tail = if self.in_block {
            String::new()
        } else {
            std::mem::take(&mut self.buffer)
        };
        self.buffer.clear();
        self.in_block = false;
        strip_orphan_close_tags(&tail)
    }

    /// Move a trailing partial-tag prefix of buf into the held-back buffer and return the
    /// remaining text (the part known not to be a tag prefix). A full tag match is never a
    /// prefix, so it is handled by the caller before this runs.
    fn hold_partial(&mut self, buf: String, tags: &[&str]) -> String {
        let held = max_partial_suffix(&buf, tags);
        if held == 0 {
            self.buffer.clear();
            return buf;
        }
        let split = buf.len() - held;
        self.buffer = buf[split..].to_string();
        buf[..split].to_string()
    }
}

/// Earliest (index, length) over tags in buf (case-insensitive), or None.
fn find_first_tag(buf: &str, tags: &[&str]) -> Option<(usize, usize)> {
    let lower = buf.to_ascii_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for tag in tags {
        if let Some(index) = lower.find(tag) {
            let candidate = (index, tag.len());
            if best.is_none_or(|current| candidate < current) {
                best = Some(candidate);
            }
        }
    }
    best
}

/// Longest suffix of buf that is a strict prefix of one of tags.
fn max_partial_suffix(buf: &str, tags: &[&str]) -> usize {
    let lower = buf.to_ascii_lowercase();
    let limit = (THINK_MAX_TAG_LEN - 1).min(lower.len());
    for length in (1..=limit).rev() {
        let start = lower.len() - length;
        if !lower.is_char_boundary(start) {
            continue;
        }
        let suffix = &lower[start..];
        if tags
            .iter()
            .any(|tag| tag.len() > length && tag.starts_with(suffix))
        {
            return length;
        }
    }
    0
}

/// Append text to out, dropping orphan close tags and their trailing whitespace.
fn emit_visible(out: &mut String, text: &str) {
    let stripped = strip_orphan_close_tags(text);
    if !stripped.is_empty() {
        out.push_str(&stripped);
    }
}

/// Remove close tags with no matching open (always noise) plus any trailing whitespace.
fn strip_orphan_close_tags(text: &str) -> String {
    if !text.contains("</") {
        return text.to_string();
    }
    let mut out = String::new();
    let mut rest = text;
    loop {
        match find_first_tag(rest, &THINK_CLOSE_TAGS) {
            Some((index, length)) => {
                out.push_str(&rest[..index]);
                rest = rest[index + length..].trim_start_matches([' ', '\t', '\n', '\r']);
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Parse one SSE frame into a done flag and the events it carried.
fn parse_sse_frame(
    frame: &str,
    scrubber: &mut StreamingThinkScrubber,
) -> CoreResult<(bool, Vec<ModelStreamEvent>)> {
    let mut done = false;
    let mut events = Vec::new();
    for line in frame.split('\n') {
        match parse_sse_line(line) {
            SseLine::Data(payload) => events.extend(parse_chunk(payload, scrubber)?),
            SseLine::Done => done = true,
            SseLine::Ignore => {}
        }
    }
    Ok((done, events))
}

/// Parse one 'data:' JSON payload into zero or more stream events.
fn parse_chunk(
    payload: &str,
    scrubber: &mut StreamingThinkScrubber,
) -> CoreResult<Vec<ModelStreamEvent>> {
    let mut value: Value = serde_json::from_str(payload).map_err(|error| {
        CoreError::ProviderUnavailable(format!("invalid provider chunk: {error}"))
    })?;

    // Some gateways report a failure as a normal SSE data frame instead of an HTTP error.
    if let Some(error) = value.get("error") {
        return Err(stream_error(error));
    }

    let mut events = Vec::new();

    if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if let Some(delta) = choice.get_mut("delta") {
                let content = delta.get("content").and_then(Value::as_str).unwrap_or("");
                if !content.is_empty() {
                    // Reasoning tags can straddle delta boundaries, so content passes through
                    // the per-stream scrubber before it becomes a visible text delta; what
                    // the tags enclosed becomes reasoning.
                    let visible = scrubber.feed(content);
                    let thought = scrubber.take_reasoning();
                    if !thought.is_empty() {
                        events.push(ModelStreamEvent::ReasoningDelta(thought));
                    }
                    if !visible.is_empty() {
                        events.push(ModelStreamEvent::TextDelta(visible));
                    }
                }

                let reasoning = take_str(delta, "reasoning_content")
                    .or_else(|| take_str(delta, "reasoning"))
                    .unwrap_or_default();
                if !reasoning.is_empty() {
                    events.push(ModelStreamEvent::ReasoningDelta(reasoning));
                }

                if let Some(tool_calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut)
                {
                    for tool_call in tool_calls {
                        events.push(parse_tool_call_delta(tool_call));
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                events.push(ModelStreamEvent::Finish(map_finish_reason(reason)));
            }
        }
    }

    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        events.push(ModelStreamEvent::Usage(parse_usage(usage)));
    }

    Ok(events)
}

/// The string at `key`, taken out of the chunk; None when absent or not a string.
fn take_str(object: &mut Value, key: &str) -> Option<String> {
    match object.get_mut(key)? {
        Value::String(text) => Some(std::mem::take(text)),
        _ => None,
    }
}

/// Parse a single 'tool_calls' delta entry.
fn parse_tool_call_delta(tool_call: &mut Value) -> ModelStreamEvent {
    let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<ToolCallId>().ok());
    let function = tool_call.get("function");
    let name = function
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // Some servers send the arguments as a JSON object rather than the string OpenAI specifies.
    let arguments_delta = match tool_call
        .pointer_mut("/function/arguments")
        .map(Value::take)
    {
        Some(Value::String(text)) => text,
        Some(Value::Null) | None => String::new(),
        Some(object) => object.to_string(),
    };
    ModelStreamEvent::ToolCallDelta {
        index,
        id,
        name,
        arguments_delta,
    }
}

/// Token accounting from `usage`; cache and reasoning splits come from
/// `prompt_tokens_details.cached_tokens` and `completion_tokens_details.reasoning_tokens`.
fn parse_usage(usage: &Value) -> TokenUsage {
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_tokens,
        reasoning_tokens,
    }
}

/// Map an OpenAI finish reason to the core enum.
///
/// Unknown reasons collapse to 'Stop' as required by the transport contract.
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

/// Build the JSON body for a streaming chat completions request.
fn build_request_body(model: &str, request: &ModelRequest) -> Value {
    let mut body = json!({
        "model": model,
        "messages": map_messages(&request.messages, reasoning_passthrough(model)),
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(map_tools(&request.tools, model));
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        // o-series and gpt-5 style models reject the legacy max_tokens field and take
        // max_completion_tokens instead; every other family keeps max_tokens.
        if uses_max_completion_tokens(model) {
            body["max_completion_tokens"] = json!(max_tokens);
        } else {
            body["max_tokens"] = json!(max_tokens);
        }
    }
    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .and_then(clamp_reasoning_effort)
    {
        body["reasoning_effort"] = json!(effort);
    }
    // A stable per-conversation key lets a cache-aware endpoint route every turn to the
    // same warm prefix. Empty keys are ignored rather than sent as an empty string.
    if let Some(cache_key) = request.cache_key.as_deref().filter(|key| !key.is_empty()) {
        body["prompt_cache_key"] = json!(cache_key);
    }

    body
}

/// Reasoning-effort levels accepted verbatim on the OpenAI-compatible wire.
const OPENAI_REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high"];

/// Clamp an effort onto the OpenAI vocabulary: extended levels (xhigh, max, ultra) fall to the
/// nearest weaker one so a clamp never raises cost; an unknown label is medium.
// The ordering ladder lives in config alongside the accepted set.
fn clamp_reasoning_effort(effort: &str) -> Option<&'static str> {
    let normalized = effort.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    if let Some(accepted) = OPENAI_REASONING_EFFORTS
        .iter()
        .copied()
        .find(|level| *level == normalized.as_str())
    {
        return Some(accepted);
    }
    let index = crate::config::REASONING_EFFORT_LADDER
        .iter()
        .copied()
        .position(|level| level == normalized.as_str());
    Some(match index {
        Some(index) => crate::config::REASONING_EFFORT_LADDER[..index]
            .iter()
            .rev()
            .copied()
            .find(|level| OPENAI_REASONING_EFFORTS.contains(level))
            .unwrap_or("medium"),
        None => "medium",
    })
}

/// The o-series and GPT-5 reject `max_tokens` and take `max_completion_tokens`.
fn uses_max_completion_tokens(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or(lower.as_str());
    let o_series = bare
        .strip_prefix('o')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|digit| digit.is_ascii_digit());
    o_series || bare.starts_with("gpt-5") || bare.starts_with("gpt5")
}

/// DeepSeek thinking mode (the default with tools) rejects a history where any assistant turn lacks
/// reasoning_content, even turns stored before it was tracked; an empty string is accepted.
fn reasoning_passthrough(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("deepseek") || lower.contains("reasoner")
}

/// Core messages as OpenAI chat messages; each tool result becomes its own `tool` message.
fn map_messages(messages: &[ModelMessage], reasoning_passthrough: bool) -> Vec<Value> {
    let mut out = Vec::new();

    for message in messages {
        match message.role {
            MessageRole::System => {
                out.push(json!({
                    "role": "system",
                    "content": collect_text(&message.content),
                }));
            }
            MessageRole::User => {
                out.push(json!({
                    "role": "user",
                    "content": user_content(&message.content),
                }));
            }
            MessageRole::Assistant => {
                let text = collect_text(&message.content);
                let reasoning = message
                    .content
                    .iter()
                    .filter_map(ContentPart::as_reasoning)
                    .collect::<Vec<_>>()
                    .join("\n");
                let tool_calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id.to_string(),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            },
                        })),
                        _ => None,
                    })
                    .collect();

                let content = if tool_calls.is_empty() || !text.is_empty() {
                    json!(text)
                } else {
                    Value::Null
                };

                let mut object = Map::new();
                object.insert("role".to_string(), json!("assistant"));
                object.insert("content".to_string(), content);
                if reasoning_passthrough {
                    // Every assistant turn must carry the field, even a legacy turn with
                    // no stored reasoning (the API accepts an empty string).
                    object.insert("reasoning_content".to_string(), json!(reasoning));
                } else if !reasoning.is_empty() {
                    object.insert("reasoning_content".to_string(), json!(reasoning));
                }
                if !tool_calls.is_empty() {
                    object.insert("tool_calls".to_string(), Value::Array(tool_calls));
                }
                out.push(Value::Object(object));
            }
            MessageRole::Tool => {
                for part in &message.content {
                    if let ContentPart::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } = part
                    {
                        // Cohere (via OpenRouter) rejects an empty tool result with HTTP 400,
                        // and a blank result tells a small model nothing either.
                        let content = if content.trim().is_empty() {
                            "(no output)"
                        } else {
                            content
                        };
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id.to_string(),
                            "content": content,
                        }));
                    }
                }
                // A `tool` message carries text only, so a loaded picture rides the next
                // user turn, which is where every OpenAI-compatible route accepts an image.
                let images = image_blocks(&message.content, image_url_block);
                if !images.is_empty() {
                    out.push(json!({"role": "user", "content": images}));
                }
            }
        }
    }

    out
}

/// Tool specs as OpenAI function tools; Moonshot/Kimi get their stricter schema flavor.
fn map_tools(tools: &[ToolSpec], model: &str) -> Vec<Value> {
    let moonshot = is_moonshot_model(model);
    tools
        .iter()
        .map(|tool| {
            let parameters = if moonshot {
                Cow::Owned(sanitize_moonshot_parameters(&tool.parameters))
            } else {
                Cow::Borrowed(&tool.parameters)
            };
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": parameters,
                },
            })
        })
        .collect()
}

/// Whether the model is served by Moonshot/Kimi, which rejects a stricter schema subset.
fn is_moonshot_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("kimi") || lower.contains("moonshot")
}

/// Force a top-level object schema with a properties map and drop `$schema` at every level.
fn sanitize_moonshot_parameters(parameters: &Value) -> Value {
    let mut schema = Value::clone(parameters);
    strip_schema_keyword(&mut schema);
    if !schema.is_object() {
        return json!({"type": "object", "properties": {}});
    }
    let object = schema.as_object_mut().expect("value is an object");
    object.insert("type".to_string(), json!("object"));
    if !object.contains_key("properties") {
        object.insert("properties".to_string(), json!({}));
    }
    schema
}

/// Remove the unsupported $schema keyword from a schema tree in place.
fn strip_schema_keyword(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            for child in map.values_mut() {
                strip_schema_keyword(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_schema_keyword(item);
            }
        }
        _ => {}
    }
}

/// A user message's content: a bare string while it is text only, and a block array once it
/// carries a picture, because no route accepts an image inside a string.
fn user_content(parts: &[ContentPart]) -> Value {
    if !parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image { .. }))
    {
        return json!(collect_text(parts));
    }
    Value::Array(
        parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } if !text.is_empty() => {
                    Some(json!({"type": "text", "text": text}))
                }
                _ => image_blocks(std::slice::from_ref(part), image_url_block)
                    .into_iter()
                    .next(),
            })
            .collect(),
    )
}

/// One wire block per image part, rendered by `render`.
fn image_blocks(parts: &[ContentPart], render: fn(&str, &str) -> Value) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(render(media_type, data)),
            _ => None,
        })
        .collect()
}

/// The OpenAI chat-completions form of one inline image.
fn image_url_block(media_type: &str, data: &str) -> Value {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{media_type};base64,{data}") },
    })
}

/// Join the text parts of a message with newlines.
fn collect_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Trim whitespace and trailing slashes from a base URL.
pub(crate) fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// A non-success response as a domain error, never leaking the key. 401/403 are terminal auth
/// failures; 408/409/425/429 and 5xx are transient, with the server's `Retry-After`.
pub(crate) fn map_http_error(
    provider: &str,
    status: u16,
    body: &str,
    api_key: &str,
    retry_after: Option<Duration>,
) -> CoreError {
    let detail = sanitize_body(body, api_key);
    if matches!(status, 401 | 403) {
        // Pasted provider bodies here are long and useless to the reader; say what to do.
        return CoreError::ProviderUnavailable(if api_key.is_empty() {
            format!("{provider} needs an API key. Add one in Settings → Providers.")
        } else {
            format!("{provider} rejected the API key (HTTP {status}). Check it in Settings → Providers.")
        });
    }
    // Classify on the raw body: the sanitized detail is capped at 500 chars, so a long
    // proxy page could hide the marker past the truncation point.
    let raw_lower = body.to_ascii_lowercase();
    if status == 413 || silver_core::error::is_payload_too_large_message(&raw_lower) {
        return CoreError::ContextTooLarge(if detail.is_empty() {
            format!("payload too large (HTTP {status})")
        } else {
            format!("payload too large (HTTP {status}): {detail}")
        });
    }
    if is_context_length_error(&raw_lower) {
        return CoreError::ContextTooLarge(if detail.is_empty() {
            format!("context too large (HTTP {status})")
        } else {
            format!("context too large (HTTP {status}): {detail}")
        });
    }
    let rate_limited = status == 429;
    let transient =
        rate_limited || matches!(status, 408 | 409 | 425) || (500..=599).contains(&status);
    let message = if detail.is_empty() {
        format!("provider returned HTTP {status}")
    } else {
        format!("provider returned HTTP {status}: {detail}")
    };
    if transient {
        CoreError::ProviderTransient {
            message,
            retry_after_ms: retry_after.map(|delay| delay.as_millis() as u64),
            rate_limited,
        }
    } else if status == 402 || silver_core::error::is_billing_message(&raw_lower) {
        CoreError::ProviderUnavailable(format!(
            "{}{message}",
            silver_core::error::BILLING_ERROR_TAG
        ))
    } else if status == 404 || silver_core::error::is_model_not_found_message(&raw_lower) {
        CoreError::ProviderUnavailable(format!(
            "{}{message}",
            silver_core::error::MODEL_NOT_FOUND_TAG
        ))
    } else if raw_lower.contains("failed to load model") {
        CoreError::ProviderUnavailable(format!(
            "{message} Load the model in LM Studio or Ollama, then retry."
        ))
    } else {
        CoreError::ProviderUnavailable(message)
    }
}

/// Whether a provider error body reports that the request exceeded the context window.
///
/// Single source of truth lives in silver-core so every transport agrees on the shape.
fn is_context_length_error(detail: &str) -> bool {
    silver_core::error::is_context_length_message(detail)
}

/// An in-stream `{"error": ...}` frame. It arrives after HTTP 200 but is still a failure; reading
/// it as an empty chunk would silently truncate the turn.
fn stream_error(error: &Value) -> CoreError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("provider stream error");
    let truncated: String = message.chars().take(ERROR_BODY_CHAR_LIMIT).collect();
    let lower = truncated.to_ascii_lowercase();
    let rate_limited = lower.contains("rate limit") || lower.contains("too many requests");
    let transient = rate_limited
        || error
            .get("code")
            .and_then(Value::as_u64)
            .is_some_and(|code| code >= 500 || code == 429)
        || [
            "overloaded",
            "temporarily",
            "timeout",
            "timed out",
            "unavailable",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
    if transient {
        CoreError::ProviderTransient {
            message: format!("provider stream error: {truncated}"),
            retry_after_ms: None,
            rate_limited,
        }
    } else {
        CoreError::ProviderUnavailable(format!("provider stream error: {truncated}"))
    }
}

/// `Retry-After` as seconds or an HTTP date (RFC 2822 or 3339), clamped to an hour; past is zero.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            return Some(Duration::from_secs_f64(seconds.min(3600.0)));
        }
        return None;
    }
    let when = chrono::DateTime::parse_from_rfc2822(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
        .ok()?;
    let seconds = (when.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    Some(Duration::from_secs(seconds.clamp(0, 3600) as u64))
}

/// An error body collapsed, redacted and truncated; an HTML page is cut to its `<title>`, and a DNS
/// or offline failure gets the offline hint.
fn sanitize_body(body: &str, api_key: &str) -> String {
    let hint = offline_hint_for_text(body);
    sanitize_body_with_hint(body, api_key, hint)
}

/// [`sanitize_body`] with a caller-supplied hint taken from a transport error chain.
pub(crate) fn sanitize_body_with_hint(body: &str, api_key: &str, hint: Option<&str>) -> String {
    let text = extract_html_title(body)
        .or_else(|| json_error_message(body))
        .unwrap_or_else(|| body.replace(['\n', '\r'], " "));
    let flattened = silver_core::redact::redact(&redact_secret(&text, api_key));
    // Keep the whole result within the body cap, reserving room for a trailing hint.
    let budget = ERROR_BODY_CHAR_LIMIT.saturating_sub(hint.map_or(0, |hint| hint.len() + 1));
    let out: String = flattened.chars().take(budget).collect();
    let mut out = out.trim().to_string();
    if let Some(hint) = hint {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(hint);
    }
    out
}

/// The `error.message` (or a bare `error` string) of a JSON error body.
fn json_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
}

/// Reduce an HTML error page to its `<title>`, if it has one.
fn extract_html_title(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    if !(lower.contains("<!doctype") || lower.contains("<html")) {
        return None;
    }
    let open = lower.find("<title")?;
    let content_start = lower[open..].find('>')? + open + 1;
    let content_end = lower[content_start..].find("</title>")? + content_start;
    let title = body[content_start..content_end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Walk a transport error's source chain looking for a DNS or offline failure.
/// An error with its causes: reqwest's own text ("error decoding response body") hides
/// the one that says what happened ("connection closed before message completed").
pub(crate) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut current = error.source();
    while let Some(cause) = current {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        current = cause.source();
    }
    text
}

pub(crate) fn offline_error_hint(
    error: &(dyn std::error::Error + 'static),
) -> Option<&'static str> {
    let mut current = Some(error);
    while let Some(error) = current {
        if offline_hint_for_text(&error.to_string()).is_some() {
            return Some(OFFLINE_HINT);
        }
        current = error.source();
    }
    None
}

/// Offline hint when the text itself names a DNS or offline failure.
fn offline_hint_for_text(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if NETWORK_RESOLUTION_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        Some(OFFLINE_HINT)
    } else {
        None
    }
}

/// Replace every occurrence of a secret with a stable placeholder.
pub(crate) fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "[redacted]")
    }
}
