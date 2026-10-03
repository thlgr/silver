//! Model metadata: static context windows, a rough token estimate, and context limits recovered
//! from provider error messages.

use crate::model::ModelMessage;
use serde::{Deserialize, Serialize};
use silver_protocol::ContentPart;
use std::collections::BTreeMap;
use std::path::Path;

/// Rough characters per token for ASCII-ish prose.
const CHARS_PER_TOKEN: usize = 4;

/// Flat cost of one inline image. Providers bill by pixel tile, so a ~1.4 megapixel picture
/// lands near this; counting the base64 instead would bill 1 MB of pixels as 250k tokens and
/// make a run compact itself to nothing.
pub const IMAGE_TOKENS: usize = 1_100;

/// [IMAGE_TOKENS] in the byte unit `compact` compares against.
pub const IMAGE_ESTIMATE_BYTES: usize = IMAGE_TOKENS * CHARS_PER_TOKEN;

/// Static context-window floor for well-known model families, in tokens.
///
/// Matched case-insensitively as a substring of the model id, longest prefix first.
pub fn context_length_for(model: &str) -> Option<usize> {
    let lower = model.to_ascii_lowercase();
    let table: &[(&str, usize)] = &[
        ("gpt-4.1", 1_047_576),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4", 8_192),
        ("gpt-3.5", 16_385),
        ("o1", 200_000),
        ("o3", 200_000),
        ("o4", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-opus-4", 200_000),
        ("claude-3-7", 200_000),
        ("claude-3-5", 200_000),
        ("claude-3", 200_000),
        ("claude-2", 100_000),
        ("gemini-2.5", 1_048_576),
        ("gemini-2.0", 1_048_576),
        ("gemini-1.5", 1_048_576),
        ("deepseek-reasoner", 131_072),
        ("deepseek", 65_536),
        ("qwen", 131_072),
        ("llama-3", 131_072),
        ("llama", 8_192),
        ("mistral", 32_768),
        ("mixtral", 32_768),
        ("command-r", 131_072),
        ("grok", 131_072),
    ];
    table
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, length)| *length)
}

/// File name of the disk cache the daemon writes after a `/models` context probe.
pub const MODELS_CACHE_FILE: &str = "models_cache.json";

/// A sane context window: at least a few hundred tokens and below ten million.
fn sane_context_length(value: usize) -> Option<usize> {
    (256..=10_000_000).contains(&value).then_some(value)
}

/// On-disk `/v1/models` context cache: exact model id -> context window in tokens.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelsCache {
    /// Exact model id -> context window in tokens.
    #[serde(default)]
    pub models: BTreeMap<String, usize>,
}

impl ModelsCache {
    /// Load the cache, returning an empty one on a missing or unreadable file.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Best-effort atomic write of the cache, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temp = path.with_file_name(format!(".{MODELS_CACHE_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&temp, text)?;
        if let Err(error) = std::fs::rename(&temp, path) {
            drop(std::fs::remove_file(&temp));
            return Err(error);
        }
        Ok(())
    }

    /// The cached window for an exact model id, ignoring zero/absurd entries.
    pub fn get(&self, model: &str) -> Option<usize> {
        self.models
            .get(model)
            .copied()
            .and_then(sane_context_length)
    }

    /// Record the window for a model id.
    pub fn insert(&mut self, model: &str, context_length: usize) {
        self.models.insert(model.to_string(), context_length);
    }
}

/// Read `<data_dir>/models_cache.json` and return the cached window for `model`.
pub fn cached_context_length(data_dir: &Path, model: &str) -> Option<usize> {
    ModelsCache::load(&data_dir.join(MODELS_CACHE_FILE)).get(model)
}

/// Config override (zero ignored), then a cached or probed value, then the static table; the caller
/// defaults when all miss.
pub fn resolve_context_length(
    config_override: Option<usize>,
    learned: Option<usize>,
    model: &str,
) -> Option<usize> {
    config_override
        .filter(|value| *value > 0)
        .or_else(|| learned.filter(|value| *value > 0))
        .or_else(|| context_length_for(model))
}

/// Extract the context window for the exact `model` id from a `/v1/models` JSON body.
pub fn parse_models_context_length(body: &str, model: &str) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)?;
    let entry = entries
        .iter()
        .find(|entry| entry_id_matches(entry, model))?;
    context_from_entry(entry)
}

/// Whether a `/models` entry carries the requested model id.
fn entry_id_matches(entry: &serde_json::Value, model: &str) -> bool {
    ["id", "model", "name"].iter().any(|key| {
        entry
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|candidate| candidate == model || candidate.eq_ignore_ascii_case(model))
    })
}

/// The first sane context window reported on a model entry.
fn context_from_entry(entry: &serde_json::Value) -> Option<usize> {
    // `loaded_context_length` (LM Studio) is the window the model is actually running with,
    // which can be far below its maximum; it must win when present. `max_model_len` is vLLM.
    for key in [
        "loaded_context_length",
        "context_length",
        "max_context_length",
        "context_window",
        "max_model_len",
    ] {
        if let Some(value) = entry.get(key).and_then(context_int) {
            return Some(value);
        }
    }
    let limit = entry.get("limit")?;
    [
        "context",
        "context_length",
        "max_context_length",
        "context_window",
    ]
    .iter()
    .find_map(|key| limit.get(*key).and_then(context_int))
}

/// Coerce a JSON number or numeric string to a sane context window.
fn context_int(value: &serde_json::Value) -> Option<usize> {
    let parsed = match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .map(|value| value as usize)
            .or_else(|| number.as_f64().map(|value| value as usize)),
        serde_json::Value::String(text) => text.trim().replace(',', "").parse().ok(),
        _ => None,
    }?;
    sane_context_length(parsed)
}

/// Conservative token estimate; wide characters (CJK) count two tokens each.
pub fn estimate_tokens_rough(text: &str) -> usize {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for character in text.chars() {
        if character.is_ascii() {
            narrow += 1;
        } else if character.is_whitespace() {
            // Whitespace is cheap in either script.
        } else if (character as u32) >= 0x1100 {
            wide += 1;
        } else {
            narrow += 1;
        }
    }
    if narrow == 0 && wide == 0 {
        return 0;
    }
    let from_narrow = narrow.div_ceil(CHARS_PER_TOKEN);
    // A CJK character is roughly one token on its own.
    (from_narrow + wide).max(1)
}

/// Estimate the token cost of a message list plus a fixed per-message overhead.
pub fn estimate_messages_tokens_rough(messages: &[ModelMessage]) -> usize {
    let mut total = 0usize;
    for message in messages {
        total += 4;
        for part in &message.content {
            total += match part {
                ContentPart::Text { text } | ContentPart::Reasoning { text } => {
                    estimate_tokens_rough(text)
                }
                ContentPart::ToolCall {
                    name, arguments, ..
                } => estimate_tokens_rough(name) + estimate_tokens_rough(&arguments.to_string()),
                ContentPart::ToolResult { content, .. } => estimate_tokens_rough(content),
                ContentPart::Image { .. } => IMAGE_TOKENS,
                ContentPart::Attachment { name, path } => {
                    estimate_tokens_rough(name) + estimate_tokens_rough(path)
                }
            };
        }
    }
    total
}

/// A context limit quoted in an error: "maximum context length is 131072 tokens", "context length
/// of 32768", "max_tokens: 8192", "limit: 200000 tokens".
pub fn parse_context_limit_from_error(message: &str) -> Option<usize> {
    let lower = message.to_ascii_lowercase();
    for marker in [
        "context length",
        "context window",
        "maximum context",
        "max context",
        "context size",
    ] {
        if let Some(index) = lower.find(marker) {
            if let Some(value) = first_number(&lower[index + marker.len()..]) {
                return Some(value);
            }
        }
    }
    if let Some(index) = lower.find("tokens") {
        // Look backwards for the nearest number before "tokens" (e.g. "of 32768 tokens").
        if let Some(value) = last_number(&lower[..index]) {
            return Some(value);
        }
    }
    None
}

/// The first decimal integer in a string, if any. Rejects absurd values.
fn first_number(text: &str) -> Option<usize> {
    let digits: String = text
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    sane_number(&digits)
}

/// The last decimal integer in a string, if any. Rejects absurd values.
fn last_number(text: &str) -> Option<usize> {
    let digits: String = text
        .chars()
        .rev()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    sane_number(&digits)
}

fn sane_number(digits: &str) -> Option<usize> {
    let value: usize = digits.parse().ok()?;
    // A real context window is at least a few hundred tokens and below ten million.
    if (256..=10_000_000).contains(&value) {
        Some(value)
    } else {
        None
    }
}
