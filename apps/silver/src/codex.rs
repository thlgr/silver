//! ChatGPT / Codex transport: the Responses API at `chatgpt.com/backend-api/codex/responses`,
//! signed in through the `openai-codex` OAuth flow. System messages become `instructions`, the
//! conversation `input` items and tools flat function declarations.

use std::collections::HashMap;

use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use serde_json::{json, Value};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{
    FinishReason, Model, ModelRequest, ModelStream, ModelStreamEvent, ToolSpec,
};
use silver_protocol::{ContentPart, MessageRole, TokenUsage, ToolCallId};
use tokio_util::sync::CancellationToken;

/// Default endpoint for a Codex subscription.
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Originator the Codex backend expects from a CLI client.
const ORIGINATOR: &str = "codex_cli_rs";

/// Streaming client for the Codex Responses backend.
pub struct CodexProvider {
    base_url: String,
    access_token: String,
    model: String,
    account_id: Option<String>,
    http: reqwest::Client,
    /// Whether to send the Codex backend's own identifiers (originator, session, account).
    /// A plain Responses API gateway (OpenCode Zen) wants a bearer and nothing else.
    codex_headers: bool,
    /// Extra request headers a gateway demands.
    extra_headers: Vec<(String, String)>,
}

impl CodexProvider {
    /// Build a transport around a Codex OAuth access token.
    pub fn new(
        base_url: impl Into<String>,
        access_token: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let base_url = if base_url.is_empty() {
            DEFAULT_CODEX_BASE_URL.to_string()
        } else {
            base_url
        };
        let access_token = access_token.into();
        let account_id = chatgpt_account_id(&access_token);
        Self {
            base_url,
            access_token,
            model: model.into(),
            account_id,
            http: reqwest::Client::new(),
            codex_headers: true,
            extra_headers: Vec::new(),
        }
    }

    /// The Responses API on a gateway that is not the Codex backend: a bearer token, no
    /// Codex identifiers, and no account header.
    pub fn responses_api(
        base_url: impl Into<String>,
        bearer: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let mut provider = Self::new(base_url, bearer, model);
        provider.codex_headers = false;
        provider.account_id = None;
        provider
    }

    /// Send these headers with every request.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// The ChatGPT account the token belongs to, when the token carried the claim.
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }
}

/// The `chatgpt_account_id` claim under the token's `https://api.openai.com/auth` namespace. None
/// for a non-JWT or a token without it; personal accounts work without the header.
pub fn chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Build the Responses request body for one turn.
pub fn build_responses_body(model: &str, request: &ModelRequest) -> Value {
    let (instructions, input) = map_input(&request.messages);
    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        // The agent loop owns the conversation, so the backend must not keep its own copy.
        "store": false,
    });
    if let Some(instructions) = instructions {
        body["instructions"] = Value::String(instructions);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(map_tools(&request.tools));
        body["tool_choice"] = Value::String("auto".to_string());
    }
    if let Some(effort) = request.reasoning_effort.as_deref() {
        if !effort.trim().is_empty() && effort != "none" {
            body["reasoning"] = json!({ "effort": effort });
        }
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if let Some(cache_key) = request.cache_key.as_deref().filter(|key| !key.is_empty()) {
        body["prompt_cache_key"] = json!(cache_key);
    }
    body
}

/// Split the conversation into `instructions` and Responses `input` items.
fn map_input(messages: &[silver_core::model::ModelMessage]) -> (Option<String>, Vec<Value>) {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System => {
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            MessageRole::User | MessageRole::Tool => {
                // A tool result arrives as a user- or tool-role message carrying ToolResult
                // parts; Responses has one item type for both. A tool turn keeps only its
                // images, for the user message that follows the outputs.
                let outputs = tool_outputs(&message.content);
                let blocks = if outputs.is_empty() {
                    input_blocks(&message.content)
                } else {
                    image_blocks(&message.content)
                };
                input.extend(outputs);
                for block in blocks {
                    input.push(json!({"type": "message", "role": "user", "content": [block]}));
                }
            }
            MessageRole::Assistant => {
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for part in &message.content {
                    if let ContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                    } = part
                    {
                        // The whole conversation is replayed on every turn with `store: false`,
                        // so a call and its output only have to agree with each other inside
                        // this request; the ids are ours and never the backend's.
                        input.push(json!({
                            "type": "function_call",
                            "name": name,
                            "call_id": id.to_string(),
                            "arguments": arguments.to_string(),
                        }));
                    }
                }
            }
        }
    }
    let instructions = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    (instructions, input)
}

/// The user-side content blocks of a message: its text, then one `input_image` per picture.
fn input_blocks(parts: &[ContentPart]) -> Vec<Value> {
    let text = collect_text(parts);
    (!text.is_empty())
        .then(|| json!({ "type": "input_text", "text": text }))
        .into_iter()
        .chain(image_blocks(parts))
        .collect()
}

/// One `input_image` block per image part.
fn image_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            _ => None,
        })
        .collect()
}

/// Tool results in a message, as Responses `function_call_output` items.
fn tool_outputs(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult {
                tool_call_id,
                content,
                ..
            } => Some(json!({
                "type": "function_call_output",
                "call_id": tool_call_id.to_string(),
                "output": content,
            })),
            _ => None,
        })
        .collect()
}

/// Flatten the tool declarations into the Responses shape.
fn map_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect()
}

/// Concatenate the text parts of a message.
fn collect_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<&str>>()
        .join("")
}

/// Streaming state: which tool call each output item belongs to.
#[derive(Default)]
pub struct ResponsesState {
    /// Output item id → index in the tool-call list the agent assembles.
    items: HashMap<String, usize>,
    /// Next index to hand out.
    next_index: usize,
}

impl ResponsesState {
    /// The stable index for an output item, allocating one on first sight.
    fn index_for(&mut self, item_id: &str) -> usize {
        if let Some(index) = self.items.get(item_id) {
            return *index;
        }
        let index = self.next_index;
        self.next_index += 1;
        self.items.insert(item_id.to_string(), index);
        index
    }
}

/// Translate one Responses SSE event into stream events, plus whether the stream is done.
pub fn map_responses_event(
    state: &mut ResponsesState,
    data: &str,
) -> CoreResult<(bool, Vec<ModelStreamEvent>)> {
    if data.trim() == "[DONE]" {
        return Ok((true, Vec::new()));
    }
    let value: Value = serde_json::from_str(data).map_err(|error| {
        CoreError::ProviderUnavailable(format!("invalid provider chunk: {error}"))
    })?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut events = Vec::new();

    match kind {
        "response.output_text.delta" | "response.text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelStreamEvent::TextDelta(delta.to_string()));
            }
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelStreamEvent::ReasoningDelta(delta.to_string()));
            }
        }
        "response.output_item.added" => {
            if let Some(item) = value.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let item_id = item_id(&value, item);
                    let index = state.index_for(&item_id);
                    events.push(ModelStreamEvent::ToolCallDelta {
                        index,
                        id: call_id(item),
                        name: item.get("name").and_then(Value::as_str).map(str::to_string),
                        arguments_delta: String::new(),
                    });
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let index = state.index_for(&item_id);
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                events.push(ModelStreamEvent::ToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments_delta: delta.to_string(),
                });
            }
        }
        "response.output_item.done" => {
            // The final item carries the authoritative name, id and arguments; emitting them
            // here repairs a call whose deltas were partial.
            if let Some(item) = value.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let item_id = item_id(&value, item);
                    let index = state.index_for(&item_id);
                    events.push(ModelStreamEvent::ToolCallDelta {
                        index,
                        id: call_id(item),
                        name: item.get("name").and_then(Value::as_str).map(str::to_string),
                        arguments_delta: String::new(),
                    });
                }
            }
        }
        "response.completed" | "response.incomplete" => {
            if let Some(usage) = value.pointer("/response/usage") {
                events.push(ModelStreamEvent::Usage(map_usage(usage)));
            }
            events.push(ModelStreamEvent::Finish(finish_reason(&value)));
            return Ok((true, events));
        }
        "response.failed" | "error" => {
            let message = value
                .pointer("/response/error/message")
                .or_else(|| value.pointer("/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("the Codex backend failed the response");
            return Err(CoreError::ProviderUnavailable(format!(
                "codex stream error: {message}"
            )));
        }
        _ => {}
    }
    Ok((false, events))
}

/// The output item id an event refers to.
fn item_id(event: &Value, item: &Value) -> String {
    event
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// The tool call id of a function-call item.
fn call_id(item: &Value) -> Option<ToolCallId> {
    // The backend's own ids (`call_…`) are not silver ids; when one does not parse the agent
    // assigns its own, which is what the next request replays.
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<ToolCallId>().ok())
}

/// Map Responses usage onto the shared token counters.
fn map_usage(usage: &Value) -> TokenUsage {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    TokenUsage {
        prompt_tokens: number("input_tokens"),
        completion_tokens: number("output_tokens"),
        total_tokens: number("total_tokens"),
        cached_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// Map the terminal event onto a finish reason.
fn finish_reason(value: &Value) -> FinishReason {
    match value
        .pointer("/response/incomplete_details/reason")
        .and_then(Value::as_str)
    {
        Some("max_output_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_string()),
        None => {
            let has_tool_call = value
                .pointer("/response/output")
                .and_then(Value::as_array)
                .map(|items| {
                    items.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("function_call")
                    })
                })
                .unwrap_or(false);
            if has_tool_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
    }
}

/// Split an SSE buffer into whole frames, returning the `data:` payload of each.
fn drain_sse_data(buffer: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(position) = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) {
        let boundary = if buffer[position..].starts_with("\r\n\r\n") {
            4
        } else {
            2
        };
        let frame: String = buffer.drain(..position + boundary).collect();
        let mut data_lines: Vec<&str> = Vec::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if !data_lines.is_empty() {
            out.push(data_lines.join("\n"));
        }
    }
    out
}

#[async_trait]
impl Model for CodexProvider {
    fn name(&self) -> &str {
        &self.model
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
        let body = build_responses_body(model, &request);
        let url = format!("{}/responses", self.base_url);
        let http_request = self.build_http_request(&url, &body)?;

        let response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(CoreError::ProviderUnavailable(
                    "provider request cancelled".to_string(),
                ));
            }
            result = http_request.send() => result,
        };
        let response = response.map_err(|error| {
            CoreError::ProviderUnavailable(format!("codex request failed: {error}"))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let detail = silver_core::redact::redact(body.trim());
            return Err(codex_status_error(status, &detail, self.codex_headers));
        }

        let bytes = response.bytes_stream().map(|chunk| {
            chunk.map(|chunk| chunk.to_vec()).map_err(|error| {
                CoreError::ProviderUnavailable(format!("codex stream failed: {error}"))
            })
        });
        let state = CodexStreamState {
            bytes: Box::pin(bytes),
            buffer: String::new(),
            cancel,
            pending: std::collections::VecDeque::new(),
            done: false,
            responses: ResponsesState::default(),
        };
        Ok(Box::pin(futures::stream::unfold(
            state,
            |mut state| async move {
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
                        None => {
                            state.done = true;
                            state.pending.push_back(Err(CoreError::ProviderUnavailable(
                                "provider stream cancelled".to_string(),
                            )));
                        }
                        Some(None) => state.done = true,
                        Some(Some(Err(error))) => {
                            state.done = true;
                            state.pending.push_back(Err(error));
                        }
                        Some(Some(Ok(chunk))) => {
                            state.buffer.push_str(&String::from_utf8_lossy(&chunk));
                            for data in drain_sse_data(&mut state.buffer) {
                                match map_responses_event(&mut state.responses, &data) {
                                    Ok((done, events)) => {
                                        state.pending.extend(events.into_iter().map(Ok));
                                        if done {
                                            state.done = true;
                                            break;
                                        }
                                    }
                                    Err(error) => {
                                        state.done = true;
                                        state.pending.push_back(Err(error));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            },
        )))
    }
}

impl CodexProvider {
    /// Attach the bearer, the Codex headers and the body to a request.
    fn build_http_request(&self, url: &str, body: &Value) -> CoreResult<reqwest::RequestBuilder> {
        if self.access_token.trim().is_empty() {
            return Err(CoreError::ProviderUnavailable(if self.codex_headers {
                "Codex needs a ChatGPT sign-in; run /login openai-codex".to_string()
            } else {
                "the Responses endpoint needs a key; sign in with /login".to_string()
            }));
        }
        let mut http_request = self
            .http
            .post(url)
            .bearer_auth(&self.access_token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if self.codex_headers {
            http_request = http_request
                .header("originator", ORIGINATOR)
                .header(reqwest::header::USER_AGENT, "codex_cli_rs/0.0.0 (silver)")
                .header("session_id", uuid::Uuid::now_v7().to_string());
            if let Some(account) = &self.account_id {
                http_request = http_request.header("chatgpt-account-id", account.as_str());
            }
        }
        for (name, value) in &self.extra_headers {
            http_request = http_request.header(name, value);
        }
        Ok(http_request.json(body))
    }
}

/// Map a non-success Codex response to a provider error.
fn codex_status_error(status: reqwest::StatusCode, detail: &str, codex_headers: bool) -> CoreError {
    match status.as_u16() {
        401 | 403 if codex_headers => CoreError::ProviderUnavailable(format!(
            "codex rejected the sign-in (HTTP {}); run /login openai-codex again: {detail}",
            status.as_u16()
        )),
        401 | 403 => CoreError::ProviderUnavailable(format!(
            "provider authentication failed (HTTP {}): {detail}",
            status.as_u16()
        )),
        429 => CoreError::ProviderRateLimited(format!("codex throttled: {detail}")),
        500..=599 => CoreError::ProviderTransient {
            message: format!("codex returned HTTP {}: {detail}", status.as_u16()),
            retry_after_ms: None,
            rate_limited: false,
        },
        other => CoreError::ProviderUnavailable(format!("codex returned HTTP {other}: {detail}")),
    }
}

/// Streaming state for one Codex response.
struct CodexStreamState {
    bytes: std::pin::Pin<Box<dyn futures::Stream<Item = CoreResult<Vec<u8>>> + Send>>,
    buffer: String,
    cancel: CancellationToken,
    pending: std::collections::VecDeque<CoreResult<ModelStreamEvent>>,
    done: bool,
    responses: ResponsesState,
}
