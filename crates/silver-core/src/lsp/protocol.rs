//! JSON-RPC 2.0 framing over stdio: `Content-Length: <bytes>\r\n\r\n<JSON body>`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// The JSON-RPC protocol version every envelope carries.
pub const JSONRPC: &str = "2.0";

/// LSP error code: the document changed since the request was computed.
pub const ERROR_CONTENT_MODIFIED: i64 = -32801;
/// LSP error code: the server does not implement the requested method.
pub const ERROR_METHOD_NOT_FOUND: i64 = -32601;

/// Refuse a header block larger than this before the terminating blank line.
pub const MAX_HEADER_BYTES: usize = 8192;
/// Refuse a message body larger than this.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The framing or envelope itself is broken (as opposed to RequestError,
/// which is a conformant error response from the server).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProtocolError(pub String);

impl ProtocolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// A JSON-RPC error response; carries code, message and optional data.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("LSP error {code}: {message}")]
pub struct RequestError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// A JSON-RPC id. Our requests use integers, but a server may echo strings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    /// The integer form, or None for a string id (which we never send).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            RequestId::Number(number) => Some(*number),
            RequestId::String(_) => None,
        }
    }
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        RequestId::Number(value)
    }
}

/// A JSON-RPC 2.0 request envelope.
#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<&'a Value>,
}

/// A JSON-RPC 2.0 notification envelope (no id).
#[derive(Debug, Serialize)]
pub struct Notification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<&'a Value>,
}

/// A JSON-RPC 2.0 success response envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: Value,
}

/// The error object of a JSON-RPC 2.0 error response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 error response envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub error: ErrorObject,
}

/// The broad class of an inbound message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response,
    Notification,
    Invalid,
}

/// A classified inbound message.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Request {
        id: RequestId,
        method: String,
        params: Option<Value>,
    },
    Response {
        id: RequestId,
        result: Option<Value>,
    },
    Error {
        id: RequestId,
        error: ErrorObject,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
    Invalid,
}

/// Build a JSON-RPC 2.0 request envelope.
pub fn make_request<'a>(id: RequestId, method: &'a str, params: Option<&'a Value>) -> Request<'a> {
    Request {
        jsonrpc: JSONRPC,
        id,
        method,
        params,
    }
}

/// Build a JSON-RPC 2.0 notification envelope (no id).
pub fn make_notification<'a>(method: &'a str, params: Option<&'a Value>) -> Notification<'a> {
    Notification {
        jsonrpc: JSONRPC,
        method,
        params,
    }
}

/// Build a JSON-RPC 2.0 success response envelope.
pub fn make_response(id: RequestId, result: Value) -> Response {
    Response {
        jsonrpc: JSONRPC.to_string(),
        id,
        result,
    }
}

/// Build a JSON-RPC 2.0 error response envelope.
pub fn make_error_response(id: RequestId, code: i64, message: impl Into<String>) -> ErrorResponse {
    ErrorResponse {
        jsonrpc: JSONRPC.to_string(),
        id,
        error: ErrorObject {
            code,
            message: message.into(),
            data: None,
        },
    }
}

/// Encode an envelope as compact UTF-8 JSON with an exact Content-Length header.
pub fn encode_message<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_vec(message)
        .map_err(|error| ProtocolError::new(format!("cannot encode LSP message: {error}")))?;
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Classify an inbound JSON value exactly as the reference implementation does.
pub fn classify_message(message: &Value) -> MessageKind {
    if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC) {
        return MessageKind::Invalid;
    }
    if message.get("id").is_some() {
        if message.get("method").is_some() {
            return MessageKind::Request;
        }
        if message.get("result").is_some() || message.get("error").is_some() {
            return MessageKind::Response;
        }
        return MessageKind::Invalid;
    }
    if message.get("method").is_some() {
        MessageKind::Notification
    } else {
        MessageKind::Invalid
    }
}

impl Incoming {
    /// Classify and decode an inbound JSON value.
    pub fn from_value(message: &Value) -> Self {
        if message.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC) {
            return Incoming::Invalid;
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = message.get("params").cloned();
        if message.get("id").is_some() {
            let id = message
                .get("id")
                .and_then(|raw| RequestId::deserialize(raw).ok())
                .unwrap_or(RequestId::Number(0));
            if let Some(method) = method {
                return Incoming::Request { id, method, params };
            }
            if let Some(error) = message.get("error") {
                if let Ok(error) = ErrorObject::deserialize(error) {
                    return Incoming::Error { id, error };
                }
                return Incoming::Invalid;
            }
            if message.get("result").is_some() {
                return Incoming::Response {
                    id,
                    result: message.get("result").cloned(),
                };
            }
            return Incoming::Invalid;
        }
        match method {
            Some(method) => Incoming::Notification { method, params },
            None => Incoming::Invalid,
        }
    }
}

/// Read one header block. None on a clean EOF before any header started.
async fn read_headers<R>(reader: &mut R) -> Result<Option<HashMap<String, String>>, ProtocolError>
where
    R: AsyncBufRead + Unpin,
{
    let mut headers = HashMap::new();
    let mut header_bytes = 0usize;
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|error| ProtocolError::new(format!("cannot read LSP header: {error}")))?;
        if read == 0 {
            if headers.is_empty() {
                return Ok(None);
            }
            return Err(ProtocolError::new(
                "unexpected EOF while reading LSP headers",
            ));
        }
        // Cap against a server streaming headers without ever emitting the blank line.
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ProtocolError::new(
                "LSP header block exceeded 8 KiB without terminator",
            ));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            return Ok(Some(headers));
        }
        let text = std::str::from_utf8(&line).map_err(|_err| {
            ProtocolError::new(format!(
                "non-ASCII LSP header: {}",
                String::from_utf8_lossy(&line)
            ))
        })?;
        let Some((key, value)) = text.split_once(':') else {
            return Err(ProtocolError::new(format!(
                "malformed LSP header line: {text:?}"
            )));
        };
        if key.is_empty() {
            return Err(ProtocolError::new(format!(
                "malformed LSP header line: {text:?}"
            )));
        }
        headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
    }
}

/// Read one framed message; None on a clean EOF between messages.
pub async fn read_message<R>(reader: &mut R) -> Result<Option<Value>, ProtocolError>
where
    R: AsyncBufRead + Unpin,
{
    let headers = match read_headers(reader).await? {
        Some(headers) => headers,
        None => return Ok(None),
    };
    let Some(length) = headers.get("content-length") else {
        return Err(ProtocolError::new(format!(
            "LSP message missing Content-Length: {headers:?}"
        )));
    };
    let length: usize = length
        .parse()
        .map_err(|_err| ProtocolError::new(format!("non-integer Content-Length: {length:?}")))?;
    if length > MAX_BODY_BYTES {
        return Err(ProtocolError::new(format!(
            "unreasonable Content-Length: {length}"
        )));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.map_err(|error| {
        ProtocolError::new(format!(
            "truncated LSP body: expected {length} bytes: {error}"
        ))
    })?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| ProtocolError::new(format!("invalid JSON in LSP body: {error}")))
}
