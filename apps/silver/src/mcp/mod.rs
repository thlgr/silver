//! Model Context Protocol client. MCP servers are untrusted: a stdio child gets a filtered
//! environment, descriptions are scanned for prompt injection, result text loses invisible
//! Unicode TAG characters, and every tool is Write/Process risk unless read-only is certain.

pub mod client;
pub mod tools;
pub mod transport;

pub use client::{McpManager, McpServerConnection};
pub use tools::{mcp_tool_name, render_call_tool_result, McpTool};

use std::collections::BTreeSet;

/// JSON-RPC 2.0 version string used on the wire.
pub const JSONRPC_VERSION: &str = "2.0";

/// clientInfo.name sent in initialize.
pub const CLIENT_NAME: &str = "silver";

/// Protocol version offered in initialize (mcp_tool.LATEST_PROTOCOL_VERSION).
pub const LATEST_PROTOCOL_VERSION: &str = "2025-03-26";
/// Version seeded into the mcp-protocol-version HTTP header; Hermes keeps these equal.
pub const LATEST_HANDSHAKE_VERSION: &str = LATEST_PROTOCOL_VERSION;

/// Per-server tool-call timeout fallback (mcp_tool_common._DEFAULT_TOOL_TIMEOUT).
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 300;
/// Initial connection timeout per server (mcp_tool._DEFAULT_CONNECT_TIMEOUT).
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 60;
/// Reconnect budget before a failing server is parked (mcp_tool._MAX_RECONNECT_RETRIES).
pub const MAX_RECONNECT_RETRIES: u32 = 5;
/// Retries for the very first connection attempt (mcp_tool._MAX_INITIAL_CONNECT_RETRIES).
pub const MAX_INITIAL_CONNECT_RETRIES: u32 = 3;
/// Ceiling for the reconnect backoff ladder (mcp_tool._MAX_BACKOFF_SECONDS).
pub const MAX_BACKOFF_SECONDS: u64 = 60;
/// Base of the per-server connect cooldown (mcp_tool._CONNECT_RETRY_BASE_BACKOFF_SEC).
pub const CONNECT_RETRY_BASE_BACKOFF_SECS: u64 = 30;
/// Cap of the per-server connect cooldown (mcp_tool._CONNECT_RETRY_MAX_BACKOFF_SEC).
pub const CONNECT_RETRY_MAX_BACKOFF_SECS: u64 = 600;
/// Reconnect jitter, +/-20% (mcp_tool_common._BACKOFF_JITTER).
pub const BACKOFF_JITTER: f64 = 0.2;

/// Wire-body cap applied before parsing a remote response (_MCP_HTTP_MAX_BODY_BYTES).
pub const MCP_HTTP_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Hard ceiling for one MCP text payload (_MCP_HARD_RESULT_CAP_CHARS).
pub const MCP_HARD_RESULT_CAP_CHARS: usize = 2_000_000;
/// Cap on decoded resource bytes per block (_MCP_RESOURCE_MAX_BYTES); media caching is out of
/// scope for this port, but the threshold is preserved for the drop notices.
pub const MCP_RESOURCE_MAX_BYTES: usize = 50 * 1024 * 1024;
/// Pagination cap for tools/list (_MCP_LIST_MAX_PAGES).
pub const MCP_LIST_MAX_PAGES: usize = 50;
/// Body head kept from an HTTP rejection for error reporting (_HTTP_REJECTION_BODY_CHARS).
pub const HTTP_REJECTION_BODY_CHARS: usize = 300;

/// JSON-RPC "method not found" (_JSONRPC_METHOD_NOT_FOUND); ping is optional in MCP.
pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
/// Stateless (2026-07-28) servers reject a legacy initialize with this code.
pub const JSONRPC_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
/// Opaque SDK-mapped server error (_is_streamable_http_rejection).
pub const JSONRPC_SERVER_ERROR: i64 = -32603;

/// Provider function-name limit: ^[a-zA-Z0-9_-]{1,64} (_MCP_TOOL_NAME_MAX_LENGTH).
pub const MCP_TOOL_NAME_MAX_LENGTH: usize = 64;
/// Hash suffix length used when clamping an over-long tool name (_MCP_TOOL_NAME_HASH_LENGTH).
pub const MCP_TOOL_NAME_HASH_LENGTH: usize = 8;

/// Environment variables safe to pass to a stdio child (_SAFE_ENV_KEYS). No secrets.
pub const SAFE_ENV_KEYS: [&str; 8] = [
    "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "SHELL", "TMPDIR",
];

/// Windows process/location vars needed by launcher-style tools
/// (_SAFE_ENV_KEYS_CASE_INSENSITIVE).
pub const SAFE_ENV_KEYS_CASE_INSENSITIVE: [&str; 27] = [
    "ALLUSERSPROFILE",
    "APPDATA",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "COMPUTERNAME",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PUBLIC",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERDOMAIN",
    "USERNAME",
    "USERPROFILE",
    "WINDIR",
];

/// Head/tail truncation ratio (tool_output_truncate.HEAD_RATIO).
pub const HEAD_RATIO: f64 = 0.4;

/// Errors raised by the MCP client. Every variant carries the server name so a status surface
/// can attribute the failure.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A remote server's url is not a parseable http(s):// URL (InvalidMcpUrlError).
    #[error("invalid MCP URL for '{server}': {detail}")]
    InvalidUrl { server: String, detail: String },
    /// Transport, IO or HTTP failure talking to the server.
    #[error("MCP transport error for '{server}': {detail}")]
    Transport { server: String, detail: String },
    /// The server answered with a JSON-RPC error object.
    #[error("MCP server '{server}' returned JSON-RPC error {code}: {message}")]
    Rpc {
        server: String,
        code: i64,
        message: String,
    },
    /// The server is not connected and the reconnect cooldown has not elapsed.
    #[error("MCP server '{server}' is not connected (connect is backing off)")]
    NotConnected { server: String },
    /// A malformed or unexpected protocol message.
    #[error("MCP protocol error for '{server}': {detail}")]
    Protocol { server: String, detail: String },
}

impl McpError {
    /// Convenience constructor for transport errors.
    pub fn transport(server: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Transport {
            server: server.into(),
            detail: detail.into(),
        }
    }

    /// Convenience constructor for protocol errors.
    pub fn protocol(server: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Protocol {
            server: server.into(),
            detail: detail.into(),
        }
    }

    /// The human-facing, credential-redacted message.
    pub fn message(&self) -> String {
        sanitize_error_text(&self.to_string())
    }
}

/// Credential patterns stripped from error text before it reaches the model
/// (mcp_tool_common._CREDENTIAL_PATTERN): GitHub PAT, OpenAI-style key, Bearer token and
/// token=/key=/API_KEY=/password=/secret= assignments.
pub fn sanitize_error_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let lower: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    collect_runs(bytes, &lower, b"ghp_", &mut ranges, |b| {
        b.is_ascii_alphanumeric() || b == b'_'
    });
    collect_runs(bytes, &lower, b"sk-", &mut ranges, |b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
    });
    for (start, end) in find_all(&lower, b"bearer") {
        let mut i = end;
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        if i == end {
            continue;
        }
        let value_start = i;
        let mut value_end = i;
        while value_end < bytes.len() && !bytes[value_end].is_ascii_whitespace() {
            value_end += 1;
        }
        if value_end > value_start {
            ranges.push((start, value_end));
        }
    }
    for needle in [
        b"token=".as_slice(),
        b"key=".as_slice(),
        b"api_key=".as_slice(),
        b"password=".as_slice(),
        b"secret=".as_slice(),
    ] {
        for (start, end) in find_all(&lower, needle) {
            let mut value_end = end;
            while value_end < bytes.len()
                && value_end - end < 255
                && !is_credential_delimiter(bytes[value_end])
            {
                value_end += 1;
            }
            if value_end > end {
                ranges.push((start, value_end));
            }
        }
    }

    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(text.len() + 16);
    let mut cursor = 0;
    for (start, end) in ranges {
        if start < cursor {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str("[REDACTED]");
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn is_credential_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b'&' | b',' | b';' | b'"' | 0x27u8)
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return out;
    }
    for i in 0..=haystack.len() - needle.len() {
        if &haystack[i..i + needle.len()] == needle {
            out.push((i, i + needle.len()));
        }
    }
    out
}

fn collect_runs(
    bytes: &[u8],
    lower: &[u8],
    prefix: &[u8],
    out: &mut Vec<(usize, usize)>,
    allowed: impl Fn(u8) -> bool,
) {
    for (start, end) in find_all(lower, prefix) {
        let mut value_end = end;
        while value_end < bytes.len() && value_end - end < 255 && allowed(bytes[value_end]) {
            value_end += 1;
        }
        if value_end > end {
            out.push((start, value_end));
        }
    }
}

/// Remove invisible Unicode TAG characters (U+E0000-U+E007F), preserving valid TR51 emoji tag
/// sequences. Ported from ansi_strip.strip_unicode_tags (block/goose#10746): tag chars render as
/// nothing but tokenizers see them, the classic ASCII-smuggling injection channel.
pub fn strip_unicode_tags(text: &str) -> String {
    const TAG_START: u32 = 0xE0000;
    const TAG_END: u32 = 0xE007F;
    const FLAG_BASE: u32 = 0x1F3F4;
    const TAG_SPEC_START: u32 = 0xE0020;
    const TAG_SPEC_END: u32 = 0xE007E;
    const CANCEL_TAG: u32 = 0xE007F;

    if !text.chars().any(|c| {
        let v = c as u32;
        (TAG_START..=TAG_END).contains(&v)
    }) {
        return text.to_string();
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] as u32 == FLAG_BASE {
            let mut j = i + 1;
            while j < chars.len() {
                let v = chars[j] as u32;
                if (TAG_SPEC_START..=TAG_SPEC_END).contains(&v) {
                    j += 1;
                    continue;
                }
                break;
            }
            if j > i + 1 && j < chars.len() && chars[j] as u32 == CANCEL_TAG {
                out.extend(&chars[i..=j]);
                i = j + 1;
                continue;
            }
        }
        let v = chars[i] as u32;
        if (TAG_START..=TAG_END).contains(&v) {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Head/tail truncation with the shared Hermes notice shape: 40% head, 60% tail around a
/// "... [LABEL TRUNCATED - N chars omitted out of T total] ..." marker.
pub fn truncate_head_tail(text: &str, max_chars: usize, label: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head = (max_chars as f64 * HEAD_RATIO) as usize;
    let tail = max_chars - head;
    let total = text.chars().count();
    let omitted = total - head - tail;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(total - tail).collect();
    format!(
        "{head_text}\n\n... [{label} TRUNCATED - {omitted} chars omitted out of {total} total] ...\n\n{tail_text}"
    )
}

/// type/subtype of a MIME string, lower-cased, parameters dropped (_base_mime).
pub fn base_mime(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Matches an MCP tool name against an include/exclude pattern set: exact names literally,
/// entries with * / ? / [ as case-sensitive globs (matches_name_filter).
pub fn matches_name_filter(tool_name: &str, patterns: &BTreeSet<String>) -> bool {
    if patterns.is_empty() {
        return false;
    }
    if patterns.contains(tool_name) {
        return true;
    }
    patterns
        .iter()
        .filter(|p| p.contains('*') || p.contains('?') || p.contains('['))
        .any(|p| glob_match(p, tool_name))
}

/// A bounded, backtracking fnmatch-style glob for *, ?, [set] and [!set].
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_here(&pat, &txt)
}

fn glob_here(pat: &[char], txt: &[char]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while t < txt.len() {
        if p < pat.len() {
            match pat[p] {
                '*' => {
                    star_p = Some(p);
                    star_t = t;
                    p += 1;
                    continue;
                }
                '?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                '[' => {
                    if let Some((matched, next_p)) = match_class(pat, p, txt[t]) {
                        if matched {
                            p = next_p;
                            t += 1;
                            continue;
                        }
                    }
                }
                c if c == txt[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_p {
            star_t += 1;
            t = star_t;
            p = sp + 1;
            continue;
        }
        return false;
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// Evaluate one [...] class starting at pat[start]; returns (matched, index_after_class).
fn match_class(pat: &[char], start: usize, ch: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if i < pat.len() && (pat[i] == '!' || pat[i] == '^') {
        negated = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == ']' && !first {
            return Some((matched != negated, i + 1));
        }
        first = false;
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            if pat[i] <= ch && ch <= pat[i + 2] {
                matched = true;
            }
            i += 3;
            continue;
        }
        if pat[i] == ch {
            matched = true;
        }
        i += 1;
    }
    None
}
