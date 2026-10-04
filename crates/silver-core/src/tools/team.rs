//! The team tools: how one chat bot finds the user's other bots and asks one for help. Each bot
//! keeps its own conversation, folder, tools and permissions, so a request carries everything the
//! other bot needs and only its final reply comes back.

use crate::error::{CoreError, CoreResult};
use crate::services::Team;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::sync::Arc;
use std::time::Duration;

pub const LIST_BOTS: &str = "list_bots";
pub const ASK_BOT: &str = "ask_bot";

/// The tool names, for hiding them from runs outside the chat.
pub const TEAM_TOOLS: [&str; 2] = [LIST_BOTS, ASK_BOT];

pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ListBots));
    registry.register(Arc::new(AskBot));
}

fn team<'a>(ctx: &'a ToolContext<'_>) -> CoreResult<&'a dyn Team> {
    ctx.run.services.team.as_deref().ok_or_else(|| {
        CoreError::ToolNotAllowed("the team tools are not available in this daemon".into())
    })
}

struct ListBots;

#[async_trait]
impl Tool for ListBots {
    fn name(&self) -> &'static str {
        LIST_BOTS
    }

    fn description(&self) -> &'static str {
        "List the user's other bots: their names, what each is for, what it is doing and the \
         folder it works in. Use it to find who could help before you ask."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    async fn execute(&self, ctx: &ToolContext<'_>, _args: Value) -> CoreResult<ToolOutcome> {
        let bots = team(ctx)?.bots(&ctx.run.session).await?;
        if bots.is_empty() {
            return Ok(ToolOutcome::ok("There are no other bots."));
        }
        let lines: Vec<String> = bots
            .iter()
            .map(|bot| {
                let about = if bot.description.is_empty() {
                    String::new()
                } else {
                    format!(" - {}", bot.description)
                };
                let folder = bot
                    .folder
                    .as_deref()
                    .map_or(String::new(), |folder| format!(", works in {folder}"));
                format!("{} ({}{folder}){about}", bot.name, bot.status)
            })
            .collect();
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

struct AskBot;

#[async_trait]
impl Tool for AskBot {
    fn name(&self) -> &'static str {
        ASK_BOT
    }

    fn description(&self) -> &'static str {
        "Ask another of the user's bots to do a bounded task, and wait for its final reply (up \
         to ten minutes, including time in its queue). It has its own conversation, folder, \
         tools and permissions, and does not see yours: put everything it needs in the message. \
         It may change files. Stopping you withdraws the request; do not blindly retry a failed \
         one, because partial work may have happened."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bot": {
                    "type": "string",
                    "description": "The bot's name, as list_bots shows it.",
                },
                "message": {
                    "type": "string",
                    "description": "The task, what it needs to know, file paths and the result you expect.",
                },
            },
            "required": ["bot", "message"],
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        // The other bot's own tools are gated by its own approvals.
        RiskLevel::Read
    }

    fn timeout_hint(&self) -> Option<Duration> {
        Some(Duration::from_secs(650))
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let text = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| args.get(key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())
        };
        let (Some(bot), Some(message)) = (
            text(&["bot", "name", "bot_id", "botId", "to"]),
            text(&["message", "task", "request", "prompt"]),
        ) else {
            return Ok(ToolOutcome::error(
                "ask_bot needs `bot` (a name from list_bots) and `message`.",
            ));
        };
        match team(ctx)?
            .ask(&ctx.run.session, bot, message, &ctx.cancel)
            .await
        {
            Ok(reply) => Ok(ToolOutcome::ok(reply)),
            Err(CoreError::InvalidRequest(reason)) => Ok(ToolOutcome::error(reason)),
            Err(error) => Err(error),
        }
    }
}
