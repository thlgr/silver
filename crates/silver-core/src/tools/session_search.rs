//! The session_search tool: `query` searches, `session_id` reads (around `around_message_id` if
//! given), nothing browses; unused arguments are ignored. The scope always comes from the run
//! context.

use crate::session::{Session, SessionSearch, SessionSearchHit, SessionTranscript};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use silver_protocol::{ContentPart, MessageId, MessageRole, RiskLevel, SessionId};
use std::sync::Arc;

/// Default number of sessions returned by discovery and browse.
pub const DEFAULT_LIMIT: usize = 3;
/// Hard cap on the number of sessions returned.
pub const MAX_LIMIT: usize = 10;
/// Default number of messages returned on each side of a scroll anchor.
pub const DEFAULT_WINDOW: usize = 5;
/// Hard cap on the scroll window.
pub const MAX_WINDOW: usize = 20;
/// Hard cap on a single read shape result.
const READ_MESSAGE_LIMIT: usize = 50;
/// Message hits fetched per search; a session often has several, and only its best is shown.
const SEARCH_FETCH: usize = 50;
/// Closes a search result so a small model knows how to go deeper.
const READ_HINT: &str =
    "Read a session with session_id; add around_message_id to see the messages around a hit.";

/// Register the session_search tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(SessionSearchTool));
}

struct SessionSearchTool;

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &'static str {
        "session_search"
    }

    fn description(&self) -> &'static str {
        "Recall past conversations in this workspace. query: find sessions by keywords (any word may match). session_id: read that conversation. session_id + around_message_id: the messages around one hit. No arguments: recent sessions."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A few keywords from the past conversation."
                },
                "session_id": {
                    "type": "string",
                    "description": "Session to read, from a prior result."
                },
                "around_message_id": {
                    "type": "string",
                    "description": "With session_id: message id to center on, from a prior result."
                },
                "window": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": 5,
                    "description": "With around_message_id: messages per side. Default 5."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 3,
                    "description": "With query or no arguments: max sessions. Default 3."
                }
            },
            "required": []
        })
    }

    fn risk(&self, _args: &serde_json::Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> crate::error::CoreResult<ToolOutcome> {
        let Some(search) = ctx.run.services.session_search.as_deref() else {
            return Ok(ToolOutcome::error("session search is unavailable"));
        };
        let query = opt_str(&args, "query");
        let mut session_id = opt_str(&args, "session_id");
        let mut around = opt_str(&args, "around_message_id");
        // A small model often puts the session id it just found here; read that session.
        if let Some(id) = around.filter(|id| id.parse::<SessionId>().is_ok()) {
            session_id = session_id.or(Some(id));
            around = None;
        }

        match (session_id, around, query) {
            (Some(id), Some(anchor), _) => run_scroll(ctx, search, &args, id, anchor).await,
            (Some(id), None, _) => run_read(ctx, search, id).await,
            (None, _, Some(query)) => run_discovery(ctx, search, &args, query).await,
            (None, _, None) => run_browse(ctx, search, &args).await,
        }
    }
}

fn opt_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn clamp_arg(args: &serde_json::Value, key: &str, default: usize, max: usize) -> usize {
    args.get(key)
        .and_then(|value| value.as_u64())
        .map(|value| (value as usize).clamp(1, max))
        .unwrap_or(default)
}

async fn run_discovery(
    ctx: &ToolContext<'_>,
    search: &dyn SessionSearch,
    args: &serde_json::Value,
    query: &str,
) -> crate::error::CoreResult<ToolOutcome> {
    let limit = clamp_arg(args, "limit", DEFAULT_LIMIT, MAX_LIMIT);
    let hits = search.search(&ctx.run.scope, query, SEARCH_FETCH).await?;
    // Hits come best first; keep each session's best one. The current session is already
    // in context, so its own prompt is not a past match.
    let mut shown: Vec<SessionId> = Vec::new();
    let mut out = String::new();
    for hit in &hits {
        if hit.session_id == ctx.run.session.id || shown.contains(&hit.session_id) {
            continue;
        }
        shown.push(hit.session_id);
        out.push_str(&format_hit(hit));
        out.push_str("\n\n");
        if shown.len() >= limit {
            break;
        }
    }
    if shown.is_empty() {
        return Ok(ToolOutcome::ok("no matching sessions"));
    }
    out.push_str(READ_HINT);
    Ok(ToolOutcome::ok(out))
}

async fn run_browse(
    ctx: &ToolContext<'_>,
    search: &dyn SessionSearch,
    args: &serde_json::Value,
) -> crate::error::CoreResult<ToolOutcome> {
    let limit = clamp_arg(args, "limit", DEFAULT_LIMIT, MAX_LIMIT);
    let sessions = search.recent_sessions(&ctx.run.scope, limit).await?;
    if sessions.is_empty() {
        return Ok(ToolOutcome::ok("no recent sessions"));
    }
    let lines: Vec<String> = sessions.iter().map(format_session).collect();
    Ok(ToolOutcome::ok(lines.join("\n\n")))
}

async fn run_read(
    ctx: &ToolContext<'_>,
    search: &dyn SessionSearch,
    session_id: &str,
) -> crate::error::CoreResult<ToolOutcome> {
    let Some(id) = parse_session_id(session_id) else {
        return Ok(ToolOutcome::error(format!(
            "session_search: invalid session_id '{session_id}'"
        )));
    };
    match search
        .read_session(&ctx.run.scope, id, None, 0, READ_MESSAGE_LIMIT)
        .await?
    {
        None => Ok(ToolOutcome::error(format!(
            "session_search: session not found in this scope: {session_id}"
        ))),
        Some(transcript) => Ok(ToolOutcome::ok(format_transcript(&transcript))),
    }
}

async fn run_scroll(
    ctx: &ToolContext<'_>,
    search: &dyn SessionSearch,
    args: &serde_json::Value,
    session_id: &str,
    around: &str,
) -> crate::error::CoreResult<ToolOutcome> {
    let Some(id) = parse_session_id(session_id) else {
        return Ok(ToolOutcome::error(format!(
            "session_search: invalid session_id '{session_id}'"
        )));
    };
    // A hit reads "user msg_…", and a small model copies the role label along with the id.
    let around = around.split_whitespace().last().unwrap_or(around);
    let Ok(anchor) = around.parse::<MessageId>() else {
        return Ok(ToolOutcome::error(format!(
            "session_search: invalid message id '{around}'; omit around_message_id to read the whole session"
        )));
    };
    let window = clamp_arg(args, "window", DEFAULT_WINDOW, MAX_WINDOW);
    match search
        .read_session(&ctx.run.scope, id, Some(anchor), window, READ_MESSAGE_LIMIT)
        .await?
    {
        None => Ok(ToolOutcome::error(format!(
            "session_search: session not found in this scope: {session_id}"
        ))),
        Some(transcript) if transcript.messages.is_empty() => Ok(ToolOutcome::error(format!(
            "session_search: message {anchor} was not found in session {session_id}"
        ))),
        Some(transcript) => Ok(ToolOutcome::ok(format_transcript(&transcript))),
    }
}

fn parse_session_id(raw: &str) -> Option<SessionId> {
    raw.parse::<SessionId>().ok()
}

/// Minutes are enough to tell sessions apart, and nanoseconds cost a small model tokens.
fn format_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%M").to_string()
}

/// Format one hit as its session header, then the matching message and its snippet.
fn format_hit(hit: &SessionSearchHit) -> String {
    let title = hit.title.as_deref().unwrap_or("(untitled)");
    let role = hit.role.map_or("message", MessageRole::as_str);
    format!(
        "session {} ({}): {title}\n{role} {}: {}",
        hit.session_id,
        format_time(hit.created_at),
        hit.message_id,
        hit.snippet
    )
}

/// Format one session header for the browse shape.
fn format_session(session: &Session) -> String {
    let title = session.title.as_deref().unwrap_or("(untitled)");
    format!(
        "session {} ({}): {title}",
        session.id,
        format_time(session.created_at)
    )
}

/// Format a read or scroll transcript with omission markers.
fn format_transcript(transcript: &SessionTranscript) -> String {
    let mut out = format_session(&transcript.session);
    if transcript.messages_before > 0 {
        out.push_str(&format!(
            "\n[{} older messages omitted]",
            transcript.messages_before
        ));
    }
    for message in &transcript.messages {
        out.push_str("\n\n");
        out.push_str(&format_message(message));
    }
    if transcript.messages_after > 0 {
        out.push_str(&format!(
            "\n\n[{} newer messages omitted]",
            transcript.messages_after
        ));
    }
    out
}

fn format_message(message: &crate::session::Message) -> String {
    let text = message
        .content
        .iter()
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{} {} ({}):\n{}",
        message.role.as_str(),
        message.id,
        format_time(message.created_at),
        text
    )
}
