//! Anthropic Messages API transport: it translates the request and the SSE stream, nothing more.

use crate::provider::{
    drain_sse_frames, error_chain, map_http_error, normalize_base_url, offline_error_hint,
    parse_retry_after, rate_limit_from_headers, redact_secret, sanitize_body_with_hint,
    store_rate_limit, RateLimitState, StreamingThinkScrubber,
};
use futures::StreamExt;
use serde_json::{json, Value};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{
    FinishReason, Model, ModelMessage, ModelRequest, ModelStream, ModelStreamEvent, ToolSpec,
};
use silver_protocol::{ContentPart, MessageRole, TokenUsage, ToolCallId};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Anthropic API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Beta header that enables explicit prompt-cache breakpoints on the Messages API.
const PROMPT_CACHING_BETA: &str = "prompt-caching-2024-07-31";

/// Output token ceiling used when the request does not set one.
///
/// Matches Hermes' Anthropic transport default (`max_tokens: 16384`).
const DEFAULT_MAX_TOKENS: u32 = 16384;

/// Beta header that enables interleaved thinking blocks when extended thinking is on.
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Output tokens reserved above a thinking budget so the visible reply is not starved.
const THINKING_MAX_TOKENS_HEADROOM: u32 = 4096;

/// Maximum output tokens per model family, via longest substring match.
///
/// Anthropic requires max_tokens and rejects a value above the model's ceiling.
const ANTHROPIC_OUTPUT_LIMITS: &[(&str, u32)] = &[
    ("claude-opus-4", 32_000),
    ("claude-sonnet-4", 64_000),
    ("claude-3-7-sonnet", 128_000),
    ("claude-3-5-sonnet", 8_192),
    ("claude-3-5-haiku", 8_192),
    ("claude-3-opus", 4_096),
    ("claude-3-sonnet", 4_096),
    ("claude-3-haiku", 4_096),
];

/// Streaming client for the Anthropic native Messages API.
#[derive(Clone)]
pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    rate_limit: Arc<Mutex<RateLimitState>>,
    /// Extra request headers a gateway demands (OpenCode's session affinity header, for
    /// example). Empty for a plain endpoint.
    extra_headers: Vec<(String, String)>,
}

impl AnthropicProvider {
    /// Create a provider for an Anthropic native Messages endpoint.
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
        if let Some(state) = rate_limit_from_headers(headers, "anthropic") {
            store_rate_limit(&self.rate_limit, state);
        }
    }

    /// Configured model identifier.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Send these headers with every request, in addition to the API key.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Normalized base URL (never has a trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[async_trait::async_trait]
impl Model for AnthropicProvider {
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
        let body = build_anthropic_body(model, &request);
        let betas = anthropic_betas(&request);
        let url = format!("{}/messages", self.base_url);

        let mut http_request = self
            .http
            .post(&url)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if !betas.is_empty() {
            http_request = http_request.header("anthropic-beta", betas.join(","));
        }
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

        Ok(Box::pin(anthropic_event_stream(byte_stream, cancel)))
    }
}

/// Running usage: input arrives on `message_start`, output on `message_delta`, and cache
/// read/write counts fold into the prompt total.
#[derive(Clone, Copy, Debug, Default)]
struct UsageAccumulator {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
}

impl UsageAccumulator {
    /// Snapshot the accumulator as protocol token accounting.
    fn token_usage(self) -> TokenUsage {
        // Anthropic reports uncached input separately from cache read/write; the protocol
        // prompt total is the sum so the cached prefix is never double-billed downstream.
        let prompt_tokens = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens);
        TokenUsage {
            prompt_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: prompt_tokens.saturating_add(self.output_tokens),
            // Cache creation rides only in the prompt total, so this stays the cache-read
            // bucket that the pricing layer can discount.
            cached_tokens: self.cache_read_tokens,
            reasoning_tokens: self.reasoning_tokens,
        }
    }
}

/// Merge one usage object, overwriting only the fields present, so a `message_delta` carrying
/// output tokens alone keeps the counts from `message_start`.
fn merge_usage_object(usage: &mut UsageAccumulator, value: &Value) {
    if let Some(input) = number(value, "input_tokens") {
        usage.input_tokens = input;
    }
    if let Some(output) = number(value, "output_tokens") {
        usage.output_tokens = output;
    }
    if let Some(read) = number(value, "cache_read_input_tokens") {
        usage.cache_read_tokens = read;
    }
    if let Some(written) = number(value, "cache_creation_input_tokens") {
        usage.cache_write_tokens = written;
    }
    if let Some(reasoning) = number(value, "reasoning_tokens") {
        usage.reasoning_tokens = reasoning;
    }
}

/// Mutable state carried through the SSE decoding stream.
struct AnthropicSseState {
    bytes: Pin<Box<dyn futures::Stream<Item = CoreResult<Vec<u8>>> + Send>>,
    buffer: String,
    cancel: CancellationToken,
    pending: VecDeque<CoreResult<ModelStreamEvent>>,
    done: bool,
    usage: UsageAccumulator,
    scrubber: StreamingThinkScrubber,
}

/// Decode a raw byte stream of 'text/event-stream' data into model events.
pub(crate) fn anthropic_event_stream<S>(
    bytes: S,
    cancel: CancellationToken,
) -> impl futures::Stream<Item = CoreResult<ModelStreamEvent>> + Send + 'static
where
    S: futures::Stream<Item = CoreResult<Vec<u8>>> + Send + 'static,
{
    let state = AnthropicSseState {
        bytes: Box::pin(bytes),
        buffer: String::new(),
        cancel,
        pending: VecDeque::new(),
        done: false,
        usage: UsageAccumulator::default(),
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
                // End of the HTTP body: flush any trailing frame, then release held-back
                // visible prose (or drop an unterminated reasoning span).
                Some(None) => {
                    let frames = drain_sse_frames(&mut state.buffer, true);
                    for frame in frames {
                        push_frame(&mut state, &frame);
                    }
                    flush_scrubber(&mut state);
                    state.done = true;
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

/// Parse a frame and queue its events, marking the stream done on a terminal frame.
fn push_frame(state: &mut AnthropicSseState, frame: &str) {
    match parse_sse_frame(&mut state.usage, &mut state.scrubber, frame) {
        Ok((done, events)) => {
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
fn flush_scrubber(state: &mut AnthropicSseState) {
    let visible = state.scrubber.flush();
    if !visible.is_empty() {
        state
            .pending
            .push_back(Ok(ModelStreamEvent::TextDelta(visible)));
    }
}

/// Parse one SSE frame. The payload's own `type` wins over the `event:` name; `data: [DONE]` is
/// accepted for compatibility.
fn parse_sse_frame(
    usage: &mut UsageAccumulator,
    scrubber: &mut StreamingThinkScrubber,
    frame: &str,
) -> CoreResult<(bool, Vec<ModelStreamEvent>)> {
    let mut event = "";
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.strip_prefix(' ').unwrap_or(rest).trim();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    if data_lines.is_empty() {
        return Ok((false, Vec::new()));
    }

    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        return Ok((true, Vec::new()));
    }
    parse_event(usage, scrubber, event, &data)
}

/// Parse one Anthropic event into a done flag and the events it carried.
fn parse_event(
    usage: &mut UsageAccumulator,
    scrubber: &mut StreamingThinkScrubber,
    event: &str,
    data: &str,
) -> CoreResult<(bool, Vec<ModelStreamEvent>)> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        CoreError::ProviderUnavailable(format!("invalid provider chunk: {error}"))
    })?;

    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .unwrap_or(event);

    let mut events = Vec::new();
    let mut done = false;

    match kind {
        "message_start" => {
            if let Some(usage_value) = value.pointer("/message/usage") {
                merge_usage_object(usage, usage_value);
                events.push(ModelStreamEvent::Usage(usage.token_usage()));
            }
        }
        "content_block_start" => content_block_start(&value, &mut events),
        "content_block_delta" => content_block_delta(&value, scrubber, &mut events),
        "content_block_stop" => {}
        "message_delta" => {
            if let Some(usage_value) = value.get("usage") {
                merge_usage_object(usage, usage_value);
                events.push(ModelStreamEvent::Usage(usage.token_usage()));
            }
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                events.push(ModelStreamEvent::Finish(map_stop_reason(reason)));
            }
        }
        "message_stop" => {
            done = true;
        }
        "ping" => {}
        "error" => return Err(stream_error(&value)),
        _ => {}
    }

    Ok((done, events))
}

/// A `content_block_start` that announces a tool call.
fn content_block_start(value: &Value, events: &mut Vec<ModelStreamEvent>) {
    let index = index_of(value);
    let Some(block) = value.get("content_block") else {
        return;
    };
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return;
    }
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<ToolCallId>().ok());
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    events.push(ModelStreamEvent::ToolCallDelta {
        index,
        id,
        name,
        arguments_delta: String::new(),
    });
}

/// A `content_block_delta`: text, tool arguments or reasoning.
fn content_block_delta(
    value: &Value,
    scrubber: &mut StreamingThinkScrubber,
    events: &mut Vec<ModelStreamEvent>,
) {
    let index = index_of(value);
    let Some(delta) = value.get("delta") else {
        return;
    };
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    // Reasoning tags can straddle delta boundaries, so text passes through the
                    // per-stream scrubber before becoming visible.
                    let visible = scrubber.feed(text);
                    if !visible.is_empty() {
                        events.push(ModelStreamEvent::TextDelta(visible));
                    }
                }
            }
        }
        Some("input_json_delta") => {
            if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                if !partial.is_empty() {
                    events.push(ModelStreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: partial.to_string(),
                    });
                }
            }
        }
        Some("thinking_delta") => {
            if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                if !thinking.is_empty() {
                    events.push(ModelStreamEvent::ReasoningDelta(thinking.to_string()));
                }
            }
        }
        _ => {}
    }
}

/// An in-stream `error` event; Anthropic reports overloads and 5xx after HTTP 200.
fn stream_error(value: &Value) -> CoreError {
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("provider error");
    let error_type = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let transient = matches!(
        error_type,
        "overloaded_error" | "rate_limit_error" | "api_error" | "timeout_error"
    ) || message.to_ascii_lowercase().contains("overloaded");
    let text = format!("provider error ({error_type}): {message}");
    if transient {
        CoreError::ProviderTransient {
            message: text,
            retry_after_ms: None,
            rate_limited: error_type == "rate_limit_error",
        }
    } else {
        CoreError::ProviderUnavailable(text)
    }
}

/// Read an unsigned integer field from a JSON object.
fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// Read the content block index, defaulting to zero.
fn index_of(value: &Value) -> usize {
    value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
}

/// Map a stop reason; an unknown one is kept verbatim.
fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

/// Whether the request has a system prompt or tools to put a cache breakpoint on, the only case
/// where the prompt-caching beta header means anything.
fn uses_prompt_caching(request: &ModelRequest) -> bool {
    !request.tools.is_empty()
        || request.messages.iter().any(|message| {
            message.role == MessageRole::System
                && message.content.iter().any(
                    |part| matches!(part, ContentPart::Text { text } if !text.trim().is_empty()),
                )
        })
}

/// The ephemeral cache breakpoint marker.
fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// The streaming Messages body. With a system block or tools, two of the four allowed cache
/// breakpoints are spent: on the system block (sent as a text block to carry the marker) and on
/// the last tool.
pub(crate) fn build_anthropic_body(model: &str, request: &ModelRequest) -> Value {
    let (system, messages) = map_messages(&request.messages);
    let thinking_budget = thinking_budget_for(request);
    let mut max_tokens = effective_max_tokens(model, request.max_tokens);
    if let Some(budget) = thinking_budget {
        // Thinking tokens count against max_tokens, so keep headroom for the actual reply.
        max_tokens = max_tokens.max(budget.saturating_add(THINKING_MAX_TOKENS_HEADROOM));
    }
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });

    if let Some(system) = system {
        body["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": cache_control(),
        }]);
    }
    if !request.tools.is_empty() {
        let mut tools = map_tools(&request.tools);
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache_control();
        }
        body["tools"] = Value::Array(tools);
    }
    if let Some(budget) = thinking_budget {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    } else if let Some(temperature) = request.temperature {
        // Extended thinking rejects a caller-supplied temperature, so it is omitted when
        // thinking is enabled.
        body["temperature"] = json!(temperature);
    }

    body
}

/// Beta headers for this request; prompt caching and interleaved thinking are sent together.
fn anthropic_betas(request: &ModelRequest) -> Vec<&'static str> {
    let mut betas = Vec::new();
    if uses_prompt_caching(request) {
        betas.push(PROMPT_CACHING_BETA);
    }
    if thinking_budget_for(request).is_some() {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    betas
}

/// Thinking-token budget for the effort label, or None to disable thinking. An unknown label gets
/// the medium budget.
fn thinking_budget_for(request: &ModelRequest) -> Option<u32> {
    let effort = request
        .reasoning_effort
        .as_deref()?
        .trim()
        .to_ascii_lowercase();
    match effort.as_str() {
        "" | "none" => None,
        "minimal" | "low" => Some(1_024),
        "high" => Some(8_192),
        _ => Some(4_096),
    }
}

/// The requested output cap (or DEFAULT_MAX_TOKENS), capped by the model's ceiling when known.
fn effective_max_tokens(model: &str, requested: Option<u32>) -> u32 {
    let base = requested.unwrap_or(DEFAULT_MAX_TOKENS);
    match anthropic_output_limit(model) {
        Some(limit) => base.min(limit),
        None => base,
    }
}

/// The model's output ceiling by longest substring match, dots read as hyphens, so dated ids and
/// `claude-opus-4.6` resolve and `claude-3-5-sonnet` beats `claude-3`.
fn anthropic_output_limit(model: &str) -> Option<u32> {
    let normalized = model.to_ascii_lowercase().replace('.', "-");
    ANTHROPIC_OUTPUT_LIMITS
        .iter()
        .filter(|(key, _)| normalized.contains(key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, limit)| *limit)
}

/// Split core messages into the `system` text and Anthropic messages. Consecutive tool results
/// merge into one user message, and a user turn is inserted when the first message is not one.
fn map_messages(messages: &[ModelMessage]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for message in messages {
        match message.role {
            MessageRole::System => {
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            MessageRole::User => {
                let blocks = user_blocks(&message.content);
                if !blocks.is_empty() {
                    out.push(json!({"role": "user", "content": blocks}));
                }
            }
            MessageRole::Assistant => {
                let blocks = assistant_blocks(&message.content);
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            MessageRole::Tool => {
                let mut results = tool_result_blocks(&message.content);
                // A tool turn carrying a picture becomes the same user message with the image
                // block appended, which is where Anthropic accepts one.
                results.extend(image_blocks(&message.content));
                if results.is_empty() {
                    continue;
                }
                if let Some(last) = out.last_mut() {
                    if is_tool_result_message(last) {
                        if let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut)
                        {
                            blocks.extend(results);
                            continue;
                        }
                    }
                }
                out.push(json!({"role": "user", "content": results}));
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n"))
    };

    ensure_leading_user(&mut out);
    (system, out)
}

/// Anthropic requires the first message to have role 'user'.
fn ensure_leading_user(messages: &mut Vec<Value>) {
    let needs_placeholder = messages
        .first()
        .map(|message| message.get("role").and_then(Value::as_str) != Some("user"))
        .unwrap_or(false);
    if needs_placeholder {
        messages.insert(
            0,
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "(empty message)"}],
            }),
        );
    }
}

/// True when the message is a user turn whose first block is a 'tool_result'.
fn is_tool_result_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str)
            == Some("tool_result")
}

/// Translate user parts into 'text' and 'image' content blocks.
fn user_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } if !text.trim().is_empty() => {
                Some(json!({"type": "text", "text": text}))
            }
            _ => None,
        })
        .chain(image_blocks(parts))
        .collect()
}

/// One 'image' content block per image part.
fn image_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data},
            })),
            _ => None,
        })
        .collect()
}

/// Translate assistant parts into 'text' and 'tool_use' content blocks.
fn assistant_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } if !text.trim().is_empty() => {
                Some(json!({"type": "text", "text": text}))
            }
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "type": "tool_use",
                "id": id.to_string(),
                "name": name,
                "input": arguments,
            })),
            _ => None,
        })
        .collect()
}

/// Translate tool result parts into 'tool_result' content blocks.
fn tool_result_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id.to_string(),
                    "content": content,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                Some(block)
            }
            _ => None,
        })
        .collect()
}

/// Translate tool specs into the Anthropic tool schema.
fn map_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect()
}

/// Join the non-blank text parts of a message with newlines.
fn collect_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(ContentPart::as_text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
