//! ACP: an external agent (`copilot --acp --stdio`, `opencode acp`) driven over stdio as a model.
//! It runs its own loop and tools; only its prose returns. Sessions are keyed by
//! [ModelRequest::cache_key], so each turn sends only the newest message.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, OnceLock, PoisonError};

use async_trait::async_trait;
use serde_json::{json, Value};
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{FinishReason, Model, ModelRequest, ModelStream, ModelStreamEvent};
use silver_protocol::{ApprovalDecision, ContentPart, MessageRole};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

/// ACP protocol version this client speaks.
const PROTOCOL_VERSION: u32 = 1;

/// Default command when the configuration names none.
const DEFAULT_COMMAND: &str = "copilot";

/// Default arguments that put the CLI in ACP stdio mode.
const DEFAULT_ARGS: [&str; 2] = ["--acp", "--stdio"];

/// An agent asking leave to act. `session` is the key of the silver session it works for.
#[derive(Clone, Debug)]
pub struct PermissionRequest {
    pub session: String,
    pub title: String,
    /// The agent's own kind of action: `execute`, `edit`, `read`, `fetch`, ...
    pub kind: String,
    /// What the call would do, as the agent describes it: a command, or a path and new text.
    pub input: Value,
}

/// Whoever can put a permission request to the user, and hear what an agent says of its plan's
/// usage. Without one every request is refused and the usage goes unheard.
#[async_trait]
pub trait AcpBroker: Send + Sync {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision;

    /// The Claude adapter's rate-limit info, which comes with each turn's usage.
    async fn rate_limit(&self, _info: Value) {}
}

static BROKER: OnceLock<Arc<dyn AcpBroker>> = OnceLock::new();

/// Name who answers agents' permission requests, before any agent starts. The first call wins.
pub fn set_broker(broker: Arc<dyn AcpBroker>) {
    drop(BROKER.set(broker));
}

/// One streamed update from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpUpdate {
    /// Prose for the user, with the message it belongs to when the agent names one.
    Message { text: String, id: Option<String> },
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
        "agent_message_chunk" => text_of(update).map(|text| AcpUpdate::Message {
            text,
            id: update
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
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
    /// Where each session's updates go. Turns for several sessions run through one process at
    /// once, so an update is delivered by the session it names.
    updates: std::sync::Mutex<HashMap<String, mpsc::UnboundedSender<AcpUpdate>>>,
    /// The silver session key each routed ACP session works for.
    keys: std::sync::Mutex<HashMap<String, String>>,
    broker: Option<Arc<dyn AcpBroker>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Connection {
    /// Wire a reader and writer into a connection, spawning the read loop.
    fn new<R, W>(reader: R, writer: W, broker: Option<Arc<dyn AcpBroker>>) -> Arc<Self>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let connection = Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            pending: std::sync::Mutex::new(HashMap::new()),
            updates: std::sync::Mutex::new(HashMap::new()),
            keys: std::sync::Mutex::new(HashMap::new()),
            broker,
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
                let Ok(mut value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                // A request (method and id) must be answered or the agent waits forever. It is
                // answered on its own task: a permission request can wait for the user while
                // other sessions go on.
                if value.get("method").is_some() {
                    if value.get("id").is_some() {
                        tokio::spawn(Arc::clone(&conn).answer(value));
                    } else if value["method"] == "session/update" {
                        let session = value
                            .pointer("/params/sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(update) = value.get("params").and_then(map_session_update) {
                            conn.emit(session, update);
                        } else if let (Some(broker), Some(info)) = (
                            &conn.broker,
                            value
                                .pointer_mut("/params/update/_meta")
                                .and_then(|meta| meta.get_mut("_claude/rateLimit"))
                                .map(Value::take),
                        ) {
                            let broker = Arc::clone(broker);
                            tokio::spawn(async move { broker.rate_limit(info).await });
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

    /// Hand one update to the stream of the session it names, if any.
    fn emit(&self, session: &str, update: AcpUpdate) {
        let sender = self
            .updates
            .lock()
            .ok()
            .and_then(|routes| routes.get(session).cloned());
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

    /// Answer a request the agent addressed to us. `session/request_permission` goes to the
    /// broker, who may put it to the user; it is refused when there is none, or it says no, and
    /// the refusal is reported as reasoning. Anything else is not supported.
    async fn answer(self: Arc<Self>, request: Value) {
        let Some(id) = request.get("id") else { return };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
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
            return;
        }
        let params = request.get("params");
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let call = params.and_then(|params| params.get("toolCall"));
        let session = text(params.and_then(|params| params.get("sessionId")));
        let title = call
            .and_then(|call| call.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("an action outside its working directory");
        let key = self
            .keys
            .lock()
            .ok()
            .and_then(|keys| keys.get(&session).cloned());
        let decision = match (&self.broker, key) {
            (Some(broker), Some(session)) => {
                broker
                    .decide(PermissionRequest {
                        session,
                        title: title.to_string(),
                        kind: text(call.and_then(|call| call.get("kind"))),
                        input: call
                            .and_then(|call| call.get("rawInput"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    })
                    .await
            }
            _ => ApprovalDecision::Deny,
        };
        // The agent's own option of the kind wanted; when it has none, `cancelled` is the
        // protocol's way of refusing that names no option.
        let wanted = match decision {
            ApprovalDecision::Approve => ["allow_once", "allow_always"],
            ApprovalDecision::ApproveSession | ApprovalDecision::ApproveAlways => {
                ["allow_always", "allow_once"]
            }
            ApprovalDecision::Deny => ["reject_once", "reject_always"],
        };
        let options = params
            .and_then(|params| params.get("options"))
            .and_then(Value::as_array);
        let chosen = wanted.iter().find_map(|kind| {
            options?
                .iter()
                .find(|option| option["kind"] == *kind)
                .and_then(|option| option["optionId"].as_str())
        });
        let outcome = match chosen {
            Some(option) => json!({ "outcome": { "outcome": "selected", "optionId": option } }),
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": outcome }))
            .await;
        if decision == ApprovalDecision::Deny {
            self.emit(
                &session,
                AcpUpdate::ToolActivity(format!("[refused] {title}")),
            );
        }
    }

    /// Tell the agent something that needs no answer.
    async fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
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

    /// Send a session's updates to this channel until `release_updates`; `key` names the silver
    /// session it works for, which a permission request is put to the user as.
    fn route_updates(&self, session: &str, key: &str, sender: mpsc::UnboundedSender<AcpUpdate>) {
        if let Ok(mut routes) = self.updates.lock() {
            routes.insert(session.to_string(), sender);
        }
        if let Ok(mut keys) = self.keys.lock() {
            keys.insert(session.to_string(), key.to_string());
        }
    }

    /// Stop routing a session's updates, which closes its channel once what is queued is read.
    fn release_updates(&self, session: &str) {
        if let Ok(mut routes) = self.updates.lock() {
            routes.remove(session);
        }
        if let Ok(mut keys) = self.keys.lock() {
            keys.remove(session);
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
        let connection = Connection::new(stdout, stdin, BROKER.get().cloned());
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
        workspace: Option<&std::path::Path>,
    ) -> Result<(String, bool), CoreError> {
        if let Some(session) = self.sessions.lock().await.get(key) {
            return Ok((String::clone(session), false));
        }
        // The agent works where the run does: a session never changes workspace.
        let cwd = workspace
            .map(|path| path.display().to_string())
            .or_else(|| Option::clone(&self.cwd))
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
        let (session_id, fresh) = self
            .session(&connection, &key, request.workspace.as_deref())
            .await?;
        let text = prompt_text(&request, fresh);

        let (sender, updates) = mpsc::unbounded_channel();
        connection.route_updates(&session_id, &key, sender);

        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let turn = Turn {
            updates,
            answer: Some(done_rx),
            cancel,
            connection: Arc::clone(&connection),
            session: session_id.clone(),
            finished: false,
            pending: None,
            message: None,
        };
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
            // The agent sends a turn's updates before its answer, so they are all queued by now.
            connection.release_updates(&session_id);
            drop(done_tx.send(outcome));
        });

        let stream = futures::stream::unfold(turn, |mut turn| async move {
            let event = turn.next().await?;
            Some((event, turn))
        });
        Ok(Box::pin(stream))
    }
}

/// One prompt in flight: what the agent says, until it has answered and the queue runs dry.
struct Turn {
    updates: mpsc::UnboundedReceiver<AcpUpdate>,
    /// The agent's answer to the prompt, until it comes.
    answer: Option<mpsc::UnboundedReceiver<Result<Value, String>>>,
    cancel: CancellationToken,
    connection: Arc<Connection>,
    session: String,
    finished: bool,
    /// One event made from an update, waiting its turn (an assistant text starts before its delta).
    pending: Option<ModelStreamEvent>,
    /// The assistant message the agent is writing, so a new one is told apart from its chunks.
    message: Option<String>,
}

impl Turn {
    /// The next event of the turn, or none once it is over. The turn ends when the queue runs dry
    /// after the agent answered, not when the answer arrives, so text sent just before it is never
    /// lost; only a failed prompt, or a cancel, ends it early.
    async fn next(&mut self) -> Option<CoreResult<ModelStreamEvent>> {
        if let Some(event) = self.pending.take() {
            return Some(Ok(event));
        }
        if self.finished {
            return None;
        }
        loop {
            let answered = async {
                match self.answer.as_mut() {
                    Some(answer) => answer.recv().await,
                    None => std::future::pending().await,
                }
            };
            // Biased: the queue closes just before the answer is sent, and a failed prompt must
            // not be read as a finished one.
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    self.finished = true;
                    // Tell the agent too, or it carries on with a turn nobody wants.
                    self.connection
                        .notify("session/cancel", json!({ "sessionId": self.session }))
                        .await;
                    return Some(Err(CoreError::ProviderUnavailable(
                        "provider stream cancelled".to_string(),
                    )));
                }
                outcome = answered => match outcome {
                    Some(Err(error)) => {
                        self.finished = true;
                        return Some(Err(CoreError::ProviderUnavailable(format!(
                            "ACP prompt failed: {error}"
                        ))));
                    }
                    Some(Ok(_)) | None => self.answer = None,
                },
                update = self.updates.recv() => {
                    let event = match update {
                        Some(AcpUpdate::Message { text, id }) => {
                            // A new message id is a new assistant message: tell the client so it
                            // drops the one before instead of showing them run together.
                            let started = id.is_some() && id != self.message;
                            if id.is_some() {
                                self.message = id;
                            }
                            if started {
                                self.pending = Some(ModelStreamEvent::TextDelta(text));
                                return Some(Ok(ModelStreamEvent::TextStarted));
                            }
                            ModelStreamEvent::TextDelta(text)
                        }
                        Some(AcpUpdate::Thought(text) | AcpUpdate::ToolActivity(text)) => {
                            ModelStreamEvent::ReasoningDelta(text)
                        }
                        Some(AcpUpdate::Finished(_)) | None => {
                            self.finished = true;
                            ModelStreamEvent::Finish(FinishReason::Stop)
                        }
                    };
                    return Some(Ok(event));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn update(session: &str, text: &str) -> String {
        format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session,
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text },
                    },
                },
            })
        )
    }

    struct Answer(ApprovalDecision);

    #[async_trait]
    impl AcpBroker for Answer {
        async fn decide(&self, request: PermissionRequest) -> ApprovalDecision {
            assert_eq!(
                (request.session.as_str(), request.kind.as_str()),
                ("key-a", "edit")
            );
            assert_eq!(request.title, "Write x");
            self.0
        }
    }

    struct Hears(mpsc::UnboundedSender<Value>);

    #[async_trait]
    impl AcpBroker for Hears {
        async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
            ApprovalDecision::Deny
        }

        async fn rate_limit(&self, info: Value) {
            self.0.send(info).unwrap();
        }
    }

    /// What a client says to an agent's permission request, with `broker` to ask.
    async fn permission_reply(broker: Option<Answer>) -> Value {
        let (mut agent_out, client_in) = tokio::io::duplex(4096);
        let (client_out, agent_in) = tokio::io::duplex(4096);
        let broker = broker.map(|answer| Arc::new(answer) as Arc<dyn AcpBroker>);
        let connection = Connection::new(client_in, client_out, broker);
        let (sender, _updates) = mpsc::unbounded_channel();
        connection.route_updates("a", "key-a", sender);
        let request = json!({
            "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
            "params": {
                "sessionId": "a",
                "toolCall": { "title": "Write x", "kind": "edit", "rawInput": { "file_path": "x" } },
                "options": [
                    { "optionId": "yes", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "always", "name": "Always", "kind": "allow_always" },
                    { "optionId": "no", "name": "Reject", "kind": "reject_once" },
                ],
            },
        });
        agent_out
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let line = BufReader::new(agent_in)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn a_permission_request_is_the_brokers_to_answer() {
        let picked = |broker| async move {
            permission_reply(broker).await["result"]["outcome"]["optionId"].clone()
        };
        assert_eq!(picked(Some(Answer(ApprovalDecision::Approve))).await, "yes");
        assert_eq!(
            picked(Some(Answer(ApprovalDecision::ApproveAlways))).await,
            "always"
        );
        assert_eq!(picked(Some(Answer(ApprovalDecision::Deny))).await, "no");
        // With nobody to ask the agent is refused, and told which request it was.
        assert_eq!(picked(None).await, "no");
        assert_eq!(permission_reply(None).await["id"], 7);
    }

    #[tokio::test]
    async fn the_claude_adapters_rate_limit_reaches_the_broker() {
        let (mut agent, client) = tokio::io::duplex(4096);
        let (heard, mut rate_limits) = mpsc::unbounded_channel();
        let broker = Arc::new(Hears(heard)) as Arc<dyn AcpBroker>;
        let _connection = Connection::new(client, tokio::io::sink(), Some(broker));
        let usage = |meta: Value| {
            let update =
                json!({ "sessionUpdate": "usage_update", "used": 9, "size": 99, "_meta": meta });
            let frame = json!({ "jsonrpc": "2.0", "method": "session/update",
                "params": { "sessionId": "a", "update": update } });
            format!("{frame}\n")
        };
        let info = json!({ "status": "allowed", "unifiedWindows": { "five_hour": { "utilization": 0.5 } } });

        for frame in [
            usage(json!({ "_claude/model": "claude-sonnet-5-5" })),
            usage(json!({ "_claude/rateLimit": info })),
        ] {
            agent.write_all(frame.as_bytes()).await.unwrap();
        }
        assert_eq!(rate_limits.recv().await, Some(info));
    }

    #[tokio::test]
    async fn a_failed_prompt_is_never_taken_for_a_finished_one() {
        // The queue is closed and the failure is waiting: both are ready, and either order used
        // to be possible.
        for _ in 0..64 {
            let (sender, updates) = mpsc::unbounded_channel();
            drop(sender);
            let (done, answer) = mpsc::unbounded_channel();
            done.send(Err("limit hit".to_string())).unwrap();
            let mut turn = Turn {
                updates,
                answer: Some(answer),
                cancel: CancellationToken::new(),
                connection: Connection::new(tokio::io::empty(), tokio::io::sink(), None),
                session: "a".into(),
                finished: false,
                pending: None,
                message: None,
            };
            assert!(matches!(turn.next().await, Some(Err(_))));
        }
    }

    #[tokio::test]
    async fn a_new_assistant_message_starts_before_its_text() {
        // Claude Code sends each assistant message with its own id; only the last is the reply.
        let (sender, updates) = mpsc::unbounded_channel();
        let mut turn = Turn {
            updates,
            answer: None,
            cancel: CancellationToken::new(),
            connection: Connection::new(tokio::io::empty(), tokio::io::sink(), None),
            session: "a".into(),
            finished: false,
            pending: None,
            message: None,
        };
        let chunk = |id: &str, text: &str| AcpUpdate::Message {
            text: text.into(),
            id: Some(id.into()),
        };
        for update in [
            chunk("m1", "one "),
            chunk("m1", "message"),
            chunk("m2", "the last"),
        ] {
            sender.send(update).unwrap();
        }
        drop(sender);

        let mut events = Vec::new();
        while let Some(event) = turn.next().await {
            events.push(event.unwrap());
        }
        // The first message announces itself before its text, as does the second; only the ids
        // the agent names get a start, so a chat can wait for the last message.
        assert!(matches!(events[0], ModelStreamEvent::TextStarted));
        assert!(matches!(&events[1], ModelStreamEvent::TextDelta(text) if text == "one "));
        assert!(matches!(&events[2], ModelStreamEvent::TextDelta(text) if text == "message"));
        assert!(matches!(events[3], ModelStreamEvent::TextStarted));
        assert!(matches!(&events[4], ModelStreamEvent::TextDelta(text) if text == "the last"));
        assert!(matches!(
            events[5],
            ModelStreamEvent::Finish(FinishReason::Stop)
        ));
    }

    #[tokio::test]
    async fn updates_reach_only_the_session_they_name() {
        let (mut agent, client) = tokio::io::duplex(4096);
        let connection = Connection::new(client, tokio::io::sink(), None);
        let (first, mut first_updates) = mpsc::unbounded_channel();
        let (second, mut second_updates) = mpsc::unbounded_channel();
        connection.route_updates("a", "key-a", first);
        connection.route_updates("b", "key-b", second);

        for frame in [
            update("b", "for b"),
            update("a", "for a"),
            update("c", "for nobody"),
        ] {
            agent.write_all(frame.as_bytes()).await.unwrap();
        }
        let text = |update| match update {
            Some(AcpUpdate::Message { text, .. }) => text,
            _ => panic!("expected a message"),
        };
        assert_eq!(text(first_updates.recv().await), "for a");
        assert_eq!(text(second_updates.recv().await), "for b");

        // Releasing a session closes its channel once what was queued is read.
        connection.release_updates("a");
        assert!(first_updates.recv().await.is_none());
    }
}
