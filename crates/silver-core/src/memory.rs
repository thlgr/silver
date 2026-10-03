//! Scope-local Markdown memory, changed only by explicit add/replace/remove (INV-8). Entries are
//! whole ENTRY_DELIMITER blocks, never substrings. Injection-shaped content is refused on write and
//! shown as [BLOCKED: ...] in the prompt view.

use crate::error::{CoreError, CoreResult};
use sha2::{Digest, Sha256};
use silver_protocol::{MemoryFileKind, MemoryOpKind, Scope};
use std::collections::BTreeSet;

/// Char budget for the MEMORY.md store, matching Hermes MemoryStore's default.
pub const MEMORY_CHAR_LIMIT: usize = 2200;
/// Char budget for the USER.md store, matching Hermes MemoryStore's default.
pub const USER_CHAR_LIMIT: usize = 1375;

/// Entry separator used on disk and in the prompt, matching Hermes ENTRY_DELIMITER.
pub const ENTRY_DELIMITER: &str = "\n\u{00a7}\n";

/// Hard cap on scanned text; the scanner is advisory, so bound worst-case runtime.
pub const MAX_SCAN_CHARS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryFile {
    Memory,
    User,
}

impl MemoryFile {
    pub fn file_name(self) -> &'static str {
        match self {
            MemoryFile::Memory => "MEMORY.md",
            MemoryFile::User => "USER.md",
        }
    }

    pub fn kind(self) -> MemoryFileKind {
        match self {
            MemoryFile::Memory => MemoryFileKind::Memory,
            MemoryFile::User => MemoryFileKind::User,
        }
    }

    /// Char budget for this store's final rendered content.
    pub fn char_limit(self) -> usize {
        match self {
            MemoryFile::Memory => MEMORY_CHAR_LIMIT,
            MemoryFile::User => USER_CHAR_LIMIT,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MemoryOperation {
    Add {
        content: String,
    },
    Replace {
        old: String,
        new: String,
    },
    Remove {
        content: String,
    },
    /// Atomic whole-file replacement used by the batch path. The tool validates every
    /// operation and the final char budget on a working copy, then persists the result
    /// with a single store call, so a batch is all-or-nothing.
    SetContent {
        content: String,
    },
}

impl MemoryOperation {
    pub fn kind(&self) -> MemoryOpKind {
        match self {
            MemoryOperation::Add { .. } => MemoryOpKind::Add,
            MemoryOperation::Replace { .. } => MemoryOpKind::Replace,
            MemoryOperation::Remove { .. } => MemoryOpKind::Remove,
            MemoryOperation::SetContent { .. } => MemoryOpKind::Replace,
        }
    }
}

/// Why an operation failed. NoMatch and over-budget count against the tool's per-turn budget;
/// Invalid and Ambiguous end the call without doing so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryOpError {
    /// Malformed operation or content rejected by the injection scan.
    Invalid(String),
    /// No entry contained old_text: a consolidation failure.
    NoMatch(String),
    /// old_text matched multiple distinct entries: the caller must be more specific.
    Ambiguous(String),
}

impl MemoryOpError {
    /// The user-facing message.
    pub fn message(&self) -> &str {
        match self {
            MemoryOpError::Invalid(message)
            | MemoryOpError::NoMatch(message)
            | MemoryOpError::Ambiguous(message) => message,
        }
    }

    /// Whether the per-turn consolidation cap counts this failure.
    pub fn is_consolidation_failure(&self) -> bool {
        matches!(self, MemoryOpError::NoMatch(_))
    }
}

/// Entries split on the full delimiter, so a bare section sign survives. A store from before the
/// delimiter is split per line, so old memory is not collapsed into one entry.
pub fn parse_entries(raw: &str) -> Vec<String> {
    if raw.contains(ENTRY_DELIMITER) {
        return raw
            .split(ENTRY_DELIMITER)
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect();
    }
    raw.lines()
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// Join entries back into the canonical on-disk/prompt representation.
pub fn render_entries(entries: &[String]) -> String {
    entries.join(ENTRY_DELIMITER)
}

/// Order-preserving deduplication, matching Hermes list(dict.fromkeys(...)).
fn dedup_entries(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// Canonical, deduplicated live view of a raw store.
pub fn canonical_content(raw: &str) -> String {
    render_entries(&dedup_entries(parse_entries(raw)))
}

/// Prompt-safe view: parse, dedup, then replace poisoned entries with a [BLOCKED: ...]
/// placeholder. The raw on-disk text is deliberately left untouched so the user can still
/// see and remove the original entry.
pub fn prompt_safe_content(file: MemoryFile, raw: &str) -> String {
    let entries = dedup_entries(parse_entries(raw));
    let sanitized: Vec<String> = entries
        .into_iter()
        .map(|entry| {
            if entry.starts_with("[BLOCKED:") {
                return entry;
            }
            let findings = scan_for_threats(&entry);
            if findings.is_empty() {
                entry
            } else {
                format!(
                    "[BLOCKED: {file} entry contained threat pattern(s): {findings}. Removed from system prompt; use memory(action=remove) to delete the original.]",
                    file = file.file_name(),
                    findings = findings.join(", ")
                )
            }
        })
        .collect();
    render_entries(&sanitized)
}

/// Char count of a rendered entry list (delimiter included), matching Hermes _char_count.
/// An entry after text was cut out of it: no blank lines, no doubled or edge spaces.
fn tidy_entry(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn entries_char_count(entries: &[String]) -> usize {
    render_entries(entries).chars().count()
}

/// Length of the canonical rendered content. Trailing newlines and blank lines do not count
/// against the budget, matching Hermes's delimiter-joined entry list.
pub fn rendered_char_count(text: &str) -> usize {
    entries_char_count(&parse_entries(text))
}

/// Human-readable usage string for a store, for memory error payloads.
pub fn usage_for(file: MemoryFile, text: &str) -> String {
    format!("{}/{}", rendered_char_count(text), file.char_limit())
}

/// True when text already holds entry as a whole (trimmed) entry. Used to skip exact
/// duplicate adds (Hermes content in entries).
pub fn contains_entry(text: &str, entry: &str) -> bool {
    let needle = entry.trim();
    !needle.is_empty()
        && parse_entries(text)
            .iter()
            .any(|existing| existing == needle)
}

/// Reject content that would push a store past its per-target char budget. The check is
/// always against the FINAL rendered content, so a batch can free space and add entries
/// in the same call.
pub fn check_char_budget(file: MemoryFile, text: &str) -> CoreResult<()> {
    let limit = file.char_limit();
    let used = rendered_char_count(text);
    if used > limit {
        return Err(CoreError::InvalidRequest(format!(
            "{file} would be at {used} chars after this change, over the {limit} char limit; consolidate entries before retrying",
            file = file.file_name()
        )));
    }
    Ok(())
}

enum EntryMatch {
    Found(usize),
    Ambiguous,
    Missing,
}

/// Locate the unique entry containing old_text. Exact-duplicate matches are safe (first
/// wins); two distinct matching entries are ambiguous, matching Hermes _find_unique_match.
fn find_unique_match(entries: &[String], old_text: &str) -> EntryMatch {
    if old_text.is_empty() {
        return EntryMatch::Missing;
    }
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.contains(old_text))
        .map(|(index, _)| index)
        .collect();
    match matches.first() {
        None => EntryMatch::Missing,
        Some(&first) => {
            let candidate = &entries[first];
            if matches.iter().any(|&index| &entries[index] != candidate) {
                EntryMatch::Ambiguous
            } else {
                EntryMatch::Found(first)
            }
        }
    }
}

/// Apply one operation to the live entry list, returning a classified failure.
pub fn apply_operation_detailed(text: &str, op: &MemoryOperation) -> Result<String, MemoryOpError> {
    let mut entries = dedup_entries(parse_entries(text));
    match op {
        MemoryOperation::Add { content } => {
            let addition = content.trim();
            if addition.is_empty() {
                return Err(MemoryOpError::Invalid("Content cannot be empty.".into()));
            }
            if let Some(message) = scan_memory_content(addition) {
                return Err(MemoryOpError::Invalid(message));
            }
            if entries.iter().any(|entry| entry == addition) {
                return Ok(render_entries(&entries));
            }
            entries.push(addition.to_string());
            Ok(render_entries(&entries))
        }
        MemoryOperation::Replace { old, new } => {
            let old = old.trim();
            let new = new.trim();
            if old.is_empty() {
                return Err(MemoryOpError::Invalid("old_text cannot be empty.".into()));
            }
            if new.is_empty() {
                return Err(MemoryOpError::Invalid(
                    "content is required for 'replace' action. Use 'remove' to delete entries."
                        .into(),
                ));
            }
            if let Some(message) = scan_memory_content(new) {
                return Err(MemoryOpError::Invalid(message));
            }
            match find_unique_match(&entries, old) {
                EntryMatch::Ambiguous => Err(MemoryOpError::Ambiguous(format!(
                    "Multiple entries matched '{old}'. Be more specific."
                ))),
                EntryMatch::Missing => Err(MemoryOpError::NoMatch(format!(
                    "No entry matched '{old}'. Check current_entries below and retry with the exact text of the entry you want to replace."
                ))),
                EntryMatch::Found(index) => {
                    entries[index] = new.to_string();
                    Ok(render_entries(&entries))
                }
            }
        }
        MemoryOperation::Remove { content } => {
            let old = content.trim();
            if old.is_empty() {
                return Err(MemoryOpError::Invalid("old_text cannot be empty.".into()));
            }
            match find_unique_match(&entries, old) {
                EntryMatch::Ambiguous => Err(MemoryOpError::Ambiguous(format!(
                    "Multiple entries matched '{old}'. Be more specific."
                ))),
                EntryMatch::Missing => Err(MemoryOpError::NoMatch(format!(
                    "No entry matched '{old}'. Check current_entries below and retry with the exact text of the entry you want to remove."
                ))),
                EntryMatch::Found(index) => {
                    // A quoted sentence or line removes just that text: deleting the whole
                    // entry would also erase the other facts written into it. A keyword
                    // still removes the entry it names.
                    let entry = &entries[index];
                    let quoted = old.ends_with(['.', '!', '?'])
                        || entry.lines().any(|line| line.trim() == old);
                    let rest = tidy_entry(&entry.replacen(old, "", 1));
                    if quoted && !rest.is_empty() {
                        entries[index] = rest;
                    } else {
                        entries.remove(index);
                    }
                    Ok(render_entries(&entries))
                }
            }
        }
        MemoryOperation::SetContent { content } => Ok(canonical_content(content)),
    }
}

/// Apply one operation to the on-disk text, mapping failures to CoreError::InvalidRequest.
pub fn apply_operation(text: &str, op: &MemoryOperation) -> CoreResult<String> {
    apply_operation_detailed(text, op)
        .map_err(|error| CoreError::InvalidRequest(error.message().to_string()))
}

// ---------------------------------------------------------------------------
// Injection / exfiltration scan (Rust subset of Hermes tools/threat_patterns.py)
// ---------------------------------------------------------------------------

/// Invisible / bidirectional codepoints used in injection smuggling (aligned with Hermes
/// INVISIBLE_CHARS): zero-width and bidi controls.
const INVISIBLE_CHARS: [char; 17] = [
    '\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{2062}', '\u{2063}', '\u{2064}', '\u{feff}',
    '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}',
];

struct ThreatMatcher {
    id: &'static str,
    matches: fn(&ScanInput) -> bool,
}

/// Normalized view used by the pattern matchers: ASCII-lowercased content plus
/// punctuation-trimmed word tokens.
struct ScanInput {
    lower: String,
    words: Vec<String>,
}

impl ScanInput {
    fn new(content: &str) -> Self {
        let lower = content.to_ascii_lowercase();
        let words = lower
            .split_whitespace()
            .map(|word| {
                word.chars()
                    .filter(|ch| {
                        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '$' | '.' | '/')
                    })
                    .collect::<String>()
            })
            .filter(|word| !word.is_empty())
            .collect();
        Self { lower, words }
    }

    fn has(&self, needle: &str) -> bool {
        self.lower.contains(needle)
    }
}

/// Index at or after start (within span words) whose token satisfies predicate.
fn window_find(
    words: &[String],
    start: usize,
    span: usize,
    predicate: impl Fn(&str) -> bool,
) -> Option<usize> {
    let end = start.saturating_add(span).min(words.len());
    (start..end).find(|&index| predicate(&words[index]))
}

fn is_word_of(haystack: &str, needle: &str) -> bool {
    find_word(haystack, needle).is_some()
}

/// Byte offset of needle in haystack bounded by non-alphanumeric characters.
fn find_word(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(relative) = haystack[from..].find(needle) {
        let absolute = from + relative;
        let before_ok = absolute == 0 || !bytes[absolute - 1].is_ascii_alphanumeric();
        let after = absolute + needle.len();
        let after_ok = after >= haystack.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(absolute);
        }
        from = absolute + needle.len();
        if from >= haystack.len() {
            break;
        }
    }
    None
}

fn cap_chars(content: &str, max: usize) -> String {
    if content.chars().count() <= max {
        content.to_string()
    } else {
        content.chars().take(max).collect()
    }
}

fn looks_like_secret_var(token: &str) -> bool {
    let name = token
        .trim_start_matches('$')
        .trim_start_matches('{')
        .trim_end_matches('}');
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|suffix| upper.ends_with(suffix) || upper.ends_with(&format!("{suffix}S")))
}

fn command_uses_secret_var(lower: &str, command: &str) -> bool {
    for line in lower.split('\n') {
        let mut from = 0;
        while let Some(relative) = line[from..].find(command) {
            let absolute = from + relative;
            let end = (absolute + command.len() + 2048).min(line.len());
            let tail = &line[absolute + command.len()..end];
            if tail.split_whitespace().any(looks_like_secret_var) {
                return true;
            }
            from = absolute + command.len();
            if from >= line.len() {
                break;
            }
        }
    }
    false
}

fn command_reads_files(lower: &str, command: &str, files: &[&str]) -> bool {
    for line in lower.split('\n') {
        if let Some(absolute) = find_word(line, command) {
            let end = (absolute + 2048).min(line.len());
            let tail = &line[absolute..end];
            if files.iter().any(|file| tail.contains(file)) {
                return true;
            }
        }
    }
    false
}

fn sends_to_url(lower: &str) -> bool {
    for line in lower.split('\n') {
        for verb in ["send", "post", "upload", "transmit"] {
            if let Some(absolute) = find_word(line, verb) {
                let end = (absolute + 2048).min(line.len());
                let tail = &line[absolute..end];
                if [" to http://", " to https://", " at http://", " at https://"]
                    .iter()
                    .any(|marker| tail.contains(marker))
                {
                    return true;
                }
            }
        }
    }
    false
}

fn has_hardcoded_secret(lower: &str) -> bool {
    for key in [
        "api_key", "apikey", "api-key", "token", "secret", "password",
    ] {
        let mut from = 0;
        while let Some(relative) = lower[from..].find(key) {
            let absolute = from + relative;
            let rest = lower[absolute + key.len()..].trim_start();
            if let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) {
                let rest = rest.trim_start();
                for quote in ['"', '\''] {
                    if let Some(inner) = rest.strip_prefix(quote) {
                        let value: String = inner
                            .chars()
                            .take_while(|ch| {
                                ch.is_ascii_alphanumeric()
                                    || matches!(ch, '+' | '/' | '=' | '_' | '-')
                            })
                            .collect();
                        if value.len() >= 20 {
                            return true;
                        }
                    }
                }
            }
            from = absolute + key.len();
            if from >= lower.len() {
                break;
            }
        }
    }
    false
}

fn m_prompt_injection(input: &ScanInput) -> bool {
    for (index, word) in input.words.iter().enumerate() {
        if word != "ignore" {
            continue;
        }
        if let Some(qualifier) = window_find(&input.words, index + 1, 9, |word| {
            matches!(word, "previous" | "all" | "above" | "prior")
        }) {
            if window_find(&input.words, qualifier + 1, 9, |word| {
                word.starts_with("instruction")
            })
            .is_some()
            {
                return true;
            }
        }
    }
    false
}

fn m_sys_prompt_override(input: &ScanInput) -> bool {
    input.has("system prompt override")
}

fn m_disregard_rules(input: &ScanInput) -> bool {
    for (index, word) in input.words.iter().enumerate() {
        if word != "disregard" {
            continue;
        }
        if window_find(&input.words, index + 1, 9, |word| {
            word.starts_with("instruction")
                || word.starts_with("rule")
                || word.starts_with("guideline")
        })
        .is_some()
        {
            return true;
        }
    }
    false
}

fn m_bypass_restrictions(input: &ScanInput) -> bool {
    if !input.has("act as if") && !input.has("act as though") {
        return false;
    }
    for (index, word) in input.words.iter().enumerate() {
        if word != "act" {
            continue;
        }
        if let Some(negation) = window_find(&input.words, index, 14, |word| {
            matches!(word, "no" | "without" | "don't" | "dont")
        }) {
            if window_find(&input.words, negation, 14, |word| {
                word.starts_with("restriction")
                    || word.starts_with("limit")
                    || word.starts_with("rule")
            })
            .is_some()
            {
                return true;
            }
        }
    }
    false
}

fn m_html_comment_injection(input: &ScanInput) -> bool {
    input.has("<!--")
        && input.has("-->")
        && ["ignore", "override", "system", "secret", "hidden"]
            .iter()
            .any(|keyword| input.has(keyword))
}

fn m_hidden_div(input: &ScanInput) -> bool {
    input.has("<div") && (input.has("display:none") || input.has("display: none"))
}

fn m_translate_execute(input: &ScanInput) -> bool {
    input.has("translate")
        && input.has(" into ")
        && input
            .words
            .iter()
            .any(|word| matches!(word.as_str(), "execute" | "run" | "eval"))
}

fn m_deception_hide(input: &ScanInput) -> bool {
    for (index, word) in input.words.iter().enumerate() {
        let hides = word == "don't"
            || word == "dont"
            || (word == "do" && input.words.get(index + 1).is_some_and(|next| next == "not"));
        if !hides {
            continue;
        }
        if let Some(tell) = window_find(&input.words, index + 1, 10, |word| word == "tell") {
            if window_find(&input.words, tell + 1, 10, |word| word == "user").is_some() {
                return true;
            }
        }
    }
    false
}

fn m_role_hijack(input: &ScanInput) -> bool {
    for (index, word) in input.words.iter().enumerate() {
        if word != "you" {
            continue;
        }
        if input.words.get(index + 1).is_none_or(|next| next != "are") {
            continue;
        }
        if let Some(now) = window_find(&input.words, index + 2, 9, |word| word == "now") {
            if input
                .words
                .get(now + 1)
                .is_some_and(|word| matches!(word.as_str(), "a" | "an" | "the"))
            {
                return true;
            }
        }
    }
    false
}

fn m_role_pretend(input: &ScanInput) -> bool {
    for (index, word) in input.words.iter().enumerate() {
        if word != "pretend" {
            continue;
        }
        if let Some(you) = window_find(&input.words, index + 1, 9, |word| word == "you") {
            let next = input.words.get(you + 1).map(String::as_str);
            let to_be =
                next == Some("to") && input.words.get(you + 2).map(String::as_str) == Some("be");
            if next == Some("are") || to_be {
                return true;
            }
        }
    }
    false
}

fn m_leak_system_prompt(input: &ScanInput) -> bool {
    for verb in ["output", "print", "show", "reveal"] {
        for (index, word) in input.words.iter().enumerate() {
            if word != verb {
                continue;
            }
            if let Some(qualifier) = window_find(&input.words, index + 1, 9, |word| {
                matches!(word, "system" | "initial")
            }) {
                if input
                    .words
                    .get(qualifier + 1)
                    .is_some_and(|word| word == "prompt")
                {
                    return true;
                }
            }
        }
    }
    false
}

fn m_remove_filters(input: &ScanInput) -> bool {
    for verb in ["respond", "answer", "reply"] {
        for (index, word) in input.words.iter().enumerate() {
            if word != verb {
                continue;
            }
            if let Some(without) = window_find(&input.words, index + 1, 9, |word| word == "without")
            {
                if window_find(&input.words, without + 1, 9, |word| {
                    word.starts_with("restriction")
                        || word.starts_with("limitation")
                        || word.starts_with("filter")
                        || word.starts_with("safety")
                })
                .is_some()
                {
                    return true;
                }
            }
        }
    }
    false
}

fn m_identity_override(input: &ScanInput) -> bool {
    input.words.iter().enumerate().any(|(index, word)| {
        word == "name"
            && input
                .words
                .get(index + 1)
                .is_some_and(|next| next == "yourself")
    })
}

fn m_env_var_unset_agent(input: &ScanInput) -> bool {
    for (index, word) in input.words.iter().enumerate() {
        if word != "unset" {
            continue;
        }
        if let Some(target) = input.words.get(index + 1) {
            if ["claude", "codex", "hermes", "agent", "openai", "anthropic"]
                .iter()
                .any(|needle| target.contains(needle))
            {
                return true;
            }
        }
    }
    false
}

fn m_known_c2_framework(input: &ScanInput) -> bool {
    [
        "cobalt strike",
        "cobaltstrike",
        "sliver",
        "havoc",
        "mythic",
        "metasploit",
        "brainworm",
    ]
    .iter()
    .any(|needle| input.has(needle))
}

fn m_c2_explicit(input: &ScanInput) -> bool {
    [
        "c2 server",
        "c2 channel",
        "c2 infrastructure",
        "c2 beacon",
        "command and control",
    ]
    .iter()
    .any(|needle| input.has(needle))
}

fn m_c2_network_connect(input: &ScanInput) -> bool {
    input.has("connect to the network")
}

fn m_anti_forensic_oneliner(input: &ScanInput) -> bool {
    input.has("only use one-liner") || input.has("only use one liners")
}

fn m_exfil_curl(input: &ScanInput) -> bool {
    command_uses_secret_var(&input.lower, "curl")
}

fn m_exfil_wget(input: &ScanInput) -> bool {
    command_uses_secret_var(&input.lower, "wget")
}

fn m_read_secrets(input: &ScanInput) -> bool {
    command_reads_files(
        &input.lower,
        "cat",
        &[
            ".env",
            "credentials",
            ".netrc",
            ".pgpass",
            ".npmrc",
            ".pypirc",
        ],
    )
}

fn m_send_to_url(input: &ScanInput) -> bool {
    sends_to_url(&input.lower)
}

fn m_context_exfil(input: &ScanInput) -> bool {
    for verb in ["include", "output", "print", "share"] {
        if let Some(absolute) = find_word(&input.lower, verb) {
            let end = (absolute + 512).min(input.lower.len());
            let tail = &input.lower[absolute..end];
            if [
                "conversation",
                "chat history",
                "previous messages",
                "full context",
                "entire context",
            ]
            .iter()
            .any(|target| tail.contains(target))
            {
                return true;
            }
        }
    }
    false
}

fn m_ssh_backdoor(input: &ScanInput) -> bool {
    input.has("authorized_keys")
}

fn m_ssh_access(input: &ScanInput) -> bool {
    const VERBS: [&str; 22] = [
        "echo", "cat", "cp", "mv", "dd", "tee", "install", "printf", "rsync", "scp", "ln",
        "append", "add", "write", "sed", "chmod", "chown", "truncate", "rm", "touch", "curl",
        "wget",
    ];
    for line in input.lower.split('\n') {
        let touches_ssh = line.contains("$home/.ssh") || line.contains("~/.ssh");
        if touches_ssh && (VERBS.iter().any(|verb| is_word_of(line, verb)) || line.contains(">>")) {
            return true;
        }
    }
    false
}

fn m_hermes_env(input: &ScanInput) -> bool {
    input.has("$home/.hermes/.env") || input.has("~/.hermes/.env")
}

fn m_agent_config_mod(input: &ScanInput) -> bool {
    config_mod(
        input,
        &["agents.md", "claude.md", ".cursorrules", ".clinerules"],
    )
}

fn m_hermes_config_mod(input: &ScanInput) -> bool {
    config_mod(input, &[".hermes/config.yaml", "soul.md"])
}

fn config_mod(input: &ScanInput, files: &[&str]) -> bool {
    for line in input.lower.split('\n') {
        if [
            "update", "modify", "edit", "write", "change", "append", "add to",
        ]
        .iter()
        .any(|verb| line.contains(verb))
            && files.iter().any(|file| line.contains(file))
        {
            return true;
        }
    }
    false
}

fn m_hardcoded_secret(input: &ScanInput) -> bool {
    has_hardcoded_secret(&input.lower)
}

const THREAT_MATCHERS: &[ThreatMatcher] = &[
    ThreatMatcher {
        id: "prompt_injection",
        matches: m_prompt_injection,
    },
    ThreatMatcher {
        id: "sys_prompt_override",
        matches: m_sys_prompt_override,
    },
    ThreatMatcher {
        id: "disregard_rules",
        matches: m_disregard_rules,
    },
    ThreatMatcher {
        id: "bypass_restrictions",
        matches: m_bypass_restrictions,
    },
    ThreatMatcher {
        id: "html_comment_injection",
        matches: m_html_comment_injection,
    },
    ThreatMatcher {
        id: "hidden_div",
        matches: m_hidden_div,
    },
    ThreatMatcher {
        id: "translate_execute",
        matches: m_translate_execute,
    },
    ThreatMatcher {
        id: "deception_hide",
        matches: m_deception_hide,
    },
    ThreatMatcher {
        id: "role_hijack",
        matches: m_role_hijack,
    },
    ThreatMatcher {
        id: "role_pretend",
        matches: m_role_pretend,
    },
    ThreatMatcher {
        id: "leak_system_prompt",
        matches: m_leak_system_prompt,
    },
    ThreatMatcher {
        id: "remove_filters",
        matches: m_remove_filters,
    },
    ThreatMatcher {
        id: "identity_override",
        matches: m_identity_override,
    },
    ThreatMatcher {
        id: "env_var_unset_agent",
        matches: m_env_var_unset_agent,
    },
    ThreatMatcher {
        id: "known_c2_framework",
        matches: m_known_c2_framework,
    },
    ThreatMatcher {
        id: "c2_explicit",
        matches: m_c2_explicit,
    },
    ThreatMatcher {
        id: "c2_network_connect",
        matches: m_c2_network_connect,
    },
    ThreatMatcher {
        id: "anti_forensic_oneliner",
        matches: m_anti_forensic_oneliner,
    },
    ThreatMatcher {
        id: "exfil_curl",
        matches: m_exfil_curl,
    },
    ThreatMatcher {
        id: "exfil_wget",
        matches: m_exfil_wget,
    },
    ThreatMatcher {
        id: "read_secrets",
        matches: m_read_secrets,
    },
    ThreatMatcher {
        id: "send_to_url",
        matches: m_send_to_url,
    },
    ThreatMatcher {
        id: "context_exfil",
        matches: m_context_exfil,
    },
    ThreatMatcher {
        id: "ssh_backdoor",
        matches: m_ssh_backdoor,
    },
    ThreatMatcher {
        id: "ssh_access",
        matches: m_ssh_access,
    },
    ThreatMatcher {
        id: "hermes_env",
        matches: m_hermes_env,
    },
    ThreatMatcher {
        id: "agent_config_mod",
        matches: m_agent_config_mod,
    },
    ThreatMatcher {
        id: "hermes_config_mod",
        matches: m_hermes_config_mod,
    },
    ThreatMatcher {
        id: "hardcoded_secret",
        matches: m_hardcoded_secret,
    },
];

/// Matched threat pattern ids in content, in stable order. Invisible codepoints are
/// reported as invisible_unicode_U+XXXX.
pub fn scan_for_threats(content: &str) -> Vec<String> {
    let mut findings: Vec<String> = Vec::new();
    if content.is_empty() {
        return findings;
    }
    let capped = cap_chars(content, MAX_SCAN_CHARS);
    let mut seen_invisible: BTreeSet<char> = BTreeSet::new();
    for ch in capped.chars() {
        if INVISIBLE_CHARS.contains(&ch) && seen_invisible.insert(ch) {
            findings.push(format!("invisible_unicode_U+{:04X}", ch as u32));
        }
    }
    let input = ScanInput::new(&capped);
    for matcher in THREAT_MATCHERS {
        if (matcher.matches)(&input) {
            findings.push(matcher.id.to_string());
        }
    }
    findings
}

/// User-facing error for the first threat in content, or None when it is clean. Memory
/// enters the system prompt, so the scan is strict.
pub fn scan_memory_content(content: &str) -> Option<String> {
    let findings = scan_for_threats(content);
    let first = findings.first()?;
    if let Some(codepoint) = first.strip_prefix("invisible_unicode_") {
        return Some(format!(
            "Blocked: content contains invisible unicode character {codepoint} (possible injection)."
        ));
    }
    Some(format!(
        "Blocked: content matches threat pattern '{first}'. Content is injected into the system prompt and must not contain injection or exfiltration payloads."
    ))
}

/// Immutable per-run view of both memory files. The fields are the prompt-safe
/// representation when produced by MemoryStore::load; use load_raw for the live,
/// unsanitized entries the memory tool edits.
#[derive(Clone, Debug, Default)]
pub struct MemorySnapshot {
    pub memory: String,
    pub user: String,
}

impl MemorySnapshot {
    /// The text of one memory file in this snapshot.
    pub fn for_file(&self, file: MemoryFile) -> &str {
        match file {
            MemoryFile::Memory => &self.memory,
            MemoryFile::User => &self.user,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemoryRender {
    pub text: String,
    pub truncated: Vec<MemoryFile>,
}

impl MemorySnapshot {
    /// Render the snapshot for the prompt, truncating each file deterministically and
    /// preserving header lines. Returns the sections plus which files were truncated.
    pub fn render(&self, max_bytes_per_file: usize) -> MemoryRender {
        let (memory, memory_truncated) =
            truncate_preserving_headers(&self.memory, max_bytes_per_file);
        let (user, user_truncated) = truncate_preserving_headers(&self.user, max_bytes_per_file);
        let mut truncated = Vec::new();
        if memory_truncated {
            truncated.push(MemoryFile::Memory);
        }
        if user_truncated {
            truncated.push(MemoryFile::User);
        }
        let mut text = String::new();
        text.push_str("## USER\n");
        text.push_str(if user.trim().is_empty() {
            "(empty)\n"
        } else {
            &user
        });
        text.push_str("\n## MEMORY\n");
        text.push_str(if memory.trim().is_empty() {
            "(empty)\n"
        } else {
            &memory
        });
        MemoryRender { text, truncated }
    }
}

/// Truncate to a byte budget, keeping every header line (a line starting with '#').
/// Deterministic: headers keep their relative order, then non-header lines fill the rest.
pub fn truncate_preserving_headers(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![false; lines.len()];
    let mut used = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with('#') {
            keep[i] = true;
            used += line.len() + 1;
        }
    }
    if used < max_bytes {
        for (i, line) in lines.iter().enumerate() {
            if keep[i] {
                continue;
            }
            let cost = line.len() + 1;
            if used + cost > max_bytes {
                break;
            }
            keep[i] = true;
            used += cost;
        }
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("[memory truncated deterministically; edit the memory files to shrink them]\n");
    (out, true)
}

pub fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Clone, Debug)]
pub struct MemoryChange {
    pub file: MemoryFile,
    pub operation: MemoryOpKind,
    pub before_hash: String,
    pub after_hash: String,
}

/// Persistence boundary for Markdown memory. The daemon implements it over the data directory.
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// Prompt-safe snapshot: poisoned entries are quarantined with a [BLOCKED: ...]
    /// placeholder so they never reach the system prompt.
    async fn load(&self, scope: &Scope) -> CoreResult<MemorySnapshot>;

    /// Live, unsanitized snapshot for read-modify-write callers (the memory tool). Stores
    /// without a separate quarantine layer may use the default delegation to load.
    async fn load_raw(&self, scope: &Scope) -> CoreResult<MemorySnapshot> {
        self.load(scope).await
    }

    async fn apply(
        &self,
        scope: &Scope,
        file: MemoryFile,
        operation: &MemoryOperation,
    ) -> CoreResult<MemoryChange>;
}
