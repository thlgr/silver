//! Decoder for Bedrock's `vnd.amazon.eventstream` framing. Each payload's `bytes` is the base64 of
//! the provider event an SSE transport would deliver. CRCs are not checked: TLS protects
//! integrity, and the length checks reject mis-framed input.

use base64::Engine;

/// Bytes of the fixed prelude: total length, headers length and the prelude CRC.
const PRELUDE_LEN: usize = 12;

/// Bytes of the trailing message CRC.
const MESSAGE_CRC_LEN: usize = 4;

/// Header value type for a UTF-8 string, the only one Bedrock uses for event metadata.
const HEADER_TYPE_STRING: u8 = 7;

/// A decoded frame: its `:event-type` (or `:exception-type`) and its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Value of the `:event-type` header, when the frame carried one.
    pub event_type: Option<String>,
    /// Value of the `:exception-type` header, when the frame is an error.
    pub exception_type: Option<String>,
    /// Raw payload bytes.
    pub payload: Vec<u8>,
}

impl Frame {
    /// The provider event this frame carries: `{"bytes": "<base64 json>"}` unwrapped, or the
    /// payload as is when it is already JSON (exceptions).
    pub fn event_json(&self) -> Option<String> {
        let value: serde_json::Value = serde_json::from_slice(&self.payload).ok()?;
        match value.get("bytes").and_then(serde_json::Value::as_str) {
            Some(encoded) => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()?;
                String::from_utf8(decoded).ok()
            }
            None => String::from_utf8(Vec::clone(&self.payload)).ok(),
        }
    }
}

/// Incremental decoder: bytes in, whole frames out.
#[derive(Default)]
pub struct EventStreamDecoder {
    buffer: Vec<u8>,
}

impl EventStreamDecoder {
    /// A decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes and take every complete frame. An error means frame boundaries are lost, so the
    /// caller should fail the turn rather than resynchronise.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < PRELUDE_LEN {
                return Ok(frames);
            }
            let total_len = u32::from_be_bytes(self.buffer[0..4].try_into().expect("4 bytes"));
            let headers_len = u32::from_be_bytes(self.buffer[4..8].try_into().expect("4 bytes"));
            let total_len = total_len as usize;
            let headers_len = headers_len as usize;
            if total_len < PRELUDE_LEN + MESSAGE_CRC_LEN + headers_len {
                return Err(format!(
                    "event stream frame declares {total_len} bytes, too short"
                ));
            }
            if self.buffer.len() < total_len {
                return Ok(frames);
            }
            let frame = self.buffer.drain(..total_len).collect::<Vec<u8>>();
            frames.push(decode_frame(&frame, headers_len)?);
        }
    }
}

/// Decode one complete frame.
fn decode_frame(frame: &[u8], headers_len: usize) -> Result<Frame, String> {
    let headers = &frame[PRELUDE_LEN..PRELUDE_LEN + headers_len];
    let payload = &frame[PRELUDE_LEN + headers_len..frame.len() - MESSAGE_CRC_LEN];
    let mut event_type = None;
    let mut exception_type = None;
    for (name, value) in decode_headers(headers)? {
        match name.as_str() {
            ":event-type" => event_type = Some(value),
            ":exception-type" | ":error-code" => exception_type = Some(value),
            _ => {}
        }
    }
    Ok(Frame {
        event_type,
        exception_type,
        payload: payload.to_vec(),
    })
}

/// Decode the header block into name/value pairs, keeping only string-valued headers.
fn decode_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let name_len = *bytes.first().ok_or("truncated header name length")? as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len + 1 {
            return Err("truncated header name".to_string());
        }
        let name = String::from_utf8_lossy(&bytes[..name_len]).to_string();
        bytes = &bytes[name_len..];
        let value_type = bytes[0];
        bytes = &bytes[1..];
        if value_type != HEADER_TYPE_STRING {
            // Only string headers carry the metadata this decoder needs; every other type is
            // fixed width or length-prefixed, and skipping them safely needs their width.
            let width = match value_type {
                0 | 1 => 0, // boolean true / false
                2 => 1,     // byte
                3 => 2,     // short
                4 => 4,     // integer
                5 | 8 => 8, // long / timestamp
                6 => {
                    // byte array: length-prefixed like a string
                    if bytes.len() < 2 {
                        return Err("truncated header value".to_string());
                    }
                    let len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                    bytes = &bytes[2..];
                    len
                }
                9 => 16, // uuid
                other => return Err(format!("unknown event stream header type {other}")),
            };
            if bytes.len() < width {
                return Err("truncated header value".to_string());
            }
            bytes = &bytes[width..];
            continue;
        }
        if bytes.len() < 2 {
            return Err("truncated header value length".to_string());
        }
        let value_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        bytes = &bytes[2..];
        if bytes.len() < value_len {
            return Err("truncated header value".to_string());
        }
        let value = String::from_utf8_lossy(&bytes[..value_len]).to_string();
        bytes = &bytes[value_len..];
        headers.push((name, value));
    }
    Ok(headers)
}
