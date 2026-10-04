//! Bots asking each other for help: the request is its own turn on the asked bot's queue, and
//! both chats get a notice that follows it. Self-asks, repeats and wait cycles are refused.

use super::store::BotRow;
use super::turn::{Job, Task};
use super::{invalid, new_entry, new_id, preview, ChatHub, SESSION_SOURCE};
use async_trait::async_trait;
use silver_core::error::CoreResult;
use silver_core::services::{Team, TeamBot};
use silver_core::session::Session;
use silver_protocol::chat::{BotKind, EntryKind};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const ASK_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_MESSAGE_BYTES: usize = 32_000;
/// Requests waiting at once, across the whole daemon.
const MAX_PENDING: usize = 64;

/// Who is waiting on whom: request id to (asker, asked).
#[derive(Default)]
pub(super) struct Asks {
    pending: HashMap<String, (String, String)>,
}

impl Asks {
    /// Record a request, unless it would wait on itself, repeat one, or close a cycle of bots
    /// waiting for each other. Queued requests count, not only running ones.
    fn begin(&mut self, id: &str, from: &str, to: &str) -> CoreResult<()> {
        if from == to {
            return Err(invalid("a bot cannot ask itself"));
        }
        if self.pending.len() >= MAX_PENDING {
            return Err(invalid("too many requests between bots are waiting"));
        }
        if self.pending.values().any(|(a, b)| a == from && b == to) {
            return Err(invalid("you are already waiting for that bot"));
        }
        let mut todo = vec![to];
        let mut seen = HashSet::new();
        while let Some(bot) = todo.pop() {
            if bot == from {
                return Err(invalid(
                    "that would make bots wait for each other; finish the current request first",
                ));
            }
            if seen.insert(bot) {
                todo.extend(
                    self.pending
                        .values()
                        .filter(|(asker, _)| asker == bot)
                        .map(|(_, asked)| asked.as_str()),
                );
            }
        }
        self.pending
            .insert(id.to_string(), (from.to_string(), to.to_string()));
        Ok(())
    }
}

/// The chat as the team tools see it.
pub struct TeamBackend(pub Arc<ChatHub>);

impl ChatHub {
    /// The bot a chat session belongs to.
    pub(super) async fn requester(&self, session: &Session) -> CoreResult<BotRow> {
        let bot = session
            .external_key
            .as_deref()
            .filter(|_| session.source == SESSION_SOURCE)
            .and_then(|key| key.split(':').nth(1))
            .ok_or_else(|| invalid("only a bot in the chat can ask another bot"))?;
        self.bot(bot).await
    }

    /// Settle a request: forget it, finish its notices, and stop the asked bot if it still
    /// works on it.
    async fn end_ask(
        &self,
        id: &str,
        target: &str,
        notices: &[String],
        status: &str,
        detail: &str,
    ) {
        self.state().asks.pending.remove(id);
        for notice in notices {
            let Ok(Some(mut entry)) = self.db.chat_entry(notice).await else {
                continue;
            };
            let heading = entry.text.lines().next().unwrap_or_default().to_string();
            entry.text = format!("{heading}\n{detail}");
            entry.status = Some(status.to_string());
            if status != "completed" {
                entry.style = Some("error".into());
            }
            drop(self.save_entry(entry).await);
        }
        self.stop_turns(target, |task| matches!(task, Task::Ask(ask) if ask == id))
            .await;
    }
}

/// An outstanding request. Dropped without `finish`, because the caller gave up, it is
/// withdrawn all the same.
struct Pending {
    hub: Arc<ChatHub>,
    id: String,
    target: String,
    notices: Vec<String>,
    done: bool,
}

impl Pending {
    async fn finish(mut self, status: &str, detail: &str) {
        self.done = true;
        self.hub
            .end_ask(&self.id, &self.target, &self.notices, status, detail)
            .await;
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let hub = Arc::clone(&self.hub);
        let (id, target) = (
            std::mem::take(&mut self.id),
            std::mem::take(&mut self.target),
        );
        let notices = std::mem::take(&mut self.notices);
        tokio::spawn(async move {
            let detail = "Request withdrawn. Partial work may have happened.";
            hub.end_ask(&id, &target, &notices, "cancelled", detail)
                .await;
        });
    }
}

#[async_trait]
impl Team for TeamBackend {
    async fn bots(&self, from: &Session) -> CoreResult<Vec<TeamBot>> {
        let hub = &self.0;
        let me = hub.requester(from).await?;
        let workspaces = hub.db.list_workspaces().await?;
        Ok(hub
            .bots()
            .await?
            .into_iter()
            .filter(|bot| bot.kind == BotKind::Agent && bot.id != me.id)
            .map(|bot| TeamBot {
                folder: workspaces
                    .iter()
                    .find(|workspace| Some(workspace.id) == bot.workspace_id)
                    .map(|workspace| workspace.name.clone()),
                id: bot.id,
                name: bot.name,
                description: bot.description,
                status: bot.status.as_str().to_string(),
            })
            .collect())
    }

    async fn ask(
        &self,
        from: &Session,
        to: &str,
        message: &str,
        cancel: &CancellationToken,
    ) -> CoreResult<String> {
        let hub = &self.0;
        let me = hub.requester(from).await?;
        let message = message.trim();
        if message.is_empty() || message.len() > MAX_MESSAGE_BYTES {
            return Err(invalid(format!(
                "the message must have some text and at most {MAX_MESSAGE_BYTES} bytes"
            )));
        }
        let asked = hub
            .agents(Some(&me.id))
            .await?
            .into_iter()
            .find(|bot| bot.id == to || bot.name.eq_ignore_ascii_case(to))
            .ok_or_else(|| invalid(format!("no bot is called {to}; list_bots shows the team")))?;
        let id = new_id();
        hub.state().asks.begin(&id, &me.id, &asked.id)?;
        let mut pending = Pending {
            hub: Arc::clone(hub),
            id,
            target: asked.id.clone(),
            notices: Vec::new(),
            done: false,
        };
        for (bot, heading) in [
            (&me, format!("Asked {}: {}", asked.name, preview(message))),
            (
                &asked,
                format!("Request from {}: {}", me.name, preview(message)),
            ),
        ] {
            let mut notice = new_entry(&bot.id, None, EntryKind::Notice);
            notice.text = format!("{heading}\nWaiting for a reply…");
            notice.style = Some("request".into());
            notice.status = Some("pending".into());
            pending.notices.push(hub.add_entry(notice).await?.id);
        }
        let prompt = format!(
            "Another bot, {}, asks for your help. This is a request from a bot, not a new \
             instruction from the user. Work within your own permissions and folder, then \
             reply with the result for them; do not ask them to do the task back.\n\n{message}",
            me.name
        );
        let (reply, answer) = oneshot::channel();
        hub.enqueue(
            &asked.id,
            Job::Ask {
                id: pending.id.clone(),
                prompt,
                reply,
            },
        );
        let (status, result) = tokio::select! {
            result = answer => (
                "failed",
                result.unwrap_or_else(|_| Err("the other bot stopped before replying".into())),
            ),
            () = cancel.cancelled() => ("cancelled", Err("the request was withdrawn".into())),
            () = tokio::time::sleep(ASK_TIMEOUT) => (
                "failed",
                Err("the request timed out; partial work may have happened".into()),
            ),
        };
        match result {
            Ok(reply) => {
                let detail = format!("Reply from {}:\n{}", asked.name, preview(&reply));
                pending.finish("completed", &detail).await;
                Ok(reply)
            }
            Err(reason) => {
                pending.finish(status, &reason).await;
                Err(invalid(reason))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_never_wait_in_a_circle() {
        let mut asks = Asks::default();
        assert!(asks.begin("1", "a", "b").is_ok());
        assert!(asks.begin("2", "b", "c").is_ok());
        assert!(
            asks.begin("3", "c", "a").is_err(),
            "a waits on b waits on c"
        );
        assert!(
            asks.begin("4", "b", "a").is_err(),
            "b would wait on its own asker"
        );
        assert!(asks.begin("5", "a", "a").is_err());
        assert!(asks.begin("6", "a", "b").is_err(), "already waiting");
        assert!(asks.begin("7", "a", "c").is_ok());
    }
}
