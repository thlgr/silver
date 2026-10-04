//! An external agent's permission requests, put to the user as approval cards in the bot's chat.
//! A bot set to approve automatically answers them itself; a card left unanswered when the turn
//! ends expires, and the agent is told no.

use super::{new_entry, preview, ChatHub};
use crate::acp::{PermissionBroker, PermissionRequest};
use async_trait::async_trait;
use serde_json::{json, Value};
use silver_protocol::chat::{BotStatus, EntryKind, PermissionView};
use silver_protocol::{ApprovalDecision, ApprovalId, RiskLevel, SessionId};
use std::collections::HashMap;
use tokio::sync::oneshot;

/// A request waiting for the user, kept by the entry that shows it.
pub(super) struct Waiting {
    bot: String,
    reply: oneshot::Sender<ApprovalDecision>,
}

/// How the card describes the agent's action: the silver tool it most resembles, whose headline
/// and icon the card already knows, and how risky that is.
fn resembling(kind: &str) -> (&'static str, RiskLevel) {
    match kind {
        "execute" => ("bash", RiskLevel::Process),
        "edit" => ("patch", RiskLevel::Write),
        "delete" | "move" => ("write_file", RiskLevel::Destructive),
        "read" => ("read_file", RiskLevel::Read),
        "search" => ("search_files", RiskLevel::Read),
        "fetch" => ("web_extract", RiskLevel::Read),
        _ => ("tool", RiskLevel::Write),
    }
}

fn card(request: PermissionRequest) -> PermissionView {
    let (tool, risk) = resembling(&request.kind);
    let input = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| request.input.get(key).and_then(Value::as_str))
    };
    let arguments = match request.kind.as_str() {
        "execute" => json!({ "command": input(&["command", "cmd"]).unwrap_or(&request.title) }),
        _ => {
            json!({ "path": input(&["file_path", "path", "url", "pattern"]).unwrap_or(&request.title) })
        }
    };
    PermissionView {
        approval_id: ApprovalId::new(),
        tool: tool.to_string(),
        risk,
        description: request.title,
        arguments,
        status: "pending".into(),
    }
}

impl ChatHub {
    /// Settle a card an agent's request was shown as: how it ended, and the bot is working again
    /// once nothing else waits on the user.
    async fn settle_permission(&self, entry_id: &str, bot: &str, status: &str) {
        if let Ok(Some(mut entry)) = self.db.chat_entry(entry_id).await {
            if let Some(card) = &mut entry.permission {
                card.status = status.into();
            }
            drop(self.save_entry(entry).await);
        }
        if let Some(live) = self.state().runtime.get_mut(bot) {
            live.pending = live.pending.saturating_sub(1);
            if live.pending == 0 {
                live.status = BotStatus::Working;
                live.activity = "Working…".into();
            }
        }
        self.publish_bot(bot).await;
    }

    /// The user decided a card an agent's request is shown as. False when it is not one.
    pub(super) async fn answer_permission(
        &self,
        entry_id: &str,
        decision: ApprovalDecision,
    ) -> bool {
        let Some(waiting) = self.state().permissions.remove(entry_id) else {
            return false;
        };
        waiting.reply.send(decision).ok();
        let status = if decision == ApprovalDecision::Deny {
            "denied"
        } else {
            "approved"
        };
        self.settle_permission(entry_id, &waiting.bot, status).await;
        true
    }

    /// A turn is over: whatever its agent still asked the user can no longer be answered.
    pub(super) async fn expire_permissions(&self, bot: &str) {
        let mine = {
            let mut state = self.state();
            let (mine, others): (HashMap<_, _>, HashMap<_, _>) =
                std::mem::take(&mut state.permissions)
                    .into_iter()
                    .partition(|(_, waiting)| waiting.bot == bot);
            state.permissions = others;
            mine
        };
        for (id, waiting) in mine {
            waiting.reply.send(ApprovalDecision::Deny).ok();
            self.settle_permission(&id, bot, "expired").await;
        }
    }
}

#[async_trait]
impl PermissionBroker for ChatHub {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision {
        let Ok(session) = request.session.parse::<SessionId>() else {
            return ApprovalDecision::Deny;
        };
        let Ok(Some(session)) = self.db.get_session(session).await else {
            return ApprovalDecision::Deny;
        };
        let Ok(bot) = self.requester(&session).await else {
            return ApprovalDecision::Deny;
        };
        if bot.yolo {
            return ApprovalDecision::Approve;
        }
        let (mut entry, run) = {
            let state = self.state();
            let Some(live) = state.runtime.get(&bot.id) else {
                return ApprovalDecision::Deny;
            };
            let Some(lane) = &live.lane else {
                return ApprovalDecision::Deny;
            };
            (
                new_entry(&lane.chat, lane.thread.as_deref(), EntryKind::Permission),
                live.run,
            )
        };
        entry.author = Some(bot.id.clone());
        entry.run_id = run;
        entry.session_id = Some(session.id);
        let needs = format!("Needs your approval: {}", preview(&request.title));
        entry.permission = Some(card(request));
        // Waiting before the card is shown, so an answer can never beat it.
        let (reply, answer) = oneshot::channel();
        {
            let mut state = self.state();
            state.permissions.insert(
                entry.id.clone(),
                Waiting {
                    bot: bot.id.clone(),
                    reply,
                },
            );
            if let Some(live) = state.runtime.get_mut(&bot.id) {
                live.pending += 1;
                live.status = BotStatus::NeedsInput;
                live.activity = needs;
            }
        }
        drop(self.add_entry(entry).await);
        self.publish_bot(&bot.id).await;
        answer.await.unwrap_or(ApprovalDecision::Deny)
    }
}
