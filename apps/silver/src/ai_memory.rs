//! The managed ai-memory server, the shared cross-harness memory of record: silver starts it (or
//! adopts one already listening), registers it over MCP, records its own runs and the handoff a
//! new session claims, and stops it on shutdown (docs/adr/0002-shared-memory-via-ai-memory.md).

use crate::config::{Config, McpServerConfig, McpTransport};
use crate::mcp::truncate_head_tail;
use serde_json::{json, Value};
use silver_core::hash::content_hash;
use silver_protocol::{EventPayload, RunId, SessionId, ToolCallId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// The MCP server name, and the executable silver looks for on `PATH`.
pub const SERVER_NAME: &str = "ai-memory";
/// How long a freshly spawned server has to answer its health endpoint.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How often the health endpoint is polled while starting.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// The producer name silver sends as `extension`, and its identity in the ingest key.
const PRODUCER: &str = "silver";
/// The agent kind silver reports on the wire. ai-memory keeps tool content only for the kinds it
/// has a verified shape for, and silver posts Claude Code's, so it reports as `claude-code` and
/// keeps the truth in `extension=silver` until it has a kind of its own upstream.
const WIRE_AGENT: &str = "claude-code";
/// ai-memory takes at most this many events in one `/hook/batch`; a fuller queue drops events.
const BATCH_MAX: usize = 256;
/// Tool events leave this many queue slots free, so a lifecycle event is never the one dropped.
const LIFECYCLE_RESERVE: usize = 8;
/// A session with no run for this long has ended: ai-memory then writes its summary and handoff.
const SESSION_QUIET: Duration = Duration::from_secs(600);
/// How often sessions are checked for having gone quiet.
const QUIET_CHECK: Duration = Duration::from_secs(30);
/// How long one `/healthz` probe may take.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long one `/hook/batch` post may take; a batch can hold several runs' events.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one `/handoff` fetch may take; the server itself answers within a second.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(2);
/// A claimed handoff is cut to this many characters, ai-memory's own session-brief budget (about
/// a thousand tokens), so it does not eat a small model's context.
const HANDOFF_CHARS: usize = 4_000;
/// How long to wait before the one retry of a batch the server did not take.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);
/// How long shutdown waits for the queued events to be delivered.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// A running (or adopted) ai-memory server and the MCP entry that reaches it.
pub struct AiMemory {
    child: Option<Child>,
    server: McpServerConfig,
    endpoint: String,
    hooks: MemoryHooks,
}

impl AiMemory {
    /// Start ai-memory beside silver, or adopt a server already listening on the configured
    /// bind. `None` when memory is off or the binary is missing; the daemon runs either way.
    pub async fn start(config: &Config, silver_data_dir: &Path) -> Option<AiMemory> {
        if !config.memory.enabled {
            return None;
        }
        let bind = config.memory.bind.trim().to_string();
        let endpoint = format!("http://{bind}");
        let binary = config.memory.binary.as_deref().unwrap_or(SERVER_NAME);
        let child = if healthy(&endpoint).await {
            tracing::info!(endpoint = %endpoint, "using the ai-memory server already listening");
            None
        } else {
            let fallback_dir = silver_data_dir.join(SERVER_NAME);
            let data_dir = config.memory.data_dir.as_deref().unwrap_or(&fallback_dir);
            let child = match spawn(binary, &bind, data_dir) {
                Ok(child) => child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        binary = %binary,
                        "ai-memory is not installed, so shared memory is off; install it and restart"
                    );
                    return None;
                }
                Err(error) => {
                    tracing::warn!(%error, binary = %binary, "ai-memory could not be started");
                    return None;
                }
            };
            if !wait_healthy(&endpoint).await {
                // Dropping the child kills it: the command sets `kill_on_drop`.
                tracing::warn!(endpoint = %endpoint, binary = %binary, "ai-memory did not become healthy");
                return None;
            }
            tracing::info!(endpoint = %endpoint, data_dir = %data_dir.display(), "ai-memory started");
            Some(child)
        };
        Some(AiMemory {
            child,
            server: server_for(&bind),
            hooks: MemoryHooks::start(&endpoint),
            endpoint,
        })
    }

    /// The MCP entry to register, pointing at this server.
    pub fn mcp_server(&self) -> &McpServerConfig {
        &self.server
    }

    /// The server's base URL, for reading its read-only HTTP API.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Where silver's own runs report what they do.
    pub fn hooks(&self) -> &MemoryHooks {
        &self.hooks
    }

    /// Deliver the queued events, then stop the server silver started; an adopted server is
    /// left running.
    pub async fn shutdown(mut self) {
        self.hooks.flush().await;
        let Some(mut child) = self.child.take() else {
            return;
        };
        drop(child.start_kill());
        match child.wait().await {
            Ok(status) => tracing::info!(%status, "ai-memory stopped"),
            Err(error) => tracing::warn!(%error, "ai-memory did not stop cleanly"),
        }
    }
}

/// The MCP entry for a server listening on `bind`.
fn server_for(bind: &str) -> McpServerConfig {
    McpServerConfig {
        name: SERVER_NAME.to_string(),
        transport: McpTransport::Http,
        url: Some(format!("http://{bind}/mcp")),
        ..McpServerConfig::default()
    }
}

/// Spawn `ai-memory serve --transport http` on `bind`, storing under `data_dir`.
fn spawn(binary: &str, bind: &str, data_dir: &Path) -> std::io::Result<Child> {
    Command::new(binary)
        .arg("serve")
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg(bind)
        // The memory panel reads the server's read-only /api/v1, which is mounted with the web UI.
        .arg("--enable-web")
        .env("AI_MEMORY_DATA_DIR", data_dir)
        .env("AI_MEMORY_LOG_LEVEL", "warn")
        // The store is text; keep it zero-LLM and offline so no embedding model is fetched.
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .kill_on_drop(true)
        .spawn()
}

/// Poll the health endpoint until it answers or the start budget runs out.
async fn wait_healthy(endpoint: &str) -> bool {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if healthy(endpoint).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// One client for the loopback ai-memory server, shared by the health polls, the handoff fetch
/// and the event batches; each request sets its own deadline.
pub(crate) fn loopback_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // ai-memory listens on loopback; never send it through the environment's proxy.
            .no_proxy()
            .build()
            .expect("the loopback client builds")
    })
}

/// Whether an ai-memory server is answering on `endpoint`.
async fn healthy(endpoint: &str) -> bool {
    loopback_client()
        .get(format!("{endpoint}/healthz"))
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// The `(workspace, project)` ai-memory resolves for this folder: the nearest `.ai-memory.toml`
/// when it names them, else `default` and the folder's name. ai-memory's git-remote identity can
/// name a project differently; the panel then shows an empty list rather than a wrong one.
pub(crate) fn resolve_project(root: &Path) -> (String, String) {
    let marker = std::fs::read_to_string(root.join(".ai-memory.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok());
    let named = |key: &str| marker.as_ref()?.get(key)?.as_str().map(str::to_string);
    let workspace = named("workspace").unwrap_or_else(|| "default".to_string());
    let project = named("project").unwrap_or_else(|| {
        root.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("default")
            .to_string()
    });
    (workspace, project)
}

/// Silver's own lifecycle capture. A native run has no harness hooks, so silver posts what it does
/// to ai-memory's `/hook/batch` itself. Events leave through one bounded queue and never hold up
/// a run; a batch the server does not take is logged and dropped.
#[derive(Clone)]
pub struct MemoryHooks {
    endpoint: String,
    tx: mpsc::Sender<Msg>,
}

enum Msg {
    /// Deliver now. A session's activity cancels the end waiting on it.
    Now(Event),
    /// A run finished: deliver this `session-end` once its session has been quiet a while.
    Quiet(Event),
    /// Deliver everything, quiet sessions included. The sender is dropped once it has gone out.
    Flush(oneshot::Sender<()>),
}

/// Who a run's events belong to.
struct RunScope {
    session: String,
    run: String,
    cwd: String,
    workspace: String,
    project: String,
}

/// One lifecycle event, as ai-memory's canonical `name` with its JSON `body`.
struct Event {
    scope: Arc<RunScope>,
    name: &'static str,
    /// What tells this event from the run's others of the same name; empty for a run's own.
    id: String,
    body: Value,
}

impl Event {
    /// The `{url, body}` item `/hook/batch` takes. The query names the project and source and
    /// carries an ingest key made from the event's identity, so a resend is stored once.
    fn wire(&self, endpoint: &str) -> Option<Value> {
        let scope = &*self.scope;
        let key = content_hash(
            &json!([PRODUCER, scope.session, scope.run, self.name, self.id]).to_string(),
        );
        let url = reqwest::Url::parse_with_params(
            &format!("{endpoint}/hook"),
            [
                ("event", self.name),
                ("agent", WIRE_AGENT),
                ("workspace", scope.workspace.as_str()),
                ("project", scope.project.as_str()),
                ("extension", PRODUCER),
                ("source_event", self.name),
                ("ingest_key", key.as_str()),
            ],
        )
        .ok()?;
        Some(json!({ "url": url.as_str(), "body": self.body }))
    }
}

impl MemoryHooks {
    /// Start delivering to the server at `endpoint`.
    fn start(endpoint: &str) -> MemoryHooks {
        let (tx, rx) = mpsc::channel(BATCH_MAX);
        tokio::spawn(deliver(endpoint.to_string(), rx));
        MemoryHooks {
            endpoint: endpoint.to_string(),
            tx,
        }
    }

    /// The capture for one run in the workspace folder `root`.
    pub fn run(&self, session: SessionId, run: RunId, root: &Path) -> RunCapture {
        let (workspace, project) = resolve_project(root);
        RunCapture {
            tx: mpsc::Sender::clone(&self.tx),
            scope: Arc::new(RunScope {
                session: session.to_string(),
                run: run.to_string(),
                cwd: root.to_string_lossy().into_owned(),
                workspace,
                project,
            }),
            tools: HashMap::new(),
        }
    }

    /// Claim the handoff waiting in the run's project for the run's session. A native run has no
    /// harness hook to receive it, and ai-memory serves a handoff once; call this for the first
    /// run of a session only.
    pub async fn handoff(&self, run: &RunCapture) -> Option<String> {
        fetch_handoff(&self.endpoint, &run.scope).await
    }

    /// Deliver what is queued, and end every session still waiting to.
    async fn flush(&self) {
        let (done, delivered) = oneshot::channel();
        let flushed = async {
            self.tx.send(Msg::Flush(done)).await.ok()?;
            delivered.await.ok()
        };
        if tokio::time::timeout(FLUSH_TIMEOUT, flushed).await.is_err() {
            tracing::warn!("ai-memory lifecycle events were not delivered before shutdown");
        }
    }
}

/// What one run tells ai-memory, as it happens: its start and prompt, each tool call, its end.
pub struct RunCapture {
    tx: mpsc::Sender<Msg>,
    scope: Arc<RunScope>,
    /// Tool calls that started and have no result yet: their name and arguments.
    tools: HashMap<ToolCallId, (String, Value)>,
}

impl RunCapture {
    /// The run began with `prompt`; `new_session` when it is the first of its session. A session
    /// is announced once: ai-memory keeps a session's observations after an idle `session-end`
    /// and rewrites its summary on the next end, so a later run needs no new `session-start`.
    pub fn begin(&self, new_session: bool, prompt: &str) {
        if new_session {
            self.send("session-start", "", json!({}));
        }
        self.send("user-prompt-submit", "", json!({ "prompt": prompt }));
    }

    /// Watch the run's events: a finished tool call becomes one `post-tool-use` with its
    /// redacted argument preview and result summary. ai-memory keeps a tool call's content only
    /// for harnesses it has a verified shape for, so a native run's shows as a count.
    pub fn observe(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::ToolStarted {
                tool_call_id,
                name,
                preview,
            } => {
                self.tools
                    .insert(tool_call_id.clone(), (name.clone(), preview.clone()));
            }
            EventPayload::ToolCompleted {
                tool_call_id,
                summary,
                ..
            } => {
                if let Some((name, input)) = self.tools.remove(tool_call_id) {
                    let body =
                        json!({ "tool_name": name, "tool_input": input, "tool_response": summary });
                    self.send_tool("post-tool-use", &tool_call_id.0, body);
                }
            }
            _ => {}
        }
    }

    /// The run ended, however it ended. Its session ends once it stays quiet.
    pub fn finish(self) {
        self.send("stop", "", json!({}));
        let end = self.event("session-end", "", json!({}));
        self.queue(Msg::Quiet(end), false);
    }

    fn event(&self, name: &'static str, id: &str, mut body: Value) -> Event {
        body["session_id"] = Value::from(self.scope.session.as_str());
        body["cwd"] = Value::from(self.scope.cwd.as_str());
        Event {
            scope: Arc::clone(&self.scope),
            name,
            id: id.to_string(),
            body,
        }
    }

    fn send(&self, name: &'static str, id: &str, body: Value) {
        let event = self.event(name, id, body);
        self.queue(Msg::Now(event), false);
    }

    /// A tool call is one of a run's many, so it yields the reserve below to lifecycle events.
    fn send_tool(&self, name: &'static str, id: &str, body: Value) {
        let event = self.event(name, id, body);
        self.queue(Msg::Now(event), true);
    }

    fn queue(&self, msg: Msg, burst: bool) {
        if burst && self.tx.capacity() <= LIFECYCLE_RESERVE {
            tracing::warn!(
                "an ai-memory tool event was dropped: the queue keeps a reserve for lifecycle events"
            );
            return;
        }
        if self.tx.try_send(msg).is_err() {
            tracing::warn!("an ai-memory event was dropped: the queue is full or closed");
        }
    }
}

/// Events waiting to go out, and the sessions waiting to end.
#[derive(Default)]
struct Outbox {
    batch: Vec<Event>,
    quiet: Vec<(Instant, Event)>,
}

impl Outbox {
    /// Take one message in, handing back the flush request it carried.
    fn accept(&mut self, msg: Msg) -> Option<oneshot::Sender<()>> {
        match msg {
            Msg::Now(event) => {
                self.forget_quiet(&event);
                self.batch.push(event);
            }
            Msg::Quiet(event) => {
                self.forget_quiet(&event);
                self.quiet.push((Instant::now(), event));
            }
            Msg::Flush(done) => {
                self.end_quiet(Duration::ZERO);
                return Some(done);
            }
        }
        None
    }

    /// Drop the end waiting on `event`'s session: it is active again, or about to wait afresh.
    fn forget_quiet(&mut self, event: &Event) {
        self.quiet
            .retain(|(_, end)| end.scope.session != event.scope.session);
    }

    /// Move the sessions that have had no run for `quiet_for` into the batch as ended.
    fn end_quiet(&mut self, quiet_for: Duration) {
        let (ended, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.quiet)
            .into_iter()
            .partition(|(since, _)| since.elapsed() >= quiet_for);
        self.quiet = waiting;
        self.batch.extend(ended.into_iter().map(|(_, end)| end));
    }
}

/// The one task that sends events, so a session's arrive in the order they happened. It ends when
/// every sender is gone.
async fn deliver(endpoint: String, mut rx: mpsc::Receiver<Msg>) {
    let mut outbox = Outbox::default();
    let mut check = tokio::time::interval(QUIET_CHECK);
    loop {
        let mut flush = None;
        let mut open = true;
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => {
                    flush = outbox.accept(msg);
                    while flush.is_none() && outbox.batch.len() < BATCH_MAX {
                        let Ok(msg) = rx.try_recv() else { break };
                        flush = outbox.accept(msg);
                    }
                }
                None => {
                    outbox.end_quiet(Duration::ZERO);
                    open = false;
                }
            },
            _ = check.tick() => outbox.end_quiet(SESSION_QUIET),
        }
        send(&endpoint, std::mem::take(&mut outbox.batch)).await;
        drop(flush);
        if !open {
            return;
        }
    }
}

/// Ask ai-memory for the run's project handoff. It comes already marked as untrusted history, so
/// it is passed on as is, cut in the middle so the closing marker stays. A failed fetch is logged:
/// the run goes on without it.
async fn fetch_handoff(endpoint: &str, scope: &RunScope) -> Option<String> {
    let client = loopback_client();
    let fetched = async {
        client
            .get(format!("{endpoint}/handoff"))
            .query(&[
                ("agent", WIRE_AGENT),
                ("cwd", scope.cwd.as_str()),
                ("workspace", scope.workspace.as_str()),
                ("project", scope.project.as_str()),
                ("session_id", scope.session.as_str()),
            ])
            .timeout(HANDOFF_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
    };
    match fetched.await {
        Ok(text) if text.trim().is_empty() => None,
        Ok(text) => Some(truncate_head_tail(&text, HANDOFF_CHARS, "HANDOFF")),
        Err(error) => {
            tracing::warn!(%error, "the ai-memory handoff could not be fetched");
            None
        }
    }
}

/// Post events to `/hook/batch`. A batch the server does not take is retried once, then logged
/// and dropped; delivery never blocks a run.
async fn send(endpoint: &str, events: Vec<Event>) {
    let client = loopback_client();
    for chunk in events.chunks(BATCH_MAX) {
        let items: Vec<Value> = chunk
            .iter()
            .filter_map(|event| event.wire(endpoint))
            .collect();
        if items.is_empty() {
            continue;
        }
        let mut failure: Option<String> = None;
        let mut delivered = false;
        // One retry covers a transient refusal or a server restarting; a 4xx will not heal.
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
            match client
                .post(format!("{endpoint}/hook/batch"))
                .timeout(SEND_TIMEOUT)
                .json(&items)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    delivered = true;
                    break;
                }
                Ok(response) => {
                    failure = Some(format!("ai-memory answered {}", response.status()));
                    if !response.status().is_server_error() {
                        break;
                    }
                }
                Err(error) => failure = Some(error.to_string()),
            }
        }
        if !delivered {
            tracing::warn!(
                dropped = items.len(),
                reason = failure.as_deref().unwrap_or("unknown"),
                "ai-memory lifecycle events were not delivered"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_resolves_to_its_name_or_the_marker_file() {
        let plain = std::env::temp_dir().join(format!("silver-mem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&plain).unwrap();
        let (workspace, project) = resolve_project(&plain);
        assert_eq!(workspace, "default");
        assert_eq!(project, plain.file_name().unwrap().to_str().unwrap());

        let marked = std::env::temp_dir().join(format!("silver-mem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&marked).unwrap();
        std::fs::write(
            marked.join(".ai-memory.toml"),
            "workspace = \"team\"\nproject = \"silver\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_project(&marked),
            ("team".to_string(), "silver".to_string())
        );
    }

    #[test]
    fn the_mcp_entry_points_at_the_bind() {
        let server = server_for("127.0.0.1:49374");
        assert_eq!(server.name, "ai-memory");
        assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:49374/mcp"));
        assert!(matches!(server.transport, McpTransport::Http));
        assert!(server.enabled);
    }

    fn scope() -> Arc<RunScope> {
        Arc::new(RunScope {
            session: "s1".to_string(),
            run: "r1".to_string(),
            cwd: "/work/app".to_string(),
            workspace: "team".to_string(),
            project: "app".to_string(),
        })
    }

    fn event(scope: &Arc<RunScope>, name: &'static str, id: &str) -> Event {
        Event {
            scope: Arc::clone(scope),
            name,
            id: id.to_string(),
            body: json!({}),
        }
    }

    fn query(item: &Value) -> HashMap<String, String> {
        let url = reqwest::Url::parse(item["url"].as_str().unwrap()).unwrap();
        url.query_pairs().into_owned().collect()
    }

    #[test]
    fn an_event_names_its_project_and_source_and_carries_a_stable_key() {
        let scope = scope();
        let endpoint = "http://127.0.0.1:49374";
        let first = event(&scope, "post-tool-use", "call_1")
            .wire(endpoint)
            .unwrap();
        let again = event(&scope, "post-tool-use", "call_1")
            .wire(endpoint)
            .unwrap();
        let other = event(&scope, "post-tool-use", "call_2")
            .wire(endpoint)
            .unwrap();

        let pairs = query(&first);
        assert_eq!(pairs["event"], "post-tool-use");
        assert_eq!(pairs["source_event"], "post-tool-use");
        assert_eq!(pairs["agent"], "claude-code");
        assert_eq!(pairs["extension"], "silver");
        assert_eq!(pairs["workspace"], "team");
        assert_eq!(pairs["project"], "app");
        assert_eq!(pairs["ingest_key"].len(), 64);
        assert_eq!(pairs["ingest_key"], query(&again)["ingest_key"]);
        assert_ne!(pairs["ingest_key"], query(&other)["ingest_key"]);
    }

    #[test]
    fn a_session_ends_only_after_it_stays_quiet() {
        let scope = scope();
        let mut outbox = Outbox::default();
        outbox.accept(Msg::Quiet(event(&scope, "session-end", "")));
        outbox.end_quiet(SESSION_QUIET);
        assert!(
            outbox.batch.is_empty(),
            "a session that just ran is not quiet"
        );

        outbox.accept(Msg::Now(event(&scope, "user-prompt-submit", "")));
        assert!(outbox.quiet.is_empty(), "a new run cancels the pending end");

        outbox.accept(Msg::Quiet(event(&scope, "session-end", "")));
        let (done, _delivered) = oneshot::channel();
        assert!(outbox.accept(Msg::Flush(done)).is_some());
        let names: Vec<_> = outbox.batch.iter().map(|event| event.name).collect();
        assert_eq!(names, ["user-prompt-submit", "session-end"]);
    }

    #[test]
    fn a_tool_burst_cannot_evict_a_lifecycle_event() {
        let (tx, mut rx) = mpsc::channel(LIFECYCLE_RESERVE + 2);
        let capture = RunCapture {
            tx,
            scope: scope(),
            tools: HashMap::new(),
        };
        // Two tool events fit, the third hits the reserve and is dropped ...
        for _ in 0..3 {
            capture.send_tool("post-tool-use", "call", json!({}));
        }
        // ... while the lifecycle events still get in ahead of the reserve.
        capture.send("stop", "", json!({}));

        let mut names = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Msg::Now(event) = msg {
                names.push(event.name);
            }
        }
        assert_eq!(names, ["post-tool-use", "post-tool-use", "stop"]);
    }

    #[tokio::test]
    async fn a_run_reaches_the_server_in_order_and_flushing_ends_its_session() {
        let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
        let app = axum::Router::new().route(
            "/hook/batch",
            axum::routing::post({
                let seen = Arc::clone(&seen);
                move |axum::Json(items): axum::Json<Vec<Value>>| async move {
                    seen.lock().unwrap().extend(items);
                    axum::Json(json!({ "accepted": 5 }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let hooks = MemoryHooks::start(&endpoint);
        let root = std::env::temp_dir().join(format!("silver-hooks-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let mut capture = hooks.run(SessionId::new(), RunId::new(), &root);
        let call = ToolCallId("call_1".to_string());
        capture.begin(true, "fix the parser");
        capture.observe(&EventPayload::ToolStarted {
            tool_call_id: call.clone(),
            name: "read_file".to_string(),
            preview: json!({ "path": "parser.rs" }),
        });
        capture.observe(&EventPayload::ToolCompleted {
            tool_call_id: call,
            status: silver_protocol::ToolStatus::Completed,
            summary: "read 40 lines".to_string(),
        });
        capture.finish();
        hooks.flush().await;

        let seen = seen.lock().unwrap();
        let events: Vec<_> = seen
            .iter()
            .map(|item| query(item)["event"].clone())
            .collect();
        assert_eq!(
            events,
            [
                "session-start",
                "user-prompt-submit",
                "post-tool-use",
                "stop",
                "session-end"
            ]
        );
        assert_eq!(seen[1]["body"]["prompt"], "fix the parser");
        let tool = &seen[2]["body"];
        assert_eq!(tool["tool_name"], "read_file");
        assert_eq!(tool["tool_input"]["path"], "parser.rs");
        assert_eq!(tool["tool_response"], "read 40 lines");
        assert_eq!(tool["cwd"], root.to_string_lossy().as_ref());
        assert!(tool["session_id"].is_string());
    }

    #[tokio::test]
    async fn a_refused_batch_is_retried_once() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
        let app = axum::Router::new().route(
            "/hook/batch",
            axum::routing::post({
                let attempts = Arc::clone(&attempts);
                let seen = Arc::clone(&seen);
                move |axum::Json(items): axum::Json<Vec<Value>>| async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    seen.lock().unwrap().extend(items);
                    axum::Json(json!({ "accepted": 1 })).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        send(&endpoint, vec![event(&scope(), "stop", "")]).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry");
        assert_eq!(seen.lock().unwrap().len(), 1, "the retry delivered it");
    }

    /// A stub `/handoff` that answers with `body` and keeps the query it was asked with.
    async fn handoff_server(
        body: String,
    ) -> (String, Arc<std::sync::Mutex<HashMap<String, String>>>) {
        let asked: Arc<std::sync::Mutex<HashMap<String, String>>> = Arc::default();
        let app =
            axum::Router::new().route(
                "/handoff",
                axum::routing::get({
                    let asked = Arc::clone(&asked);
                    move |axum::extract::Query(query): axum::extract::Query<
                        HashMap<String, String>,
                    >| async move {
                        *asked.lock().unwrap() = query;
                        body
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (endpoint, asked)
    }

    #[tokio::test]
    async fn a_handoff_is_claimed_for_the_runs_session_and_project() {
        let (endpoint, asked) =
            handoff_server("**Next steps**\n- fix the parser".to_string()).await;

        let handoff = fetch_handoff(&endpoint, &scope()).await;

        assert_eq!(handoff.as_deref(), Some("**Next steps**\n- fix the parser"));
        let asked = asked.lock().unwrap();
        assert_eq!(asked["agent"], "claude-code");
        assert_eq!(asked["session_id"], "s1");
        assert_eq!(asked["cwd"], "/work/app");
        assert_eq!(asked["workspace"], "team");
        assert_eq!(asked["project"], "app");
    }

    #[tokio::test]
    async fn no_handoff_is_none_and_a_long_one_keeps_its_closing_marker() {
        let (endpoint, _) = handoff_server("\n".to_string()).await;
        assert_eq!(fetch_handoff(&endpoint, &scope()).await, None);

        let end = "<!-- ai-memory:untrusted-history:end -->";
        let long = format!("start\n{}\n{end}\n", "step\n".repeat(HANDOFF_CHARS));
        let (endpoint, _) = handoff_server(long).await;
        let handoff = fetch_handoff(&endpoint, &scope()).await.unwrap();
        assert!(handoff.starts_with("start"));
        assert!(handoff.contains("HANDOFF TRUNCATED"));
        assert!(handoff.trim_end().ends_with(end));
        assert!(handoff.chars().count() < HANDOFF_CHARS + 200);
    }
}
