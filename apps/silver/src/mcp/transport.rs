//! MCP transports: stdio JSON-RPC with a filtered child environment, and Streamable HTTP with JSON
//! or SSE responses.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::Instant;

use super::{
    McpError, HTTP_REJECTION_BODY_CHARS, JSONRPC_METHOD_NOT_FOUND, JSONRPC_VERSION,
    LATEST_HANDSHAKE_VERSION, MCP_HTTP_MAX_BODY_BYTES, SAFE_ENV_KEYS,
    SAFE_ENV_KEYS_CASE_INSENSITIVE,
};

/// A live connection capable of one JSON-RPC exchange, driven under a per-server mutex because a
/// stdio session is a single stream.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Short human label used in error text ("stdio" or "http").
    fn describe(&self) -> &'static str;
    /// Send one JSON-RPC request and read the matching response.
    async fn exchange(&mut self, request: Value, timeout: Duration) -> Result<Value, McpError>;
    /// Send one JSON-RPC notification (no response expected).
    async fn notify(&mut self, notification: Value) -> Result<(), McpError>;
    /// True while the transport is believed usable (a stdio child has not exited).
    fn is_alive(&mut self) -> bool;
    /// Terminate the transport, killing a spawned child process.
    async fn shutdown(&mut self);
}

/// Build the transport a server config asks for.
pub fn build_transport(
    config: &crate::config::McpServerConfig,
) -> Result<Box<dyn McpTransport>, McpError> {
    match config.transport {
        crate::config::McpTransport::Stdio => Ok(Box::new(StdioTransport::spawn(config)?)),
        crate::config::McpTransport::Http => Ok(Box::new(HttpTransport::new(config)?)),
    }
}

/// A stdio server's environment: the safe baseline, XDG_* and the server's own env, never the
/// daemon's, so keys cannot leak into an untrusted server.
pub fn build_safe_env(server_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in std::env::vars() {
        let allowed = SAFE_ENV_KEYS.contains(&key.as_str())
            || SAFE_ENV_KEYS_CASE_INSENSITIVE
                .iter()
                .any(|candidate| key.eq_ignore_ascii_case(candidate))
            || key.starts_with("XDG_");
        if allowed {
            env.insert(key, value);
        }
    }
    for (key, value) in server_env {
        env.insert(String::clone(key), String::clone(value));
    }
    env
}

/// Validate a remote MCP URL (mcp_tool_errors._validate_remote_mcp_url): must be a
/// non-empty http(s) URL with a host.
pub fn validate_remote_mcp_url(server_name: &str, url: &str) -> Result<String, McpError> {
    let bad = |detail: String| McpError::InvalidUrl {
        server: server_name.to_string(),
        detail,
    };
    let stripped = url.trim();
    if stripped.is_empty() {
        return Err(bad("empty url".to_string()));
    }
    let (scheme, rest) = stripped
        .split_once("://")
        .ok_or_else(|| bad(format!("scheme must be http or https ({stripped:?})")))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(bad(format!(
            "scheme must be http or https, got {scheme:?} ({stripped:?})"
        )));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    if authority.is_empty() || host.is_empty() {
        return Err(bad(format!("missing host ({stripped:?})")));
    }
    Ok(stripped.to_string())
}

/// A build-time JSON-RPC request object.
pub fn jsonrpc_request(id: i64, method: &str, params: &Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    })
}

/// A JSON-RPC notification object (no id).
pub fn jsonrpc_notification(method: &str, params: Option<Value>) -> Value {
    let mut value = json!({"jsonrpc": JSONRPC_VERSION, "method": method});
    if let Some(params) = params {
        value["params"] = params;
    }
    value
}

/// What one incoming JSON-RPC message means to a waiting request.
pub enum Incoming {
    /// The matching response arrived: Ok is a result, Err is a JSON-RPC error.
    Done(Result<Value, McpError>),
    /// A notification or unrelated message: keep reading.
    Ignore,
    /// A server-initiated request: the caller replies with this error object.
    Reply(Value),
}

/// Classify an incoming message against the id we are waiting for.
pub fn handle_incoming(server: &str, message: &Value, expected_id: i64) -> Incoming {
    let id = message.get("id");
    let is_expected = match id {
        Some(Value::Number(n)) => n.as_i64() == Some(expected_id),
        Some(Value::String(s)) => s.parse::<i64>().ok() == Some(expected_id),
        _ => false,
    };
    if !is_expected {
        if id.is_some() && message.get("method").is_some() {
            // Server-initiated request (sampling/elicitation/roots). This port has none of the
            // callbacks, so answer method-not-found rather than hanging the stream.
            let id = id.cloned().unwrap_or(Value::Null);
            return Incoming::Reply(json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "error": {"code": JSONRPC_METHOD_NOT_FOUND, "message": "method not supported by this client"},
            }));
        }
        return Incoming::Ignore;
    }
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Incoming::Done(Err(McpError::Rpc {
            server: server.to_string(),
            code,
            message: text.to_string(),
        }));
    }
    match message.get("result") {
        Some(result) => Incoming::Done(Ok(Value::clone(result))),
        None => Incoming::Done(Err(McpError::protocol(
            server,
            "response carried neither result nor error",
        ))),
    }
}

/// A JSON-RPC message that may be a single object or a batch array. Returns one Incoming per
/// element plus the first outgoing server-request reply, ready to be written back.
fn classify_message(
    server: &str,
    message: &Value,
    expected_id: i64,
) -> (Option<Incoming>, Vec<Value>) {
    let mut replies = Vec::new();
    let messages: Vec<&Value> = match message {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for item in messages {
        match handle_incoming(server, item, expected_id) {
            Incoming::Ignore => {}
            Incoming::Reply(reply) => replies.push(reply),
            done => return (Some(done), replies),
        }
    }
    (None, replies)
}

/// Standard input/output transport: newline-delimited JSON-RPC over a spawned child.
pub struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioTransport {
    /// Spawn the configured command with the filtered environment (kill_on_drop: the child is
    /// signalled when this value is dropped, and explicitly on shutdown).
    pub fn spawn(config: &crate::config::McpServerConfig) -> Result<Self, McpError> {
        let command = config
            .command
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                McpError::protocol(
                    String::clone(&config.name),
                    "stdio server has no 'command' in config",
                )
            })?;
        let env = build_safe_env(&config.env);
        let mut cmd = Command::new(command);
        cmd.args(&config.args)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|err| {
            McpError::transport(
                String::clone(&config.name),
                format!("spawn {command:?} failed: {err}"),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            McpError::transport(String::clone(&config.name), "child stdin unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            McpError::transport(String::clone(&config.name), "child stdout unavailable")
        })?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn write_value(&mut self, value: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(value).map_err(|err| {
            McpError::protocol("stdio", format!("failed to encode JSON-RPC: {err}"))
        })?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| McpError::transport("stdio", format!("write failed: {err}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| McpError::transport("stdio", format!("flush failed: {err}")))?;
        Ok(())
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    fn describe(&self) -> &'static str {
        "stdio"
    }

    async fn exchange(&mut self, request: Value, timeout: Duration) -> Result<Value, McpError> {
        let expected_id = request.get("id").and_then(Value::as_i64).unwrap_or(0);
        let mut line = serde_json::to_string(&request)
            .map_err(|err| McpError::protocol("stdio", format!("encode failed: {err}")))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| McpError::transport("stdio", format!("write failed: {err}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| McpError::transport("stdio", format!("flush failed: {err}")))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(McpError::transport(
                    "stdio",
                    "timed out waiting for response",
                ));
            }
            let mut buf = String::new();
            let read = tokio::time::timeout(remaining, self.stdout.read_line(&mut buf))
                .await
                .map_err(|_elapsed| McpError::transport("stdio", "timed out waiting for response"))?
                .map_err(|err| McpError::transport("stdio", format!("read failed: {err}")))?;
            if read == 0 {
                return Err(McpError::transport(
                    "stdio",
                    "child closed stdout (subprocess exited)",
                ));
            }
            if buf.len() > MCP_HTTP_MAX_BODY_BYTES {
                return Err(McpError::transport(
                    "stdio",
                    "response exceeded the body cap",
                ));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(_) => continue, // non-JSON banner line on the stream
            };
            let (done, replies) = classify_message("stdio", &message, expected_id);
            for reply in replies {
                self.write_value(&reply).await?;
            }
            if let Some(done) = done {
                return match done {
                    Incoming::Done(result) => result,
                    _ => Err(McpError::protocol("stdio", "unexpected message state")),
                };
            }
        }
    }

    async fn notify(&mut self, notification: Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(&notification)
            .map_err(|err| McpError::protocol("stdio", format!("encode failed: {err}")))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| McpError::transport("stdio", format!("write failed: {err}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| McpError::transport("stdio", format!("flush failed: {err}")))?;
        Ok(())
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    async fn shutdown(&mut self) {
        drop(self.child.kill().await);
    }
}

/// Streamable HTTP transport: POST JSON-RPC, accept application/json or text/event-stream.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
}

impl HttpTransport {
    /// Validate the URL and seed the protocol-version header (mcp_tool_transport._run_http).
    pub fn new(config: &crate::config::McpServerConfig) -> Result<Self, McpError> {
        let url = validate_remote_mcp_url(&config.name, config.url.as_deref().unwrap_or(""))?;
        let mut headers = BTreeMap::clone(&config.headers);
        if !headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("mcp-protocol-version"))
        {
            headers.insert(
                "mcp-protocol-version".to_string(),
                LATEST_HANDSHAKE_VERSION.to_string(),
            );
        }
        let client = reqwest::Client::builder().build().map_err(|err| {
            McpError::transport(
                String::clone(&config.name),
                format!("HTTP client build failed: {err}"),
            )
        })?;
        Ok(Self {
            client,
            url,
            headers,
            session_id: None,
        })
    }

    fn request_builder(&self) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        if let Some(session_id) = &self.session_id {
            builder = builder.header("mcp-session-id", session_id);
        }
        builder
    }

    fn capture_session(&mut self, response: &reqwest::Response) {
        if let Some(value) = response.headers().get("mcp-session-id") {
            if let Ok(text) = value.to_str() {
                if !text.is_empty() {
                    self.session_id = Some(text.to_string());
                }
            }
        }
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    fn describe(&self) -> &'static str {
        "http"
    }

    async fn exchange(&mut self, request: Value, timeout: Duration) -> Result<Value, McpError> {
        let expected_id = request.get("id").and_then(Value::as_i64).unwrap_or(0);
        let response = self
            .request_builder()
            .json(&request)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| {
                McpError::transport("http", format!("POST {} failed: {err}", self.url))
            })?;
        self.capture_session(&response);
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(HTTP_REJECTION_BODY_CHARS)
                .collect::<String>();
            return Err(McpError::transport(
                "http",
                format!("HTTP {status} from POST {}: {body}", self.url),
            ));
        }
        if content_type == "application/json" {
            let body = response.text().await.map_err(|err| {
                McpError::transport("http", format!("reading body failed: {err}"))
            })?;
            if body.len() > MCP_HTTP_MAX_BODY_BYTES {
                return Err(McpError::transport(
                    "http",
                    "response exceeded the body cap",
                ));
            }
            let message: Value = serde_json::from_str(&body).map_err(|err| {
                McpError::protocol("http", format!("invalid JSON response: {err}"))
            })?;
            let (done, _replies) = classify_message("http", &message, expected_id);
            return match done {
                Some(Incoming::Done(result)) => result,
                _ => Err(McpError::protocol(
                    "http",
                    "response did not carry the requested id",
                )),
            };
        }
        if content_type == "text/event-stream" {
            return self.read_sse(response, expected_id, timeout).await;
        }
        Err(McpError::transport(
            "http",
            format!(
                "returned Content-Type '{content_type}', not an MCP response (expected application/json or text/event-stream)"
            ),
        ))
    }

    async fn notify(&mut self, notification: Value) -> Result<(), McpError> {
        let response = self
            .request_builder()
            .json(&notification)
            .send()
            .await
            .map_err(|err| {
                McpError::transport("http", format!("POST {} failed: {err}", self.url))
            })?;
        self.capture_session(&response);
        Ok(())
    }

    fn is_alive(&mut self) -> bool {
        true
    }

    async fn shutdown(&mut self) {
        // Stateless HTTP has no child to reap.
    }
}

impl HttpTransport {
    async fn read_sse(
        &mut self,
        response: reqwest::Response,
        expected_id: i64,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let deadline = Instant::now() + timeout;
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(McpError::transport("http", "SSE stream timed out"));
            }
            let chunk = match tokio::time::timeout(remaining, stream.next()).await {
                Err(_) => return Err(McpError::transport("http", "SSE stream timed out")),
                Ok(None) => {
                    return Err(McpError::transport(
                        "http",
                        "SSE stream closed before the response arrived",
                    ))
                }
                Ok(Some(Err(err))) => {
                    return Err(McpError::transport(
                        "http",
                        format!("SSE read failed: {err}"),
                    ))
                }
                Ok(Some(Ok(chunk))) => chunk,
            };
            buffer.extend_from_slice(&chunk);
            if buffer.len() > MCP_HTTP_MAX_BODY_BYTES {
                return Err(McpError::transport(
                    "http",
                    "SSE response exceeded the body cap",
                ));
            }
            while let Some((end, boundary)) = find_sse_boundary(&buffer) {
                let event: Vec<u8> = buffer[..end].to_vec();
                buffer.drain(..end + boundary);
                if let Some(data) = sse_event_data(&event) {
                    if let Ok(message) = serde_json::from_str::<Value>(&data) {
                        let (done, _replies) = classify_message("http", &message, expected_id);
                        if let Some(Incoming::Done(result)) = done {
                            return result;
                        }
                    }
                }
            }
        }
    }
}

/// Find the first SSE event boundary: two consecutive line terminators, where a terminator is
/// `\r\n`, a lone `\r`, or `\n` (mcp_tool_errors._SSE_BOUNDARY_RE). Returns
/// (event_end, boundary_len).
pub fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buffer.len() {
        if let Some(first) = line_terminator_len(buffer, i) {
            if let Some(second) = line_terminator_len(buffer, i + first) {
                return Some((i, first + second));
            }
            i += first;
        } else {
            i += 1;
        }
    }
    None
}

fn line_terminator_len(buffer: &[u8], index: usize) -> Option<usize> {
    match buffer.get(index)? {
        b'\n' => Some(1),
        b'\r' => {
            if buffer.get(index + 1) == Some(&b'\n') {
                Some(2)
            } else {
                Some(1)
            }
        }
        _ => None,
    }
}

/// Concatenate the `data:` lines of one SSE event, or None when it has none.
pub fn sse_event_data(event: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(event);
    let normalized = text.replace('\r', "\n");
    let mut data: Vec<&str> = Vec::new();
    for line in normalized.split('\n') {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}
