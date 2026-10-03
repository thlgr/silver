//! Async LSP client over stdio, one process per (server, workspace root). Diagnostics are tagged
//! with the document version they describe, so a slow server's leftovers never pass for a verdict
//! on the current content. Whole-document sync only.

use super::protocol::{self, Incoming, RequestError, RequestId};
use super::LspError;
use crate::tools::command::kill_process_group;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Default initialize handshake budget.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
/// Default per-request budget.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for a document-scoped diagnostics wait.
pub const DIAGNOSTICS_DOCUMENT_WAIT: Duration = Duration::from_secs(5);
/// Budget for a full-project diagnostics wait.
pub const DIAGNOSTICS_FULL_WAIT: Duration = Duration::from_secs(10);
/// Grace period after the exit notification before the process is killed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// Retries for transient ContentModified responses.
pub const MAX_CONTENT_MODIFIED_RETRIES: usize = 3;
/// Base delay for the ContentModified retry backoff.
pub const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;
type BoxReader = Box<dyn AsyncBufRead + Send + Unpin>;
type PendingSender = oneshot::Sender<Result<Value, RequestError>>;
type PendingMap = Mutex<HashMap<i64, PendingSender>>;

/// The lifecycle state of a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}

impl ClientState {
    /// A stable lower-case label for status output.
    pub fn name(self) -> &'static str {
        match self {
            ClientState::Starting => "starting",
            ClientState::Running => "running",
            ClientState::Stopping => "stopping",
            ClientState::Stopped => "stopped",
            ClientState::Error => "error",
        }
    }
}

/// Options for spawning a server.
#[derive(Clone, Debug)]
pub struct LspClientOptions {
    pub server_id: String,
    pub workspace_root: PathBuf,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub initialization_options: Value,
    pub init_timeout: Duration,
    pub request_timeout: Duration,
}

impl LspClientOptions {
    /// Options with inherited environment, no cwd override and default timeouts.
    pub fn new(
        server_id: impl Into<String>,
        workspace_root: PathBuf,
        command: Vec<String>,
    ) -> Self {
        Self {
            server_id: server_id.into(),
            workspace_root,
            command,
            env: BTreeMap::new(),
            initialization_options: Value::Object(serde_json::Map::new()),
            init_timeout: INITIALIZE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// A file URI for a path, percent-encoding everything outside the safe set.
/// Handles spaces, unicode and Windows drive letters.
pub fn file_uri(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_string_lossy().replace('\\', "/");
    let text = if cfg!(windows) && !text.starts_with('/') {
        format!("/{text}")
    } else {
        text
    };
    let mut encoded = String::with_capacity(text.len() + 7);
    encoded.push_str("file://");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                encoded.push('%');
                encoded.push(
                    char::from_digit(u32::from(byte >> 4), 16)
                        .unwrap_or('0')
                        .to_ascii_uppercase(),
                );
                encoded.push(
                    char::from_digit(u32::from(byte & 0x0f), 16)
                        .unwrap_or('0')
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    encoded
}

/// Inverse of file_uri; a non-file URI is returned verbatim.
pub fn uri_to_path(uri: &str) -> PathBuf {
    let Some(raw) = uri.strip_prefix("file://") else {
        return PathBuf::from(uri);
    };
    let mut raw = raw.to_string();
    if cfg!(windows) && raw.starts_with('/') && raw.len() > 2 && raw.as_bytes()[2] == b':' {
        raw.remove(0);
    }
    PathBuf::from(percent_decode(&raw))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The client capabilities we advertise during initialize.
fn client_capabilities() -> Value {
    json!({
        "window": {"workDoneProgress": true},
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "didChangeWatchedFiles": {"dynamicRegistration": true},
            "diagnostics": {"refreshSupport": false}
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false, "didOpen": true, "didChange": true,
                "didSave": true, "willSave": false, "willSaveWaitUntil": false
            },
            "publishDiagnostics": {
                "relatedInformation": true, "versionSupport": true,
                "codeDescriptionSupport": true, "dataSupport": false
            },
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "definition": {"linkSupport": true},
            "references": {},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true}
        },
        "general": {"positionEncodings": ["utf-16"]}
    })
}

#[derive(Clone, Debug, Default)]
struct DiagnosticState {
    items: Arc<Vec<Value>>,
    /// The document version this result describes; -1 means no data yet.
    version: i64,
}

struct IoHandles {
    child: Option<Child>,
    stdin: BoxWriter,
    reader: BoxReader,
    stderr: Option<ChildStderr>,
    pgid: Option<u32>,
}

/// One language server process and one workspace root.
pub struct LspClient {
    server_id: String,
    workspace_root: PathBuf,
    child: AsyncMutex<Option<Child>>,
    stdin: AsyncMutex<Option<BoxWriter>>,
    pending: PendingMap,
    /// Absolute path to the document version last sent (didOpen = 0, +1 per didChange).
    docs: Mutex<HashMap<PathBuf, i64>>,
    diagnostics: Mutex<HashMap<PathBuf, DiagnosticState>>,
    next_id: AtomicI64,
    state: Mutex<ClientState>,
    pgid: Option<u32>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
    /// The server's last stderr line: when it dies, usually the reason (a missing binary,
    /// a bad toolchain), so errors can say more than "connection closed".
    last_stderr: Mutex<String>,
    shutdown_once: AtomicBool,
    init_timeout: Duration,
    request_timeout: Duration,
    initialization_options: Value,
}

impl LspClient {
    /// Whether this client serves `server_id` at `root`.
    pub fn matches(&self, server_id: &str, root: &Path) -> bool {
        self.server_id == server_id && self.workspace_root == root
    }

    /// Whether this client serves the same server and root as `other`.
    pub fn same_target(&self, other: &LspClient) -> bool {
        self.matches(&other.server_id, &other.workspace_root)
    }

    /// Spawn a server with its own process group and run the initialize handshake.
    pub async fn spawn(options: LspClientOptions) -> Result<Arc<Self>, LspError> {
        let Some(program) = options.command.first() else {
            return Err(LspError::ServerUnavailable("empty LSP command".to_string()));
        };
        let mut command = Command::new(program);
        command.args(&options.command[1..]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.current_dir(&options.workspace_root);
        for (key, value) in &options.env {
            command.env(key, value);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| {
            LspError::ServerUnavailable(format!("cannot spawn {program}: {error}"))
        })?;
        let pgid = child.id();
        let Some(stdin) = child.stdin.take().map(|stdin| Box::new(stdin) as BoxWriter) else {
            return Err(LspError::ServerUnavailable(
                "server has no stdin pipe".to_string(),
            ));
        };
        let Some(reader) = child
            .stdout
            .take()
            .map(|stdout| Box::new(BufReader::new(stdout)) as BoxReader)
        else {
            return Err(LspError::ServerUnavailable(
                "server has no stdout pipe".to_string(),
            ));
        };
        let stderr = child.stderr.take();
        let client = Self::assemble(
            options.server_id,
            options.workspace_root,
            IoHandles {
                child: Some(child),
                stdin,
                reader,
                stderr,
                pgid,
            },
            options.initialization_options,
            options.init_timeout,
            options.request_timeout,
        );
        match client.initialize().await {
            Ok(()) => {
                client.set_state(ClientState::Running);
                Ok(client)
            }
            Err(error) => {
                client.cleanup_after_failed_start();
                Err(error)
            }
        }
    }

    fn assemble(
        server_id: String,
        workspace_root: PathBuf,
        handles: IoHandles,
        initialization_options: Value,
        init_timeout: Duration,
        request_timeout: Duration,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            server_id,
            workspace_root,
            child: AsyncMutex::new(handles.child),
            stdin: AsyncMutex::new(Some(handles.stdin)),
            pending: Mutex::new(HashMap::new()),
            docs: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            state: Mutex::new(ClientState::Starting),
            pgid: handles.pgid,
            reader_task: Mutex::new(None),
            stderr_task: Mutex::new(None),
            last_stderr: Mutex::new(String::new()),
            shutdown_once: AtomicBool::new(false),
            init_timeout,
            request_timeout,
            initialization_options,
        });
        if let Some(stderr) = handles.stderr {
            let weak = Arc::downgrade(&client);
            let handle = tokio::spawn(async move { drain_stderr(weak, stderr).await });
            *client.stderr_task.lock().expect("stderr task lock") = Some(handle);
        }
        let weak = Arc::downgrade(&client);
        let handle = tokio::spawn(async move { reader_loop(weak, handles.reader).await });
        *client.reader_task.lock().expect("reader task lock") = Some(handle);
        client
    }

    /// The server id.
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// The workspace root this client serves.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// The terminal lifecycle state.
    pub fn state(&self) -> ClientState {
        *self.state.lock().expect("state lock")
    }

    /// True once the initialize handshake has completed.
    pub fn is_running(&self) -> bool {
        self.state() == ClientState::Running
    }

    fn connection_open(&self) -> bool {
        matches!(self.state(), ClientState::Starting | ClientState::Running)
    }

    fn set_state(&self, state: ClientState) {
        *self.state.lock().expect("state lock") = state;
    }

    async fn initialize(&self) -> Result<(), LspError> {
        let name = self
            .workspace_root
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("workspace");
        let params = json!({
            "processId": std::process::id(),
            "rootUri": file_uri(&self.workspace_root),
            "rootPath": self.workspace_root.to_string_lossy(),
            "workspaceFolders": [{"name": name, "uri": file_uri(&self.workspace_root)}],
            "capabilities": client_capabilities(),
            "initializationOptions": self.initialization_options,
        });
        self.request("initialize", Some(&params), self.init_timeout)
            .await?;
        self.notify("initialized", None).await?;
        if self
            .initialization_options
            .as_object()
            .map(|options| !options.is_empty())
            .unwrap_or(false)
        {
            let settings = json!({"settings": self.initialization_options});
            self.notify("workspace/didChangeConfiguration", Some(&settings))
                .await?;
        }
        Ok(())
    }

    fn cleanup_after_failed_start(&self) {
        self.set_state(ClientState::Error);
        self.kill_group();
        self.abort_tasks();
        self.pending.lock().expect("pending lock").clear();
    }

    fn require_open(&self, method: &str) -> Result<(), LspError> {
        if self.connection_open() {
            Ok(())
        } else {
            Err(self.closed(&format!("cannot send {method}: server connection closed")))
        }
    }

    fn closed(&self, what: &str) -> LspError {
        let said = self.last_stderr.lock().expect("stderr lock");
        let message = if said.is_empty() {
            what.to_string()
        } else {
            format!("{what}; the server said: {said}")
        };
        protocol::ProtocolError::new(message).into()
    }

    async fn write(&self, bytes: &[u8]) -> Result<(), LspError> {
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Err(protocol::ProtocolError::new("server stdin is closed").into());
        };
        stdin.write_all(bytes).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        self.require_open(method)?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(id, sender);
        let message = protocol::make_request(RequestId::Number(id), method, params);
        let bytes = protocol::encode_message(&message)?;
        if let Err(error) = self.write(&bytes).await {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Err(_)) => Err(self.closed("server connection closed")),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(LspError::Timeout(method.to_string()))
            }
        }
    }

    async fn request_with_retry(
        &self,
        method: &str,
        params: Option<&Value>,
    ) -> Result<Value, LspError> {
        let mut delay = RETRY_BASE_DELAY;
        for attempt in 0..=MAX_CONTENT_MODIFIED_RETRIES {
            match self.request(method, params, self.request_timeout).await {
                Err(LspError::Request(error))
                    if error.code == protocol::ERROR_CONTENT_MODIFIED
                        && attempt < MAX_CONTENT_MODIFIED_RETRIES =>
                {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            }
        }
        unreachable!("the retry loop returns on its final attempt")
    }

    async fn notify(&self, method: &str, params: Option<&Value>) -> Result<(), LspError> {
        if !self.connection_open() {
            return Ok(());
        }
        let message = protocol::make_notification(method, params);
        let bytes = protocol::encode_message(&message)?;
        self.write(&bytes).await
    }

    async fn dispatch(&self, message: Value) {
        match Incoming::from_value(&message) {
            Incoming::Response { id, result } => {
                if let Some(id) = id.as_i64() {
                    if let Some(sender) = self.pending.lock().expect("pending lock").remove(&id) {
                        drop(sender.send(Ok(result.unwrap_or(Value::Null))));
                    }
                }
            }
            Incoming::Error { id, error } => {
                if let Some(id) = id.as_i64() {
                    if let Some(sender) = self.pending.lock().expect("pending lock").remove(&id) {
                        drop(sender.send(Err(RequestError {
                            code: error.code,
                            message: error.message,
                            data: error.data,
                        })));
                    }
                }
            }
            Incoming::Request { id, method, params } => {
                self.handle_server_request(id, &method, params).await;
            }
            Incoming::Notification { method, params } => {
                if method == "textDocument/publishDiagnostics" {
                    self.handle_publish_diagnostics(params);
                }
            }
            Incoming::Invalid => {}
        }
    }

    async fn handle_server_request(&self, id: RequestId, method: &str, params: Option<Value>) {
        let reply: Value = match method {
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "workspace/diagnostic/refresh" => envelope(protocol::make_response(id, Value::Null)),
            "workspace/configuration" => envelope(protocol::make_response(
                id,
                self.configuration_for(params.as_ref()),
            )),
            "workspace/workspaceFolders" => envelope(protocol::make_response(
                id,
                json!([{
                    "name": self.workspace_root.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("workspace"),
                    "uri": file_uri(&self.workspace_root),
                }]),
            )),
            _ => envelope(protocol::make_error_response(
                id,
                protocol::ERROR_METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        };
        if let Ok(bytes) = protocol::encode_message(&reply) {
            drop(self.write(&bytes).await);
        }
    }

    fn configuration_for(&self, params: Option<&Value>) -> Value {
        let Some(items) = params
            .and_then(|params| params.get("items"))
            .and_then(Value::as_array)
        else {
            return json!([Value::Null]);
        };
        Value::Array(
            items
                .iter()
                .map(|item| self.configuration_section(item))
                .collect(),
        )
    }

    fn configuration_section(&self, item: &Value) -> Value {
        let Some(section) = item.get("section").and_then(Value::as_str) else {
            return Value::clone(&self.initialization_options);
        };
        let mut current = &self.initialization_options;
        for part in section.split('.') {
            match current.get(part) {
                Some(next) => current = next,
                None => return Value::Null,
            }
        }
        Value::clone(current)
    }

    fn handle_publish_diagnostics(&self, params: Option<Value>) {
        let Some(params) = params else {
            return;
        };
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return;
        };
        let path = uri_to_path(uri);
        let items = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let echoed_version = params.get("version").and_then(Value::as_i64);
        let current_version = self
            .docs
            .lock()
            .expect("docs lock")
            .get(&path)
            .copied()
            .unwrap_or(-1);
        let mut store = self.diagnostics.lock().expect("diagnostics lock");
        let entry = store.entry(path).or_default();
        entry.items = Arc::new(items);
        entry.version = echoed_version.unwrap_or(current_version);
    }

    fn document_version(&self, path: &Path) -> i64 {
        self.docs
            .lock()
            .expect("docs lock")
            .get(path)
            .copied()
            .unwrap_or(-1)
    }

    /// Send didOpen the first time or didChange afterwards; returns the new
    /// document version. Whole-document sync is always sent.
    pub async fn open_file(&self, path: &Path, language_id: &str) -> Result<i64, LspError> {
        if !self.is_running() {
            return Err(protocol::ProtocolError::new("client not running").into());
        }
        let absolute = absolute_path(path);
        let text = read_text_lossy(&absolute).await?;
        let uri = file_uri(&absolute);
        let existing = self
            .docs
            .lock()
            .expect("docs lock")
            .get(&absolute)
            .copied()
            .filter(|version| *version >= 0);
        match existing {
            Some(version) => {
                let new_version = version + 1;
                self.docs
                    .lock()
                    .expect("docs lock")
                    .insert(absolute, new_version);
                let params = json!({
                    "textDocument": {"uri": uri, "version": new_version},
                    "contentChanges": [{"text": text}],
                });
                self.notify("textDocument/didChange", Some(&params)).await?;
                Ok(new_version)
            }
            None => {
                self.docs.lock().expect("docs lock").insert(absolute, 0);
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    Some(&json!({"changes": [{"uri": uri, "type": 1}]})),
                )
                .await?;
                let params = json!({
                    "textDocument": {
                        "uri": uri, "languageId": language_id, "version": 0, "text": text
                    },
                });
                self.notify("textDocument/didOpen", Some(&params)).await?;
                Ok(0)
            }
        }
    }

    /// Send didSave for a file. Some linters only re-scan on save.
    pub async fn save_file(&self, path: &Path) -> Result<(), LspError> {
        if !self.is_running() {
            return Ok(());
        }
        let absolute = absolute_path(path);
        let params = json!({"textDocument": {"uri": file_uri(&absolute)}});
        self.notify("textDocument/didSave", Some(&params)).await
    }

    /// Send didClose and forget the tracked document.
    pub async fn close_file(&self, path: &Path) -> Result<(), LspError> {
        let absolute = absolute_path(path);
        self.docs.lock().expect("docs lock").remove(&absolute);
        self.diagnostics
            .lock()
            .expect("diagnostics lock")
            .remove(&absolute);
        if self.is_running() {
            let params = json!({"textDocument": {"uri": file_uri(&absolute)}});
            self.notify("textDocument/didClose", Some(&params)).await?;
        }
        Ok(())
    }

    /// Wait until fresh diagnostics for path at version or newer arrive.
    /// False means no verdict arrived in the budget, not that the file is clean.
    pub async fn wait_for_diagnostics(
        &self,
        path: &Path,
        version: i64,
        timeout: Duration,
    ) -> Result<bool, LspError> {
        let absolute = absolute_path(path);
        let deadline = Instant::now() + timeout;
        loop {
            if !self.is_running() {
                return Err(protocol::ProtocolError::new(
                    "server connection closed while waiting for diagnostics",
                )
                .into());
            }
            if self.diagnostics_fresh(&absolute, version) {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let remaining = deadline - now;
            tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
        }
    }

    fn diagnostics_fresh(&self, path: &Path, version: i64) -> bool {
        self.diagnostics
            .lock()
            .expect("diagnostics lock")
            .get(path)
            .map(|state| state.version >= version)
            .unwrap_or(false)
    }

    /// Stored diagnostics for one file. With fresh_only a result only counts
    /// once its version tag has caught up to the document's.
    pub fn diagnostics_for(&self, path: &Path, fresh_only: bool) -> Arc<Vec<Value>> {
        let absolute = absolute_path(path);
        let document_version = self.document_version(&absolute);
        let store = self.diagnostics.lock().expect("diagnostics lock");
        let Some(state) = store.get(&absolute) else {
            return Arc::new(Vec::new());
        };
        if fresh_only && state.version < document_version {
            return Arc::new(Vec::new());
        }
        Arc::clone(&state.items)
    }

    async fn position_request(
        &self,
        method: &str,
        path: &Path,
        line: u32,
        character: u32,
        extra: Option<Value>,
    ) -> Result<Value, LspError> {
        let absolute = absolute_path(path);
        let mut params = json!({
            "textDocument": {"uri": file_uri(&absolute)},
            "position": {"line": line, "character": character},
        });
        if let (Some(map), Some(Value::Object(extra))) = (params.as_object_mut(), extra) {
            map.extend(extra);
        }
        self.request_with_retry(method, Some(&params)).await
    }

    /// textDocument/hover at a position.
    pub async fn hover(&self, path: &Path, line: u32, character: u32) -> Result<Value, LspError> {
        self.position_request("textDocument/hover", path, line, character, None)
            .await
    }

    /// textDocument/definition at a position.
    pub async fn definition(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        self.position_request("textDocument/definition", path, line, character, None)
            .await
    }

    /// textDocument/references at a position.
    pub async fn references(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Value, LspError> {
        let extra = json!({"context": {"includeDeclaration": include_declaration}});
        self.position_request(
            "textDocument/references",
            path,
            line,
            character,
            Some(extra),
        )
        .await
    }

    /// textDocument/documentSymbol.
    pub async fn document_symbols(&self, path: &Path) -> Result<Value, LspError> {
        let absolute = absolute_path(path);
        let params = json!({"textDocument": {"uri": file_uri(&absolute)}});
        self.request_with_retry("textDocument/documentSymbol", Some(&params))
            .await
    }

    /// textDocument/rename at a position with a new name.
    pub async fn rename(
        &self,
        path: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Value, LspError> {
        let extra = json!({"newName": new_name});
        self.position_request("textDocument/rename", path, line, character, Some(extra))
            .await
    }

    /// Best-effort graceful shutdown, then kill the process group.
    pub async fn shutdown(&self) {
        if self.shutdown_once.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.connection_open() {
            self.set_state(ClientState::Stopping);
            drop(self.request("shutdown", None, Duration::from_secs(2)).await);
            drop(self.notify("exit", None).await);
            {
                let mut guard = self.child.lock().await;
                if let Some(child) = guard.as_mut() {
                    if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
                        .await
                        .is_err()
                    {
                        drop(child.start_kill());
                        drop(child.wait().await);
                    }
                }
            }
            self.kill_group();
        } else {
            self.kill_group();
        }
        self.abort_tasks();
        self.pending.lock().expect("pending lock").clear();
        self.set_state(ClientState::Stopped);
    }

    fn kill_group(&self) {
        if let Some(pgid) = self.pgid {
            kill_process_group(pgid);
        }
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(child) = guard.as_mut() {
                drop(child.start_kill());
            }
        }
    }

    fn abort_tasks(&self) {
        if let Some(handle) = self.reader_task.lock().expect("reader task lock").take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_task.lock().expect("stderr task lock").take() {
            handle.abort();
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.shutdown_once.store(true, Ordering::SeqCst);
        self.kill_group();
        if let Ok(mut guard) = self.reader_task.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        if let Ok(mut guard) = self.stderr_task.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn read_text_lossy(path: &Path) -> Result<String, LspError> {
    let bytes = tokio::fs::read(path).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn reader_loop(weak: Weak<LspClient>, mut reader: BoxReader) {
    loop {
        match protocol::read_message(&mut reader).await {
            Ok(Some(message)) => {
                let Some(client) = weak.upgrade() else {
                    return;
                };
                client.dispatch(message).await;
            }
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(error = %error, "LSP reader loop stopped");
                break;
            }
        }
    }
    if let Some(client) = weak.upgrade() {
        if !matches!(client.state(), ClientState::Stopping | ClientState::Stopped) {
            client.set_state(ClientState::Error);
        }
        client.pending.lock().expect("pending lock").clear();
    }
}

async fn drain_stderr(weak: Weak<LspClient>, stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let Some(client) = weak.upgrade() else { break };
                    tracing::debug!(server = %client.server_id, "lsp stderr: {}", truncate(trimmed, 1000));
                    *client.last_stderr.lock().expect("stderr lock") =
                        truncate(trimmed, 300).to_string();
                }
            }
            Err(_) => break,
        }
    }
}

fn envelope<T: serde::Serialize>(reply: T) -> Value {
    serde_json::to_value(reply).unwrap_or(Value::Null)
}

fn truncate(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        text
    } else {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
}
