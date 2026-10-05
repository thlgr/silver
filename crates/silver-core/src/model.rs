//! Model transport abstraction. The provider is transport only; the agent loop owns control.

use crate::error::CoreResult;
use silver_protocol::{ContentPart, MessageRole, RunId, TokenUsage, ToolCallId};
use std::path::PathBuf;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: Vec<silver_protocol::ContentPart>,
}

impl ModelMessage {
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![silver_protocol::ContentPart::text(text)],
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::text(MessageRole::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::text(MessageRole::User, text)
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Stable per-conversation key a provider may use to route requests to a warm prompt
    /// cache. Set once from the session id and never changed mid-conversation, because a
    /// changing key would defeat the cached prefix. None disables prompt-cache routing.
    pub cache_key: Option<String>,
    /// Requested reasoning effort, none through ultra; a transport clamps what its wire does not
    /// take onto its own vocabulary. None leaves the
    /// provider default in place, while none explicitly disables reasoning.
    pub reasoning_effort: Option<String>,
    /// The run this request serves, one per user message. None outside a run.
    pub run_id: Option<RunId>,
    /// Root of the run's workspace. None when the run has none.
    pub workspace: Option<PathBuf>,
}

/// What a request says in place of a picture the route refused to carry. It lands in the tool
/// receipt the picture rode and in the run note, so neither the model nor the user has to guess
/// where the picture went.
pub const PICTURE_DROPPED: &str =
    "picture not sent: this route rejects inline images; use a model or provider that takes them";

impl ModelRequest {
    /// Whether any message of this request carries a picture.
    pub fn has_pictures(&self) -> bool {
        self.messages
            .iter()
            .any(|message| message.content.iter().any(is_picture))
    }

    /// Drop every picture, naming why in the receipt it rode, and say whether any were dropped.
    pub fn drop_pictures(&mut self, note: &str) -> bool {
        let mut dropped = false;
        for message in &mut self.messages {
            if !message.content.iter().any(is_picture) {
                continue;
            }
            dropped = true;
            message.content.retain(|part| !is_picture(part));
            match message.content.iter_mut().find_map(|part| match part {
                ContentPart::ToolResult { content, .. } => Some(content),
                _ => None,
            }) {
                Some(content) => {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(note);
                }
                // A message whose only part was the picture still says what happened to it.
                None => message.content.push(ContentPart::text(note)),
            }
        }
        dropped
    }
}

fn is_picture(part: &ContentPart) -> bool {
    matches!(part, ContentPart::Image { .. })
}

#[derive(Clone, Debug, PartialEq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other(String),
}

#[derive(Clone, Debug)]
pub enum ModelStreamEvent {
    /// The model began a new text message; text already streamed is superseded. External
    /// agents report one message per assistant turn, only the last of which is the reply.
    TextStarted,
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<ToolCallId>,
        name: Option<String>,
        arguments_delta: String,
    },
    Usage(TokenUsage),
    /// Provider-reported reason the stream ended (before assembly).
    Finish(FinishReason),
}

#[derive(Clone, Debug, Default)]
pub struct ModelResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCallRequest>,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<TokenUsage>,
    /// True when the completion produced no content, no reasoning and no tool calls.
    pub empty: bool,
    /// True when the provider indicated it actually generated tokens (used by the empty guard).
    pub observed_generation: bool,
    /// True when the turn cut the stream off because the model was still only reasoning when
    /// it passed its reasoning budget.
    pub reasoning_cut: bool,
}

#[derive(Clone, Debug)]
pub struct ToolCallRequest {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
    pub raw_arguments: String,
    /// Set when the provider's argument string was not valid JSON. The loop answers with an
    /// error tool result instead of executing the tool with empty arguments.
    pub malformed_arguments: Option<String>,
}

pub type ModelStream = Pin<Box<dyn futures::Stream<Item = CoreResult<ModelStreamEvent>> + Send>>;

/// The context window of the model a run is about to use, asked per run since sessions and
/// providers change it. Cheap on repeat; None rather than a guess.
#[async_trait::async_trait]
pub trait ContextLengthResolver: Send + Sync {
    /// The context window in tokens, when known.
    async fn context_length(&self, model: &str) -> Option<usize>;

    /// The model's list price, when known. Resolved here because the same catalog that
    /// knows a model's window usually knows its price.
    async fn price(&self, _model: &str) -> Option<crate::pricing::ModelPrice> {
        None
    }

    /// The model a local server actually runs when asked for `model`, if that is another
    /// one: LM Studio answers an id it does not list, such as its `local-model` placeholder,
    /// with the model it has loaded.
    async fn served_model(&self, _model: &str) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Provider/model identifier for logging and capabilities.
    fn name(&self) -> &str;

    /// Whether the endpoint runs on this machine or its LAN. Local servers are slow and some (LM
    /// Studio) emit a whole tool call after producing it in silence, so they get a longer idle
    /// budget.
    fn is_local(&self) -> bool {
        false
    }

    /// Begin a streaming completion. Cancellation must abort promptly.
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream>;
}
