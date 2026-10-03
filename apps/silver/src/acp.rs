//! ACP: an external agent (`copilot --acp --stdio`, `opencode acp`) driven over stdio as a model.
//! It runs its own loop and tools; only its prose returns. Sessions are keyed by
//! [ModelRequest::cache_key], so each turn sends only the newest message.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, PoisonError};

use async_trait::async_trait;
use serde_json::{json, Value};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{FinishReason, Model, ModelRequest, ModelStream, ModelStreamEvent};
use silver_protocol::{ContentPart, MessageRole};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

/// ACP protocol version this client speaks.
const PROTOCOL_VERSION: u32 = 1;

/// Default command when the configuration names none.
const DEFAULT_COMMAND: &str = "copilot";

/// Default arguments that put the CLI in ACP stdio mode.
const DEFAULT_ARGS: [&str; 2] = ["--acp", "--stdio"];

/// One streamed update from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpUpdate {
    /// Prose for the user.
    Message(String),
    /// The agent's own thinking.
    Thought(String),
    /// Tool activity inside the external agent, surfaced so the turn is not silent.
    ToolActivity(String),
    /// The prompt finished with this stop reason.
    Finished(String),
}

/// Translate a `session/update` notification into an update, if it carries one.
pub fn map_session_update(params: &Value) -> Option<AcpUpdate> {
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    match kind {
        "agent_message_chunk" => text_of(update).map(AcpUpdate::Message),
        "agent_thought_chunk" => text_of(update).map(AcpUpdate::Thought),
        "tool_call" | "tool_call_update" => {
            let title = update
                .get("title")
                .or_else(|| update.get("rawInput"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    update
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "tool".to_string());
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            Some(AcpUpdate::ToolActivity(format!("[{status}] {title}")))
        }
        _ => None,
    }
}

/// The text inside an update's `content` block.
fn text_of(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    // A content array: concatenate its text parts.
    let parts = content.as_array()?;
    let text: String = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then_some(text)
}

/// In-flight requests, keyed by their JSON-RPC id.
type PendingCalls = std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// The JSON-RPC plumbing of one agent process.
struct Connection {
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: PendingCalls,
    updates: std::sync::Mutex<Option<mpsc::UnboundedSender<AcpUpdate>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Connection {
    /// Wire a reader and writer into a connection, spawning the read loop.
    fn new<R, W>(reader: R, writer: W) -> Arc<Self>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let connection = Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            pending: std::sync::Mutex::new(HashMap::new()),
            updates: std::sync::Mutex::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
        });
        let conn = Arc::clone(&connection);
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                // A frame carrying a method is something the agent is telling us or asking us. A
                // request — method and id together — has to be answered, or the agent waits
                // forever for a client that never replies.
                if let Some(method) = value.get("method").and_then(Value::as_str) {
                    if value.get("id").is_some() {
                        if let Some(update) = conn.answer(&value).await {
                            conn.emit(update);
                        }
                    } else if method == "session/update" {
                        if let Some(update) = value.get("params").and_then(map_session_update) {
                            conn.emit(update);
                        }
                    }
                    continue;
                }
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    let sender = conn.pending.lock().ok().and_then(|mut map| map.remove(&id));
                    if let Some(sender) = sender {
                        let outcome = match value.get("error") {
                            Some(error) => Err(error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("agent error")
                                .to_string()),
                            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        drop(sender.send(outcome));
                    }
                    continue;
                }
            }
            // The process ended: fail every in-flight call rather than hang.
            if let Ok(mut map) = conn.pending.lock() {
                for (_, sender) in map.drain() {
                    drop(sender.send(Err("the ACP agent exited".to_string())));
                }
            }
        });
        connection
    }

    /// Hand one update to the stream currently routed to this connection, if any.
    fn emit(&self, update: AcpUpdate) {
        let sender = self
            .updates
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().cloned());
        if let Some(sender) = sender {
            drop(sender.send(update));
        }
    }

    /// Write one frame to the agent.
    async fn send(&self, frame: Value) {
        let Ok(mut line) = serde_json::to_vec(&frame) else {
            return;
        };
        line.push(b'\n');
        let mut writer = self.writer.lock().await;
        drop(writer.write_all(&line).await);
        drop(writer.flush().await);
    }

    /// Answer a request the agent addressed to us. `session/request_permission` must be answered or
    /// the turn hangs; silver refuses it, since the agent stays in the directory it started in,
    /// and reports the refusal as reasoning.
    async fn answer(&self, request: &Value) -> Option<AcpUpdate> {
        let id = request.get("id")?;
        let method = request.get("method")?.as_str()?;
        if method != "session/request_permission" {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("{method} is not supported by this client"),
                },
            }))
            .await;
            return None;
        }
        let params = request.get("params");
        let title = params
            .and_then(|params| params.get("toolCall"))
            .and_then(|tool| tool.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("an action outside its working directory");
        // Prefer the agent's own reject option; without one, `cancelled` is the protocol's way of
        // refusing that names no option.
        let outcome = params
            .and_then(|params| params.get("options"))
            .and_then(Value::as_array)
            .and_then(|options| {
                options
                    .iter()
                    .find(|option| {
                        matches!(
                            option.get("kind").and_then(Value::as_str),
                            Some("reject_once") | Some("reject_always")
                        )
                    })
                    .and_then(|option| option.get("optionId"))
                    .and_then(Value::as_str)
            })
            .map(|option| json!({ "outcome": { "outcome": "selected", "optionId": option } }))
            .unwrap_or_else(|| json!({ "outcome": { "outcome": "cancelled" } }));
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": outcome }))
            .await;
        Some(AcpUpdate::ToolActivity(format!("[refused] {title}")))
    }

    /// Send a request and wait for its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut writer = self.writer.lock().await;
            let mut line = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
            line.push(b'\n');
            writer
                .write_all(&line)
                .await
                .map_err(|error| error.to_string())?;
            writer.flush().await.map_err(|error| error.to_string())?;
        }
        receiver
            .await
            .map_err(|_closed| "the ACP agent closed the connection".to_string())?
    }

    /// Route updates to this channel until it is replaced.
    fn route_updates(&self, sender: mpsc::UnboundedSender<AcpUpdate>) {
        if let Ok(mut slot) = self.updates.lock() {
            *slot = Some(sender);
        }
    }
}

/// An external ACP agent, presented as a model.
pub struct AcpProvider {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    model: String,
    connection: Mutex<Option<Arc<Connection>>>,
    sessions: Mutex<HashMap<String, String>>,
}

/// The `provider/model` value for the agent's `model` config option; a bare label such as the
/// default `copilot` names none, so the agent keeps its own default.
fn model_option_value(name: &str) -> Option<&str> {
    let name = name.trim();
    let (provider, model) = name.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some(name)
}

impl AcpProvider {
    /// Build a provider that spawns `command args…`.
    ///
    /// An empty command uses `copilot --acp --stdio`; `COPILOT_CLI_PATH` overrides the binary.
    pub fn new(command: impl Into<String>, model: impl Into<String>) -> Self {
        let raw = command.into();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let command = parts
            .next()
            .filter(|command| !command.is_empty())
            .or_else(|| std::env::var("COPILOT_CLI_PATH").ok())
            .unwrap_or_else(|| DEFAULT_COMMAND.to_string());
        let args: Vec<String> = {
            let rest: Vec<String> = parts.collect();
            if rest.is_empty() {
                DEFAULT_ARGS.iter().map(|arg| arg.to_string()).collect()
            } else {
                rest
            }
        };
        Self {
            command,
            args,
            cwd: None,
            model: model.into(),
            connection: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Run the agent in this directory, which is the workspace it can see.
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        self.cwd = (!cwd.trim().is_empty()).then_some(cwd);
        self
    }

    /// The spawned connection, starting the agent on first use.
    async fn connection(&self) -> Result<Arc<Connection>, CoreError> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref() {
            return Ok(Arc::clone(connection));
        }
        let mut command = tokio::process::Command::new(&self.command);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|error| {
            CoreError::ProviderUnavailable(format!(
                "could not start ACP agent '{}': {error}; check the command is on PATH",
                self.command
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Internal("ACP agent has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Internal("ACP agent has no stdout".to_string()))?;
        let connection = Connection::new(stdout, stdin);
        connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
                    "clientInfo": { "name": "silver", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await
            .map_err(|error| {
                CoreError::ProviderUnavailable(format!("ACP initialize failed: {error}"))
            })?;
        // The child is owned by the task that reaps it; dropping the handle here would kill it.
        tokio::spawn(async move {
            drop(child.wait().await);
        });
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }

    /// The ACP session for a conversation and whether this call opened it: a fresh session needs
    /// the whole conversation, an existing one only the newest turn.
    async fn session(
        &self,
        connection: &Arc<Connection>,
        key: &str,
    ) -> Result<(String, bool), CoreError> {
        if let Some(session) = self.sessions.lock().await.get(key) {
            return Ok((String::clone(session), false));
        }
        let cwd = Option::clone(&self.cwd)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| ".".to_string());
        let response = connection
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await
            .map_err(|error| {
                CoreError::ProviderUnavailable(format!("ACP session/new failed: {error}"))
            })?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CoreError::ProviderUnavailable("ACP session/new returned no session".to_string())
            })?
            .to_string();
        if let Some(model) = model_option_value(&self.model) {
            connection
                .request(
                    "session/set_config_option",
                    json!({ "sessionId": session_id, "configId": "model", "value": model }),
                )
                .await
                .map_err(|error| {
                    CoreError::ProviderUnavailable(format!(
                        "ACP agent has no model '{model}': {error}"
                    ))
                })?;
        }
        self.sessions
            .lock()
            .await
            .insert(key.to_string(), String::clone(&session_id));
        Ok((session_id, true))
    }
}

/// The prompt for a turn: the whole conversation for a fresh session, else the newest user message.
pub fn prompt_text(request: &ModelRequest, fresh_session: bool) -> String {
    if !fresh_session {
        if let Some(last) = request
            .messages
            .iter()
            .rev()
            .find(|message| matches!(message.role, MessageRole::User | MessageRole::Tool))
        {
            let text = message_text(last);
            if !text.is_empty() {
                return text;
            }
        }
    }
    request
        .messages
        .iter()
        .filter_map(|message| {
            let text = message_text(message);
            (!text.is_empty()).then(|| match message.role {
                MessageRole::System => format!("[instructions]\n{text}"),
                MessageRole::Assistant => format!("[assistant]\n{text}"),
                MessageRole::User | MessageRole::Tool => text,
            })
        })
        .collect::<Vec<String>>()
        .join("\n\n")
}

/// The readable text of a message, including tool results.
fn message_text(message: &silver_core::model::ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<&str>>()
        .join("\n")
        .trim()
        .to_string()
}

#[async_trait]
impl Model for AcpProvider {
    fn name(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        mut request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        let connection = self.connection().await?;
        let key = request
            .cache_key
            .take()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let (session_id, fresh) = self.session(&connection, &key).await?;
        let text = prompt_text(&request, fresh);

        let (sender, receiver) = mpsc::unbounded_channel();
        connection.route_updates(sender);

        let (done_tx, done_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let outcome = connection
                .request(
                    "session/prompt",
                    json!({
                        "sessionId": session_id,
                        "prompt": [{ "type": "text", "text": text }],
                    }),
                )
                .await;
            drop(done_tx.send(outcome));
        });

        let stream = futures::stream::unfold(
            (receiver, done_rx, cancel, false),
            |(mut receiver, mut done_rx, cancel, finished)| async move {
                if finished {
                    return None;
                }
                tokio::select! {
                    _ = cancel.cancelled() => Some((
                        Err(CoreError::ProviderUnavailable(
                            "provider stream cancelled".to_string(),
                        )),
                        (receiver, done_rx, CancellationToken::new(), true),
                    )),
                    update = receiver.recv() => match update {
                        Some(AcpUpdate::Message(text)) => Some((
                            Ok(ModelStreamEvent::TextDelta(text)),
                            (receiver, done_rx, cancel, false),
                        )),
                        Some(AcpUpdate::Thought(text)) | Some(AcpUpdate::ToolActivity(text)) => {
                            Some((
                                Ok(ModelStreamEvent::ReasoningDelta(text)),
                                (receiver, done_rx, cancel, false),
                            ))
                        }
                        Some(AcpUpdate::Finished(_)) | None => Some((
                            Ok(ModelStreamEvent::Finish(FinishReason::Stop)),
                            (receiver, done_rx, cancel, true),
                        )),
                    },
                    outcome = done_rx.recv() => match outcome {
                        Some(Ok(_)) | None => Some((
                            Ok(ModelStreamEvent::Finish(FinishReason::Stop)),
                            (receiver, done_rx, cancel, true),
                        )),
                        Some(Err(error)) => Some((
                            Err(CoreError::ProviderUnavailable(format!(
                                "ACP prompt failed: {error}"
                            ))),
                            (receiver, done_rx, cancel, true),
                        )),
                    },
                }
            },
        );
        Ok(Box::pin(stream))
    }
}
