//! AG-UI (Agent-User Interaction protocol, docs.ag-ui.com) server support: the wire types of the
//! HTTP + SSE binding and the translation between silver's run events and the AG-UI event stream.
//! The handler behind `POST /agent` lives in `api::agui`; the docs are in `docs/agui.md`.

use chrono::Utc;
use serde::Deserialize;
use silver_core::session::Message;
use silver_protocol::{
    ApprovalId, ContentPart, EventPayload, MessageId, MessageRole, RunEvent, TokenUsage,
    ToolCallId, ToolStatus,
};

/// The protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "1.0";

/// The session source of every conversation started through the AG-UI endpoint.
pub const SESSION_SOURCE: &str = "agui";

/// One run request, POSTed to `/agent`. The fields of the input this server does not use
/// (`tools`, `context`, `state`, `forwardedProps`, `resume`) are accepted and ignored: the
/// conversation arrives in `messages`.
#[derive(Debug, Deserialize)]
pub struct RunAgentInput {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: Option<String>,
    pub messages: Vec<AguiMessage>,
    /// The application's own frontend tools, executed by the app rather than silver. Accepted
    /// but never offered to the model, so nothing ever calls one.
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
    /// Ambient information for the run, injected into the run's grounding.
    #[serde(default)]
    pub context: Vec<AguiContext>,
    /// Application-specific values passed through; injected into the grounding like context.
    #[serde(rename = "forwardedProps", default)]
    pub forwarded_props: Option<serde_json::Value>,
    /// The consumer's shared state. Accepted; silver keeps no shared state, so nothing is
    /// merged and no state events are emitted.
    #[serde(default)]
    pub state: Option<serde_json::Value>,
    /// Answers to the interrupts an earlier run on this thread raised (approvals).
    #[serde(default)]
    pub resume: Vec<ResumeEntry>,
    /// Members this version of the protocol does not define, stripped with a warning per the
    /// processing model.
    #[serde(flatten)]
    pub _unknown: serde_json::Map<String, serde_json::Value>,
}

/// One named piece of ambient information the application wants the agent to see.
#[derive(Debug, Deserialize)]
pub struct AguiContext {
    pub description: String,
    pub value: String,
}

/// An answer to one interrupt an earlier run raised, keyed by the interrupt's id.
#[derive(Debug, Deserialize)]
pub struct ResumeEntry {
    #[serde(rename = "interruptId")]
    pub interrupt_id: String,
    pub status: ResumeStatus,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// Whether the interrupt was answered or abandoned.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResumeStatus {
    Resolved,
    Cancelled,
}

/// Render the run's ambient `context` entries and `forwardedProps` into the text block injected
/// into the prompt. None when the client sent neither.
pub fn render_external_context(
    context: &[AguiContext],
    forwarded_props: Option<&serde_json::Value>,
) -> Option<String> {
    let mut out = String::new();
    for entry in context {
        if entry.description.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("{}: {}\n", entry.description, entry.value));
    }
    let forwarded = forwarded_props.filter(|value| !value.is_null());
    let mut body = out.trim().to_string();
    if let Some(value) = forwarded {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!(
            "forwardedProps (opaque to the protocol): {}",
            serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
        ));
    }
    (!body.is_empty()).then_some(body)
}

/// A conversation message, discriminated by `role`. Only the roles that resume a conversation
/// are modeled; `activity` and `reasoning` messages are the consumer's rendering material and
/// are skipped on input.
#[derive(Debug, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum AguiMessage {
    User {
        content: AguiContent,
    },
    Assistant {
        #[serde(default)]
        content: Option<String>,
        #[serde(rename = "toolCalls", default)]
        tool_calls: Vec<AguiToolCall>,
    },
    Tool {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: AguiContent,
        #[serde(default)]
        error: Option<String>,
    },
    System {
        content: String,
    },
    Developer {
        content: String,
    },
    Activity,
    /// The agent's reasoning for its next message; carried over as a `Reasoning` part on that
    /// message, because reasoning providers must have it echoed back. A trailing reasoning
    /// message with no assistant message after it is dropped.
    Reasoning {
        content: String,
    },
}

/// Message body: either plain text or an ordered list of parts.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AguiContent {
    Plain(String),
    Parts(Vec<AguiPart>),
}

/// One part of a message body, discriminated by `type`. Audio, video and document parts are
/// skipped: the model input silver builds carries text and inline images only.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AguiPart {
    Text {
        text: String,
    },
    Image {
        source: AguiSource,
    },
    #[serde(other)]
    Skipped,
}

/// Where a media part's bytes come from. Only inline `data` is usable here; a `url` or `file`
/// part is skipped rather than fetched.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AguiSource {
    Data {
        value: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Url {
        value: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
    },
    File {
        value: String,
        #[serde(default)]
        provider: Option<String>,
    },
}

/// A tool call an assistant message made.
#[derive(Debug, Deserialize)]
pub struct AguiToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    #[serde(rename = "function")]
    function: Option<AguiFunctionCall>,
}

#[derive(Debug, Deserialize)]
pub struct AguiFunctionCall {
    name: String,
    arguments: String,
}

/// Split the run input into the session history and the run prompt: everything before the last
/// user message is seeded into the session, and that last user message becomes the run input.
pub fn split_input(
    mut messages: Vec<AguiMessage>,
    session_id: silver_protocol::SessionId,
) -> Result<(Vec<Message>, Vec<ContentPart>), String> {
    if messages.is_empty() {
        return Err("messages is empty".to_string());
    }
    let Some(AguiMessage::User { content }) = messages.pop() else {
        return Err("the last message must be a user message".to_string());
    };
    let mut seed = Vec::new();
    let mut pending_reasoning: Vec<String> = Vec::new();
    for message in messages {
        match message {
            AguiMessage::Reasoning { content } => pending_reasoning.push(content),
            other => {
                if let Some(mut message) = to_silver_message(other, session_id) {
                    if message.role == MessageRole::Assistant && !pending_reasoning.is_empty() {
                        let reasoning = std::mem::take(&mut pending_reasoning);
                        let reasoning = reasoning.into_iter().map(ContentPart::reasoning);
                        message.content = reasoning.chain(message.content).collect();
                    }
                    seed.push(message);
                }
            }
        }
    }
    Ok((seed, to_content_parts(content)))
}

/// Convert one AG-UI conversation message into a silver message, skipping the roles silver does
/// not resume from. The AG-UI ids are opaque to silver, so each message gets a fresh id.
fn to_silver_message(
    message: AguiMessage,
    session_id: silver_protocol::SessionId,
) -> Option<Message> {
    let (role, content) = match message {
        AguiMessage::User { content } => (MessageRole::User, to_content_parts(content)),
        AguiMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut parts = content
                .map(|text| vec![ContentPart::text(text)])
                .unwrap_or_default();
            parts.extend(tool_calls.into_iter().filter_map(to_tool_call_part));
            (MessageRole::Assistant, parts)
        }
        AguiMessage::Tool {
            tool_call_id,
            content,
            error,
        } => (
            MessageRole::Tool,
            vec![ContentPart::ToolResult {
                tool_call_id: ToolCallId(tool_call_id),
                content: content_plain_text(content),
                is_error: error.is_some(),
            }],
        ),
        AguiMessage::System { content } | AguiMessage::Developer { content } => {
            (MessageRole::System, vec![ContentPart::text(content)])
        }
        AguiMessage::Activity => return None,
        AguiMessage::Reasoning { .. } => return None,
    };
    Some(Message {
        id: MessageId::new(),
        session_id,
        run_id: None,
        role,
        content,
        created_at: Utc::now(),
    })
}

/// An assistant tool call becomes a `ToolCall` content part. The arguments travel as a JSON
/// string per the protocol; parse what parses and keep an empty object otherwise, matching
/// silver's own tool-call representation.
fn to_tool_call_part(call: AguiToolCall) -> Option<ContentPart> {
    let function = call.function?;
    let arguments =
        serde_json::from_str(&function.arguments).unwrap_or_else(|_| serde_json::json!({}));
    let id = if call.id.is_empty() {
        ToolCallId::new()
    } else {
        ToolCallId(call.id)
    };
    Some(ContentPart::ToolCall {
        id,
        name: function.name,
        arguments,
    })
}

/// Map a message body to the content parts silver carries. Text and inline-image parts pass
/// through; every other part is skipped, as the protocol allows a producer to skip what it
/// cannot use.
fn to_content_parts(content: AguiContent) -> Vec<ContentPart> {
    match content {
        AguiContent::Plain(text) => vec![ContentPart::text(text)],
        AguiContent::Parts(parts) => parts
            .into_iter()
            .filter_map(|part| match part {
                AguiPart::Text { text } => Some(ContentPart::text(text)),
                AguiPart::Image {
                    source: AguiSource::Data { value, mime_type },
                } => Some(ContentPart::Image {
                    media_type: mime_type,
                    data: value,
                }),
                _ => None,
            })
            .collect(),
    }
}

/// The readable text of a message body, for tool results.
fn content_plain_text(content: AguiContent) -> String {
    match content {
        AguiContent::Plain(text) => text,
        AguiContent::Parts(parts) => parts
            .into_iter()
            .filter_map(|part| match part {
                AguiPart::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Translates a silver run event stream into AG-UI events. Owns the stream state the protocol
/// requires: whether a text or reasoning message is open, and whether the run has ended.
pub struct Translator {
    thread_id: String,
    run_id: String,
    text_message_id: Option<String>,
    reasoning_message_id: Option<String>,
    ended: bool,
}

impl Translator {
    pub fn new(thread_id: String, run_id: String) -> Self {
        Self {
            thread_id,
            run_id,
            text_message_id: None,
            reasoning_message_id: None,
            ended: false,
        }
    }

    /// The `RUN_STARTED` event that opens the stream. The protocol's identity fields come from
    /// the run input, so this is emitted before any silver event is seen.
    pub fn run_started(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "RUN_STARTED",
            "threadId": self.thread_id,
            "runId": self.run_id,
            "protocolVersion": PROTOCOL_VERSION,
        })
    }

    /// Whether the run's terminal event has been translated; the stream must close.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// Translate one silver event into AG-UI events. Empty once the stream must close.
    pub fn push(&mut self, event: &RunEvent) -> Vec<serde_json::Value> {
        if self.ended {
            return Vec::new();
        }
        match &event.payload {
            EventPayload::TextDelta { delta } => self.text_delta(delta),
            EventPayload::TextCompleted { text } => {
                if self.text_message_id.is_some() {
                    // The text already streamed as deltas; close the message it built.
                    vec![self.close_text()]
                } else {
                    // Replay coalesces deltas, so the completed text is the whole message.
                    let mut out = self.open_text();
                    out.push(self.content_event(text));
                    out.push(self.close_text());
                    out
                }
            }
            EventPayload::ReasoningDelta { delta } => self.reasoning_delta(delta),
            EventPayload::ToolStarted {
                tool_call_id,
                name,
                preview,
            } => {
                let mut out = self.close_open_messages();
                out.extend(tool_call_started(tool_call_id, name, preview));
                out
            }
            EventPayload::ToolCompleted {
                tool_call_id,
                status,
                summary,
            } => {
                // A failed tool says so in its result text, since the result event has no
                // status field of its own.
                vec![serde_json::json!({
                    "type": "TOOL_CALL_RESULT",
                    "messageId": new_message_id(),
                    "toolCallId": tool_call_id,
                    "content": tool_result_text(status, summary),
                })]
            }
            EventPayload::ApprovalRequired {
                approval_id,
                tool_call_id,
                description,
                ..
            } => self.interrupt_approval(approval_id, tool_call_id, description),
            EventPayload::SubagentStarted {
                tool_call_id,
                index,
                agent,
                description,
                ..
            } => subagent_started(tool_call_id, *index, agent, description),
            EventPayload::SubagentStep {
                tool_call_id,
                index,
                event,
            } => attributed_steps(event, &subagent_run_id(tool_call_id, *index)),
            EventPayload::SubagentCompleted {
                tool_call_id,
                index,
                status,
                summary,
                ..
            } => subagent_completed(tool_call_id, *index, status, summary),
            EventPayload::RunCompleted { usage, .. } => self.finish_run(usage.as_ref()),
            EventPayload::RunFailed { code, message } => self.fail_run(code, message),
            EventPayload::RunCancelled { origin } => {
                self.ended = true;
                let mut out = self.close_open_messages();
                out.push(serde_json::json!({
                    "type": "RUN_FINISHED",
                    "threadId": self.thread_id,
                    "runId": self.run_id,
                    "outcome": { "type": "cancelled" },
                }));
                if !origin.is_empty() {
                    out.push(serde_json::json!({
                        "type": "CUSTOM",
                        "customType": "silver.cancelled.origin",
                        "payload": origin,
                    }));
                }
                out
            }
            // run.queued, context.*, run.waiting, steer.*, memory.*, advisor.*,
            // context.injected, plan_mode.exited, heartbeat, replay.gap:
            // not AG-UI events.
            _ => Vec::new(),
        }
    }

    /// Close the stream for a run waiting on outside input: `RUN_FINISHED` with the interrupt
    /// outcome, answered on a later run's `resume` list.
    fn interrupt_approval(
        &mut self,
        approval_id: &ApprovalId,
        tool_call_id: &ToolCallId,
        description: &str,
    ) -> Vec<serde_json::Value> {
        self.ended = true;
        self.close_open_messages();
        vec![serde_json::json!({
            "type": "RUN_FINISHED",
            "threadId": self.thread_id,
            "runId": self.run_id,
            "outcome": {
                "type": "interrupt",
                "interrupts": [{
                    "id": approval_id,
                    "reason": "approval",
                    "message": description,
                    "toolCallId": tool_call_id,
                }],
            },
        })]
    }

    /// Close a run that did not fail: close open messages and emit `RUN_FINISHED` with usage.
    fn finish_run(&mut self, usage: Option<&TokenUsage>) -> Vec<serde_json::Value> {
        self.ended = true;
        let mut out = self.close_open_messages();
        let finished = match usage {
            Some(usage) => self.run_finished(usage),
            None => self.run_finished_plain(),
        };
        out.push(finished);
        out
    }

    /// Close a failed run with `RUN_ERROR`, carrying the silver error code.
    fn fail_run(
        &mut self,
        code: &silver_protocol::ErrorCode,
        message: &str,
    ) -> Vec<serde_json::Value> {
        self.ended = true;
        let mut out = self.close_open_messages();
        out.push(serde_json::json!({
            "type": "RUN_ERROR",
            "message": message,
            "code": code.as_str(),
        }));
        out
    }

    fn text_delta(&mut self, delta: &str) -> Vec<serde_json::Value> {
        let mut out = self.close_reasoning();
        if self.text_message_id.is_none() {
            out.extend(self.open_text());
            out.push(self.content_event(delta));
            out
        } else {
            out.push(self.content_event(delta));
            out
        }
    }

    fn reasoning_delta(&mut self, delta: &str) -> Vec<serde_json::Value> {
        let Some(id) = self.reasoning_message_id.as_ref() else {
            let id = new_message_id();
            self.reasoning_message_id = Some(String::clone(&id));
            return vec![
                serde_json::json!({
                    "type": "REASONING_MESSAGE_START",
                    "messageId": id,
                    "role": "reasoning",
                }),
                serde_json::json!({
                    "type": "REASONING_MESSAGE_CONTENT",
                    "messageId": id,
                    "delta": delta,
                }),
            ];
        };
        vec![serde_json::json!({
            "type": "REASONING_MESSAGE_CONTENT",
            "messageId": id,
            "delta": delta,
        })]
    }

    /// Open a text message and return the events that close the messages before it.
    fn open_text(&mut self) -> Vec<serde_json::Value> {
        let mut out = self.close_open_messages();
        let id = new_message_id();
        self.text_message_id = Some(String::clone(&id));
        out.push(serde_json::json!({
            "type": "TEXT_MESSAGE_START",
            "messageId": id,
        }));
        // role defaults to assistant, which is the only role silver emits.
        out
    }

    fn content_event(&self, delta: &str) -> serde_json::Value {
        let id = self.text_message_id.as_ref().expect("text message open");
        serde_json::json!({
            "type": "TEXT_MESSAGE_CONTENT",
            "messageId": id,
            "delta": delta,
        })
    }

    fn close_text(&mut self) -> serde_json::Value {
        let id = self.text_message_id.take().expect("text message open");
        serde_json::json!({
            "type": "TEXT_MESSAGE_END",
            "messageId": id,
        })
    }

    fn close_reasoning(&mut self) -> Vec<serde_json::Value> {
        let Some(id) = self.reasoning_message_id.take() else {
            return Vec::new();
        };
        vec![serde_json::json!({
            "type": "REASONING_MESSAGE_END",
            "messageId": id,
        })]
    }

    fn close_open_messages(&mut self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        if let Some(id) = self.reasoning_message_id.take() {
            out.push(serde_json::json!({
                "type": "REASONING_MESSAGE_END",
                "messageId": id,
            }));
        }
        if let Some(id) = self.text_message_id.take() {
            out.push(serde_json::json!({
                "type": "TEXT_MESSAGE_END",
                "messageId": id,
            }));
        }
        out
    }

    fn run_finished(&self, usage: &TokenUsage) -> serde_json::Value {
        let mut value = self.run_finished_plain();
        value["usage"] = serde_json::json!([{
            "inputTokens": usage.prompt_tokens,
            "outputTokens": usage.completion_tokens,
            "totalTokens": usage.total_tokens,
            "reasoningTokens": usage.reasoning_tokens,
            "cachedInputTokens": usage.cached_tokens,
        }]);
        value
    }

    fn run_finished_plain(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "RUN_FINISHED",
            "threadId": self.thread_id,
            "runId": self.run_id,
        })
    }
}

/// A fresh opaque message id for a text, reasoning or tool-result message the producer mints.
fn new_message_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The opaque id of one subagent invocation, minted from the `delegate_task` batch entry.
fn subagent_run_id(tool_call_id: &ToolCallId, index: u32) -> String {
    format!("{tool_call_id}:{index}")
}

/// The `SUBAGENT_STARTED` event for one invocation.
fn subagent_started(
    tool_call_id: &ToolCallId,
    index: u32,
    agent: &str,
    description: &str,
) -> Vec<serde_json::Value> {
    let mut out = vec![serde_json::json!({
        "type": "SUBAGENT_STARTED",
        "subagentRunId": subagent_run_id(tool_call_id, index),
        "name": agent,
    })];
    if !description.is_empty() {
        out[0]["description"] = serde_json::json!(description);
    }
    out
}

/// The event closing one invocation: `SUBAGENT_FINISHED` on success, `SUBAGENT_ERROR` with a
/// status-derived code when it failed or was refused.
fn subagent_completed(
    tool_call_id: &ToolCallId,
    index: u32,
    status: &ToolStatus,
    summary: &str,
) -> Vec<serde_json::Value> {
    let subagent_run_id = subagent_run_id(tool_call_id, index);
    match status {
        silver_protocol::ToolStatus::Completed => vec![serde_json::json!({
            "type": "SUBAGENT_FINISHED",
            "subagentRunId": subagent_run_id,
            "result": summary,
        })],
        other => vec![serde_json::json!({
            "type": "SUBAGENT_ERROR",
            "subagentRunId": subagent_run_id,
            "message": summary,
            "code": format!("{other:?}").to_lowercase(),
        })],
    }
}

/// The `TOOL_CALL_START` / `TOOL_CALL_ARGS` / `TOOL_CALL_END` stream for a started tool call.
/// The args are the sanitized preview silver shows in its own UI.
fn tool_call_started(
    tool_call_id: &ToolCallId,
    name: &str,
    preview: &serde_json::Value,
) -> Vec<serde_json::Value> {
    let preview = serde_json::to_string(preview).unwrap_or_default();
    vec![
        serde_json::json!({
            "type": "TOOL_CALL_START",
            "toolCallId": tool_call_id,
            "toolCallName": name,
        }),
        serde_json::json!({
            "type": "TOOL_CALL_ARGS",
            "toolCallId": tool_call_id,
            "delta": preview,
        }),
        serde_json::json!({
            "type": "TOOL_CALL_END",
            "toolCallId": tool_call_id,
        }),
    ]
}

/// Translate one of the subagent's own events, which silver wraps as `SUBAGENT_STEP`. Only its
/// tool activity survives the wrapping; the output is attributed to the subagent's invocation
/// and never touches the run's own open text or reasoning messages.
fn attributed_steps(event: &EventPayload, subagent_run_id: &str) -> Vec<serde_json::Value> {
    let mut out = match event {
        EventPayload::ToolStarted {
            tool_call_id,
            name,
            preview,
            ..
        } => tool_call_started(tool_call_id, name, preview),
        EventPayload::ToolCompleted {
            tool_call_id,
            status,
            summary,
            ..
        } => vec![serde_json::json!({
            "type": "TOOL_CALL_RESULT",
            "messageId": new_message_id(),
            "toolCallId": tool_call_id,
            "content": tool_result_text(status, summary),
        })],
        _ => Vec::new(),
    };
    for value in &mut out {
        value["subagentRunId"] = serde_json::json!(subagent_run_id);
    }
    out
}

/// The result text of a completed tool: the summary as-is, with the status appended when it did
/// not succeed, since the protocol's result event has no status field of its own.
fn tool_result_text<'a>(status: &ToolStatus, summary: &'a str) -> std::borrow::Cow<'a, str> {
    match status {
        silver_protocol::ToolStatus::Failed
        | silver_protocol::ToolStatus::Denied
        | silver_protocol::ToolStatus::Blocked => {
            std::borrow::Cow::Owned(format!("{summary} [{status:?}]"))
        }
        _ => std::borrow::Cow::Borrowed(summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silver_protocol::{EventId, RunId};

    fn input(json: &str) -> RunAgentInput {
        serde_json::from_str(json).unwrap()
    }

    fn silver_event(payload: EventPayload) -> RunEvent {
        RunEvent {
            run_id: RunId::new(),
            event_id: EventId(1),
            created_at: Utc::now(),
            payload,
        }
    }

    fn types_of(seed: &[Message]) -> Vec<MessageRole> {
        seed.iter().map(|m| m.role).collect()
    }

    #[test]
    fn split_input_seeds_history_and_input() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-1",
            "messages": [
                {"role": "system", "id": "s1", "content": "be terse"},
                {"role": "user", "id": "u1", "content": "hello"},
                {"role": "assistant", "id": "a1", "content": "hi",
                 "toolCalls": [{"id": "call_1", "type": "function",
                     "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}]},
                {"role": "tool", "id": "t1", "toolCallId": "call_1",
                 "content": "file contents"},
                {"role": "user", "id": "u2", "content": "next"}
            ]
        }"#,
        );
        let session_id = silver_protocol::SessionId::new();
        let (seed, prompt) = split_input(input.messages, session_id).unwrap();
        assert_eq!(
            types_of(&seed),
            vec![
                MessageRole::System,
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::Tool,
            ]
        );
        assert_eq!(seed[0].content, vec![ContentPart::text("be terse")]);
        let ContentPart::ToolCall { id, name, .. } = &seed[2].content[1] else {
            panic!("expected tool call part");
        };
        assert_eq!(id.as_str(), "call_1");
        assert_eq!(name, "read_file");
        let ContentPart::ToolResult {
            tool_call_id,
            content,
            ..
        } = &seed[3].content[0]
        else {
            panic!("expected tool result part");
        };
        assert_eq!(tool_call_id.as_str(), "call_1");
        assert_eq!(content, "file contents");
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0].as_text(), Some("next"));
    }

    #[test]
    fn split_input_skips_activity_and_drops_trailing_reasoning() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-1",
            "messages": [
                {"role": "activity", "id": "act1", "activity": "running", "content": "thinking"},
                {"role": "reasoning", "id": "r1", "content": "hmm", "metadata": {"k": 1}},
                {"role": "user", "id": "u1", "content": "go"}
            ]
        }"#,
        );
        let session_id = silver_protocol::SessionId::new();
        let (seed, _) = split_input(input.messages, session_id).unwrap();
        assert!(seed.is_empty());
    }

    #[test]
    fn split_input_carries_reasoning_onto_the_next_assistant_message() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-1",
            "messages": [
                {"role": "reasoning", "id": "r1", "content": "think first"},
                {"role": "assistant", "id": "a1", "content": "answered"},
                {"role": "user", "id": "u2", "content": "next"}
            ]
        }"#,
        );
        let session_id = silver_protocol::SessionId::new();
        let (seed, _) = split_input(input.messages, session_id).unwrap();
        assert_eq!(seed.len(), 1);
        assert_eq!(seed[0].role, MessageRole::Assistant);
        assert_eq!(
            seed[0].content,
            vec![
                ContentPart::reasoning("think first"),
                ContentPart::text("answered")
            ]
        );
    }

    #[test]
    fn split_input_maps_inline_image_and_skips_the_rest() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-1",
            "messages": [
                {"role": "user", "id": "u1", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "source": {"type": "data", "value": "AAAA", "mimeType": "image/png"}},
                    {"type": "image", "source": {"type": "url", "value": "https://x/y.png", "mimeType": "image/png"}},
                    {"type": "document", "name": "d.pdf", "source": {"type": "url", "value": "https://x/d.pdf"}}
                ]},
                {"role": "user", "id": "u2", "content": "that one"}
            ]
        }"#,
        );
        let session_id = silver_protocol::SessionId::new();
        let (seed, _) = split_input(input.messages, session_id).unwrap();
        assert_eq!(seed.len(), 1);
        assert_eq!(seed[0].content.len(), 2);
        let ContentPart::Image { media_type, data } = &seed[0].content[1] else {
            panic!("expected image part");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(data, "AAAA");
    }

    #[test]
    fn split_input_rejects_missing_or_non_user_last_message() {
        let empty = input(r#"{"threadId": "t", "runId": "r", "messages": []}"#);
        let session_id = silver_protocol::SessionId::new();
        assert!(split_input(empty.messages, session_id)
            .unwrap_err()
            .contains("empty"));

        let assistant_only = input(
            r#"{
            "threadId": "t", "runId": "r",
            "messages": [{"role": "assistant", "id": "a", "content": "hi"}]
        }"#,
        );
        assert!(split_input(assistant_only.messages, session_id)
            .unwrap_err()
            .contains("last message"));
    }

    #[test]
    fn run_input_tolerates_unknown_members() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-1",
            "protocolVersion": "1.0",
            "messages": [{"role": "user", "id": "u1", "content": "hi"}],
            "tools": [{"name": "weather", "description": "the weather",
                       "parameters": {"type": "object"}}],
            "context": [{"description": "today", "value": "sunny"}],
            "state": {"count": 1},
            "forwardedProps": {"a": true}
        }"#,
        );
        assert_eq!(input.thread_id, "thr-1");
        assert_eq!(input.messages.len(), 1);
    }

    #[test]
    fn translator_streams_text_reasoning_tools_and_finish() {
        let mut t = Translator::new("thr-1".into(), "run-1".into());
        let mut out = vec![t.run_started()];
        out.extend(t.push(&silver_event(EventPayload::ReasoningDelta {
            delta: "think".into(),
        })));
        out.extend(t.push(&silver_event(EventPayload::TextDelta {
            delta: "Hell".into(),
        })));
        out.extend(t.push(&silver_event(EventPayload::TextDelta { delta: "o".into() })));
        out.extend(t.push(&silver_event(EventPayload::TextCompleted {
            text: "Hello".into(),
        })));
        out.extend(t.push(&silver_event(EventPayload::ToolStarted {
            tool_call_id: ToolCallId("call_1".into()),
            name: "read_file".into(),
            preview: serde_json::json!({"path": "a.txt"}),
        })));
        out.extend(t.push(&silver_event(EventPayload::ToolCompleted {
            tool_call_id: ToolCallId("call_1".into()),
            status: silver_protocol::ToolStatus::Completed,
            summary: "file contents".into(),
        })));
        out.extend(t.push(&silver_event(EventPayload::RunCompleted {
            usage: Some(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 2,
                reasoning_tokens: 3,
            }),
            cost_usd: None,
            duration_ms: 100,
        })));

        let types: Vec<&str> = out.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "RUN_STARTED",
                "REASONING_MESSAGE_START",
                "REASONING_MESSAGE_CONTENT",
                "REASONING_MESSAGE_END",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "TOOL_CALL_START",
                "TOOL_CALL_ARGS",
                "TOOL_CALL_END",
                "TOOL_CALL_RESULT",
                "RUN_FINISHED",
            ]
        );
        // The reasoning message ends when the text message opens; the text message the deltas
        // built is the one that closes when the tool call starts.
        let text_id = out[4]["messageId"].clone();
        assert_eq!(out[5]["messageId"], out[6]["messageId"]);
        assert_eq!(text_id, out[7]["messageId"]);
        assert_eq!(out[3]["type"], "REASONING_MESSAGE_END");
        // Token usage lands on RUN_FINISHED.
        assert_eq!(out[12]["usage"][0]["inputTokens"], 10);
        assert_eq!(out[12]["usage"][0]["cachedInputTokens"], 2);
        // Nothing more after the terminal event.
        assert!(t
            .push(&silver_event(EventPayload::TextDelta {
                delta: "late".into(),
            }))
            .is_empty());
    }

    #[test]
    fn translator_backfills_completed_text_on_replay() {
        let mut t = Translator::new("thr-1".into(), "run-1".into());
        let out = t.push(&silver_event(EventPayload::TextCompleted {
            text: "whole message".into(),
        }));
        let types: Vec<&str> = out.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END"
            ]
        );
        assert_eq!(out[1]["delta"], "whole message");
    }

    #[test]
    fn translator_maps_failure_cancellation_and_approval() {
        let mut t = Translator::new("thr-1".into(), "run-1".into());
        let out = t.push(&silver_event(EventPayload::RunFailed {
            code: silver_protocol::ErrorCode::ProviderUnavailable,
            message: "provider down".into(),
        }));
        assert_eq!(out[0]["type"], "RUN_ERROR");
        assert_eq!(out[0]["code"], "provider_unavailable");
        assert!(t.ended);

        let mut t = Translator::new("thr-1".into(), "run-1".into());
        let out = t.push(&silver_event(EventPayload::RunCancelled {
            origin: "user".into(),
        }));
        assert_eq!(out[0]["type"], "RUN_FINISHED");
        assert_eq!(out[0]["outcome"]["type"], "cancelled");

        let mut t = Translator::new("thr-1".into(), "run-1".into());
        let approval_id = silver_protocol::ApprovalId::new();
        let out = t.push(&silver_event(EventPayload::ApprovalRequired {
            approval_id,
            tool_call_id: ToolCallId("call_1".into()),
            name: "write_file".into(),
            risk: silver_protocol::RiskLevel::Write,
            description: "overwrite a.txt".into(),
            arguments_preview: serde_json::json!({}),
        }));
        // An approval is an interrupt, not an error: the run ends asking, and a later run's
        // `resume` list answers it.
        assert_eq!(out[0]["type"], "RUN_FINISHED");
        assert_eq!(out[0]["outcome"]["type"], "interrupt");
        let interrupt = &out[0]["outcome"]["interrupts"][0];
        assert_eq!(interrupt["id"], approval_id.to_string());
        assert_eq!(interrupt["reason"], "approval");
        assert_eq!(interrupt["toolCallId"], "call_1");
        assert_eq!(interrupt["message"], "overwrite a.txt");
        assert!(t.ended);
    }

    #[test]
    fn translator_maps_subagent_lifecycle_and_attributed_steps() {
        let mut t = Translator::new("thr-1".into(), "run-1".into());
        let mut out = Vec::new();
        out.extend(t.push(&silver_event(EventPayload::SubagentStarted {
            tool_call_id: ToolCallId("call_del".into()),
            index: 0,
            agent: "general-purpose".into(),
            description: "explore the code".into(),
            model: "mock-model".into(),
            worktree: None,
        })));
        out.extend(t.push(&silver_event(EventPayload::SubagentStep {
            tool_call_id: ToolCallId("call_del".into()),
            index: 0,
            event: Box::new(EventPayload::ToolStarted {
                tool_call_id: ToolCallId("call_inner".into()),
                name: "read_file".into(),
                preview: serde_json::json!({"path": "x.rs"}),
            }),
        })));
        out.extend(t.push(&silver_event(EventPayload::SubagentStep {
            tool_call_id: ToolCallId("call_del".into()),
            index: 0,
            event: Box::new(EventPayload::ToolCompleted {
                tool_call_id: ToolCallId("call_inner".into()),
                status: silver_protocol::ToolStatus::Completed,
                summary: "fn main".into(),
            }),
        })));
        out.extend(t.push(&silver_event(EventPayload::SubagentCompleted {
            tool_call_id: ToolCallId("call_del".into()),
            index: 0,
            status: silver_protocol::ToolStatus::Completed,
            summary: "done".into(),
            tool_uses: 1,
            duration_ms: 12,
            worktree: None,
        })));
        assert_eq!(out[0]["type"], "SUBAGENT_STARTED");
        assert_eq!(out[0]["subagentRunId"], "call_del:0");
        assert_eq!(out[0]["name"], "general-purpose");
        assert_eq!(out[1]["type"], "TOOL_CALL_START");
        assert_eq!(out[1]["subagentRunId"], "call_del:0");
        assert_eq!(out[1]["toolCallId"], "call_inner");
        assert_eq!(out[4]["type"], "TOOL_CALL_RESULT");
        assert_eq!(out[4]["subagentRunId"], "call_del:0");
        assert_eq!(out[5]["type"], "SUBAGENT_FINISHED");
        assert_eq!(out[5]["result"], "done");
        // The subagent's lifecycle does not close the run.
        assert!(!t.ended);
    }

    #[test]
    fn resume_input_parses_and_context_renders() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-2",
            "messages": [{"role": "user", "id": "u1", "content": "hi"}],
            "resume": [
                {"interruptId": "apr_1", "status": "resolved", "payload": "go ahead"},
                {"interruptId": "apr_2", "status": "cancelled"}
            ],
            "context": [{"description": "the user's timezone", "value": "Europe/Lisbon"}],
            "forwardedProps": {"theme": "dark"},
            "tools": [{"name": "weather", "description": "the weather"}],
            "state": {"count": 1}
        }"#,
        );
        assert_eq!(input.resume.len(), 2);
        assert!(matches!(input.resume[0].status, ResumeStatus::Resolved));
        assert!(matches!(input.resume[1].status, ResumeStatus::Cancelled));
        assert!(input._unknown.is_empty());
        assert_eq!(input.messages.len(), 1);

        let rendered =
            render_external_context(&input.context, input.forwarded_props.as_ref()).unwrap();
        assert!(rendered.contains("the user's timezone: Europe/Lisbon"));
        assert!(rendered.contains("theme"));

        let empty = render_external_context(&[], None);
        assert!(empty.is_none());
    }

    #[test]
    fn run_input_strips_unknown_members_into_the_flatten() {
        let input = input(
            r#"{
            "threadId": "thr-1",
            "runId": "run-1",
            "messages": [{"role": "user", "content": "hi"}],
            "somethingFuture": {"a": 1}
        }"#,
        );
        assert!(input._unknown.contains_key("somethingFuture"));
    }
}
