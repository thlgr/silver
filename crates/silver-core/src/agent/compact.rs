//! Context compaction: clear old tool results, drop a span of older turns, and note what went in
//! the system message, optionally with an auxiliary model's summary. The persisted transcript is
//! never touched, so later requests only append and a local server keeps its prompt cache.

use crate::model::ModelMessage;
use crate::model_metadata::IMAGE_ESTIMATE_BYTES;
use silver_protocol::{ContentPart, MessageRole};
use std::ops::Range;
use std::time::Duration;

/// Placeholder that replaces a pruned tool result. The tool-call pairing is preserved.
pub const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool output cleared to save context space]";

/// Marker noting a tool result was truncated to fit the context window.
pub const TRUNCATED_TOOL_MARKER: &str = "\n...[tool output truncated to fit context]...\n";

/// Minimum bytes retained per tool result when truncating to fit. Below this the
/// content is replaced wholesale rather than head+tail sliced.
pub const MIN_TOOL_RESULT_BYTES: usize = 500;

/// System prompt for the auxiliary summarizer that replaces the deterministic compaction note.
pub const SUMMARY_SYSTEM_PROMPT: &str = "Summarize the following conversation span into a compact structured summary preserving decisions, file paths, commands, errors and open threads; do not invent";

/// Maximum bytes of a dropped span sent to the summary model. The input is head+tail bounded.
pub const SUMMARY_INPUT_MAX_BYTES: usize = 64 * 1024;

/// Output token budget for one summary call.
pub const SUMMARY_MAX_TOKENS: u32 = 1024;

/// Longest summary text retained from the model, in characters.
pub const SUMMARY_MAX_CHARS: usize = 16_000;

/// Wall-clock bound on one summary model call.
pub const SUMMARY_TIMEOUT: Duration = Duration::from_secs(30);

/// Heading of the synthetic compaction note appended to the system message.
pub const SUMMARY_HEADING: &str = "[CONTEXT SUMMARY]";

/// Result of one compaction pass.
#[derive(Clone, Debug)]
pub struct CompactionOutcome {
    /// The request messages to send. Equal to the input when nothing was compacted.
    pub messages: Vec<ModelMessage>,
    /// How many tool results were replaced with the placeholder.
    pub pruned_tool_results: usize,
    /// How many older messages were summarized away.
    pub dropped_messages: usize,
    /// How many (recent, otherwise protected) tool results were truncated to fit.
    pub truncated_tool_results: usize,
    /// Half-open range of the input messages removed as the dropped span, when any were.
    /// The turn loop uses it to summarize the span before falling back to the deterministic note.
    pub dropped_span: Option<Range<usize>>,
    /// The note appended to the system message, when one was.
    pub summary: Option<String>,
    /// Whether anything changed.
    pub compacted: bool,
}

impl CompactionOutcome {
    /// A short status note, or None when the context was already small enough.
    pub fn note(&self) -> Option<String> {
        if !self.compacted {
            return None;
        }
        Some(format!(
            "context compacted: {} tool result(s) cleared, {} message(s) summarized, {} truncated",
            self.pruned_tool_results, self.dropped_messages, self.truncated_tool_results
        ))
    }
}

/// Cheap, deterministic, conservative byte estimate of the request messages.
pub fn estimate_bytes(messages: &[ModelMessage]) -> usize {
    let mut total = 0usize;
    for message in messages {
        total += 16;
        for part in &message.content {
            total += match part {
                ContentPart::Text { text } | ContentPart::Reasoning { text } => text.len(),
                ContentPart::ToolCall {
                    name, arguments, ..
                } => name.len() + arguments.to_string().len(),
                ContentPart::ToolResult { content, .. } => content.len(),
                ContentPart::Image { .. } => IMAGE_ESTIMATE_BYTES,
                ContentPart::Attachment { name, path } => name.len() + path.len(),
            };
        }
    }
    total
}

/// Compact once the estimated size reaches the threshold (0 disables), keeping recent tool results
/// and user turns while they fit; the newest user turn always stays.
pub fn compact(
    messages: &[ModelMessage],
    threshold_bytes: usize,
    keep_recent_tool_results: usize,
    keep_recent_turns: usize,
) -> CompactionOutcome {
    compact_with_summary(
        messages,
        threshold_bytes,
        keep_recent_tool_results,
        keep_recent_turns,
        None,
        None,
    )
}

/// [compact] with `model_summary` in place of the deterministic note; the dropped span is the same
/// either way. Tool results the model has not read yet are never cleared, only truncated if they
/// alone overflow.
pub fn compact_with_summary(
    messages: &[ModelMessage],
    threshold_bytes: usize,
    keep_recent_tool_results: usize,
    keep_recent_turns: usize,
    model_summary: Option<&str>,
    open_todos: Option<&str>,
) -> CompactionOutcome {
    if threshold_bytes == 0 || estimate_bytes(messages) < threshold_bytes {
        return CompactionOutcome {
            messages: messages.to_vec(),
            pruned_tool_results: 0,
            dropped_messages: 0,
            truncated_tool_results: 0,
            dropped_span: None,
            summary: None,
            compacted: false,
        };
    }

    // Free a quarter of the budget, not just enough to fit: the requests that follow extend
    // this one until the next compaction, so a server keeps its prompt cache meanwhile.
    let target = threshold_bytes / 4 * 3;
    let unread = messages
        .iter()
        .rev()
        .take_while(|message| message.role != MessageRole::Assistant)
        .filter(|message| message.role == MessageRole::Tool)
        .count();
    let mut working = messages.to_vec();
    let mut keep_tools = keep_recent_tool_results.max(unread);
    let mut pruned = prune_tool_results(&mut working, keep_tools);
    // In a small window the recent tool results alone can fill the budget: clear those too,
    // oldest first, down to the ones the model has not read. A tool can be run again; a
    // dropped turn can take the task itself with it.
    while keep_tools > unread && estimate_bytes(&working) >= target {
        keep_tools -= 1;
        pruned += prune_tool_results(&mut working, keep_tools);
    }
    let mut dropped = 0usize;
    let mut dropped_span: Option<Range<usize>> = None;
    let total = estimate_bytes(&working);
    if total >= threshold_bytes {
        // Keep fewer recent turns when they alone overflow; otherwise a session of long
        // turns fails every run, however small the next message. Once turns must go, as
        // many go as free the same quarter, so the next request does not drop another.
        let fits = |span: &Option<Range<usize>>| {
            span.as_ref()
                .map_or(0, |span| estimate_bytes(&working[span.start..span.end]))
                + target
                > total
        };
        let mut keep = keep_recent_turns.max(1);
        dropped_span = span_to_drop(&working, keep);
        while keep > 1 && !fits(&dropped_span) {
            keep -= 1;
            dropped_span = span_to_drop(&working, keep);
        }
        if let Some(span) = &dropped_span {
            dropped = span.len();
            working.drain(span.start..span.end);
        }
    }
    // Even unread results must fit, or the provider rejects the request again after the overflow
    // retry halves the threshold. The note goes in before truncating so the total still fits; a
    // summary is capped at a quarter of the budget and the todo list at an eighth.
    let summary = (pruned > 0 || dropped > 0).then(|| {
        let model_summary = model_summary
            .map(|summary| &summary[..floor_char_boundary(summary, threshold_bytes / 4)]);
        let open_todos =
            open_todos.map(|todos| &todos[..floor_char_boundary(todos, threshold_bytes / 8)]);
        annotate_summary(&mut working, model_summary, open_todos)
    });
    let truncated = if estimate_bytes(&working) >= threshold_bytes {
        truncate_tool_results_to_fit(&mut working, threshold_bytes)
    } else {
        0
    };
    let compacted = pruned > 0 || dropped > 0 || truncated > 0;
    CompactionOutcome {
        messages: working,
        pruned_tool_results: pruned,
        dropped_messages: dropped,
        truncated_tool_results: truncated,
        dropped_span,
        summary,
        compacted,
    }
}

/// Replace every tool result older than the most recent keep_recent with the placeholder.
fn prune_tool_results(messages: &mut [ModelMessage], keep_recent: usize) -> usize {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == MessageRole::Tool)
        .map(|(index, _)| index)
        .collect();
    if tool_indices.len() <= keep_recent {
        return 0;
    }
    let prune_before = tool_indices.len() - keep_recent;
    let mut pruned = 0;
    for &index in &tool_indices[..prune_before] {
        for part in &mut messages[index].content {
            if let ContentPart::ToolResult { content, .. } = part {
                if content != PRUNED_TOOL_PLACEHOLDER {
                    *content = PRUNED_TOOL_PLACEHOLDER.to_string();
                    pruned += 1;
                }
            }
        }
    }
    pruned
}

/// Shorten tool results, even recent ones, until under budget; returns how many. If the newest user
/// turn alone is too large, the provider's context error ends the turn.
fn truncate_tool_results_to_fit(messages: &mut [ModelMessage], threshold_bytes: usize) -> usize {
    let mut truncated = 0usize;
    // Oldest first for stability: the most recent evidence stays verbatim longest.
    loop {
        if estimate_bytes(messages) < threshold_bytes {
            break;
        }
        // Largest remaining truncatable tool result.
        let mut best: Option<(usize, usize, usize)> = None; // (msg_idx, part_idx, len)
        for (msg_idx, message) in messages.iter().enumerate() {
            if message.role != MessageRole::Tool {
                continue;
            }
            for (part_idx, part) in message.content.iter().enumerate() {
                if let ContentPart::ToolResult { content, .. } = part {
                    if content == PRUNED_TOOL_PLACEHOLDER {
                        continue;
                    }
                    if content.contains(TRUNCATED_TOOL_MARKER)
                        && content.len() <= MIN_TOOL_RESULT_BYTES + TRUNCATED_TOOL_MARKER.len()
                    {
                        continue;
                    }
                    let len = content.len();
                    if len > MIN_TOOL_RESULT_BYTES
                        && best.is_none_or(|(_, _, best_len)| len > best_len)
                    {
                        best = Some((msg_idx, part_idx, len));
                    }
                }
            }
        }
        let Some((msg_idx, part_idx, len)) = best else {
            break;
        };
        let excess = estimate_bytes(messages)
            .saturating_sub(threshold_bytes)
            .saturating_add(1);
        // Shrink this result enough to fit when possible, else halve it; never
        // below the minimum (placeholder swap happens on the next pass via prune
        // semantics, but here we keep pairing and just floor the text).
        let target = len.saturating_sub(excess).max(MIN_TOOL_RESULT_BYTES);
        let new_len = target.min(len.saturating_sub(1).max(MIN_TOOL_RESULT_BYTES));
        if let ContentPart::ToolResult { content, .. } = &mut messages[msg_idx].content[part_idx] {
            *content = truncate_tool_text(content, new_len);
            truncated += 1;
        } else {
            break;
        }
        // Safety: never loop forever on pathological single-message histories.
        if truncated > 256 {
            break;
        }
    }
    truncated
}

/// Head+tail slice of a tool result to `max_bytes`, with an explicit marker.
fn truncate_tool_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes <= MIN_TOOL_RESULT_BYTES {
        let mut out = String::with_capacity(max_bytes + TRUNCATED_TOOL_MARKER.len());
        let end = floor_char_boundary(text, max_bytes);
        out.push_str(&text[..end]);
        out.push_str(TRUNCATED_TOOL_MARKER);
        return out;
    }
    let remaining = max_bytes.saturating_sub(TRUNCATED_TOOL_MARKER.len());
    let head = floor_char_boundary(text, remaining * 3 / 4);
    let tail_start = ceil_char_boundary(
        text,
        text.len().saturating_sub(remaining - remaining * 3 / 4),
    );
    let mut out = String::with_capacity(max_bytes + TRUNCATED_TOOL_MARKER.len());
    out.push_str(&text[..head]);
    out.push_str(TRUNCATED_TOOL_MARKER);
    out.push_str(&text[tail_start..]);
    out
}

/// The range dropping old turns would remove: after the system message, up to the
/// keep_recent_turns-th newest user turn, extended past leading tool results so the kept region
/// never starts with one. None when nothing can go.
pub fn span_to_drop(messages: &[ModelMessage], keep_recent_turns: usize) -> Option<Range<usize>> {
    // Always retain the newest user turn (the request being answered); 0 behaves as 1.
    let keep = keep_recent_turns.max(1);
    let start = usize::from(
        messages
            .first()
            .is_some_and(|message| message.role == MessageRole::System),
    );
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, message)| message.role == MessageRole::User)
        .map(|(index, _)| index)
        .collect();
    if user_indices.len() <= keep {
        return None;
    }
    let keep_from = user_indices[user_indices.len() - keep];
    if keep_from <= start {
        return None;
    }
    let mut end = keep_from;
    while messages
        .get(end)
        .is_some_and(|message| message.role == MessageRole::Tool)
    {
        end += 1;
    }
    Some(start..end)
}

/// A dropped span as redacted, head+tail truncated text for the auxiliary summarizer.
pub fn serialize_span(messages: &[ModelMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = message.role.as_str();
        for part in &message.content {
            match part {
                ContentPart::Text { text } | ContentPart::Reasoning { text } => {
                    if !text.trim().is_empty() {
                        out.push_str(role);
                        out.push_str(": ");
                        out.push_str(text);
                        out.push('\n');
                    }
                }
                ContentPart::ToolCall {
                    name, arguments, ..
                } => {
                    out.push_str(role);
                    out.push_str(" tool call ");
                    out.push_str(name);
                    out.push_str(": ");
                    out.push_str(&arguments.to_string());
                    out.push('\n');
                }
                ContentPart::ToolResult {
                    content, is_error, ..
                } => {
                    out.push_str(role);
                    out.push_str(if *is_error {
                        " tool error: "
                    } else {
                        " tool result: "
                    });
                    out.push_str(content);
                    out.push('\n');
                }
                ContentPart::Image { .. } => {
                    out.push_str(role);
                    out.push_str(": [an image the model was shown]\n");
                }
                ContentPart::Attachment { name, path } => {
                    out.push_str(role);
                    out.push_str(": [attached file ");
                    out.push_str(name);
                    out.push_str(" at ");
                    out.push_str(path);
                    out.push_str("]\n");
                }
            }
        }
    }
    let redacted = crate::redact::redact(&out);
    truncate_middle(&redacted, SUMMARY_INPUT_MAX_BYTES)
}

/// Keep the head and tail of text, replacing the omitted middle with a marker.
fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    const MARKER: &str = "\n...[summary input truncated]...\n";
    let remaining = max_bytes.saturating_sub(MARKER.len());
    let head = floor_char_boundary(text, remaining / 2);
    let tail_start = ceil_char_boundary(text, text.len() - (remaining - remaining / 2));
    let mut out = String::with_capacity(max_bytes);
    out.push_str(&text[..head]);
    out.push_str(MARKER);
    out.push_str(&text[tail_start..]);
    out
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Put the note in the system message, replacing an earlier one. It names no counts, so compacting
/// again with the same todos leaves the system message byte-identical and the prompt cache valid.
fn annotate_summary(
    messages: &mut Vec<ModelMessage>,
    model_summary: Option<&str>,
    open_todos: Option<&str>,
) -> String {
    let mut note = match model_summary {
        Some(summary) => format!("{SUMMARY_HEADING} {summary}"),
        None => format!("{SUMMARY_HEADING} Older messages and tool outputs were cleared to fit the context window. Re-read files or re-run tools if a detail is missing, and respond to the most recent user message."),
    };
    if let Some(todos) = open_todos {
        note.push_str("\n\n");
        note.push_str(todos);
    }
    match messages.first_mut() {
        Some(first) if first.role == MessageRole::System => {
            let mut appended = false;
            for part in &mut first.content {
                if let ContentPart::Text { text } = part {
                    if let Some(at) = text.rfind(&format!("\n\n{SUMMARY_HEADING}")) {
                        text.truncate(at);
                    }
                    text.push_str("\n\n");
                    text.push_str(&note);
                    appended = true;
                    break;
                }
            }
            if !appended {
                first.content.push(ContentPart::text(String::clone(&note)));
            }
        }
        _ => messages.insert(0, ModelMessage::system(String::clone(&note))),
    }
    note
}
