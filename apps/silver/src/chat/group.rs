//! Group chats: a user message starts a room turn of up to `MAX_ROUNDS` rounds, in which members
//! answer one at a time in their own sessions; `(pass)` says nothing, and a round where nobody
//! speaks ends it. A newer message, or Stop, ends a turn before its next speaker.

use super::store::BotRow;
use super::turn::{Job, Lane, Task};
use super::ChatHub;
use silver_core::error::CoreResult;
use silver_protocol::chat::{ChatEntry, EntryKind};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

const MAX_ROUNDS: usize = 3;
/// Replies per room turn, across all rounds.
const MAX_REPLIES: usize = 10;
/// Room lines a member is shown per turn.
const HISTORY_LINES: u32 = 24;
/// A bot's line in another member's prompt is clipped; the user's never is.
const LINE_CHARS: usize = 2000;
/// A member that has not answered by then counts as having passed.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

impl ChatHub {
    fn next_room(&self, lane: &Lane) -> u64 {
        let mut state = self.state();
        let epoch = state.rooms.entry(lane.key().into_owned()).or_default();
        *epoch += 1;
        *epoch
    }

    fn room_is(&self, lane: &Lane, epoch: u64) -> bool {
        self.state().rooms.get(lane.key().as_ref()) == Some(&epoch)
    }
}

/// The user wrote in a group's lane: start a room turn there, ending one still running.
pub(super) fn start(hub: &Arc<ChatHub>, group: &BotRow, lane: Lane) {
    let epoch = hub.next_room(&lane);
    let (hub, group) = (Arc::clone(hub), group.id.clone());
    tokio::spawn(async move {
        if let Err(error) = run(&hub, &group, &lane, epoch).await {
            tracing::warn!(%group, %error, "a room turn failed");
        }
    });
}

/// End every room turn of `group`, in its chat and its threads, and the members' turns in them.
pub(super) async fn stop(hub: &Arc<ChatHub>, group: &BotRow) {
    {
        let mut state = hub.state();
        let threads = format!("{}/", group.id);
        for (key, epoch) in &mut state.rooms {
            if *key == group.id || key.starts_with(&threads) {
                *epoch += 1;
            }
        }
    }
    for member in &group.members {
        hub.stop_turns(
            member,
            |task| matches!(task, Task::Room(chat) if *chat == group.id),
        )
        .await;
    }
}

async fn run(hub: &Arc<ChatHub>, group_id: &str, lane: &Lane, epoch: u64) -> CoreResult<()> {
    let mut replies = 0;
    // Members whose turn failed: they sit out the rest of this one.
    let mut out: Vec<String> = Vec::new();
    for round in 0..MAX_ROUNDS {
        let group = hub.bot(group_id).await?;
        let members = members(hub, &group).await?;
        let since = since_last_user_message(hub, lane).await?;
        let mut speakers = responders(&members, &out, &since);
        // Each round starts one member later, so the same bot does not always speak first.
        if !speakers.is_empty() {
            let shift = round % speakers.len();
            speakers.rotate_left(shift);
        }
        let mut spoke = 0;
        for member in speakers {
            if !hub.room_is(lane, epoch) || replies >= MAX_REPLIES {
                return Ok(());
            }
            let prompt = member_prompt(hub, &group, &members, member, lane).await?;
            let (reply, answer) = oneshot::channel();
            hub.enqueue(
                &member.id,
                Job::Room {
                    lane: lane.clone(),
                    prompt,
                    reply,
                },
            );
            match tokio::time::timeout(TURN_TIMEOUT, answer).await {
                Ok(Ok(Ok(Some(_)))) => {
                    spoke += 1;
                    replies += 1;
                }
                Ok(Ok(Ok(None))) => {}
                // A failed, stopped or overdue turn sits the member out of the rest of this
                // turn; a failure shows in the room.
                _ => out.push(member.id.clone()),
            }
        }
        if spoke == 0 {
            break;
        }
    }
    Ok(())
}

async fn members(hub: &ChatHub, group: &BotRow) -> CoreResult<Vec<BotRow>> {
    let bots = hub.db.chat_bots().await?;
    Ok(group
        .members
        .iter()
        .filter_map(|id| bots.iter().find(|bot| bot.id == *id).cloned())
        .collect())
}

/// What the lane said since the user last wrote there, that message included.
async fn since_last_user_message(hub: &ChatHub, lane: &Lane) -> CoreResult<Vec<ChatEntry>> {
    let recent = hub
        .db
        .chat_entries_after(&lane.chat, lane.thread.as_deref(), 0, HISTORY_LINES)
        .await?;
    let start = recent
        .iter()
        .rposition(|entry| entry.kind == EntryKind::User)
        .unwrap_or(0);
    Ok(recent[start..].to_vec())
}

/// Who answers: the members @-mentioned since the user's last message, or everyone when nobody
/// (or `@all` / `@everyone`) is. Members in `out` never answer, so a mention of one that is
/// stuck falls to the rest instead of stranding the room.
fn responders<'a>(
    members: &'a [BotRow],
    out: &[String],
    messages: &[ChatEntry],
) -> Vec<&'a BotRow> {
    let live = || members.iter().filter(|member| !out.contains(&member.id));
    let texts: Vec<&str> = messages.iter().map(|entry| entry.text.as_str()).collect();
    let everyone = texts
        .iter()
        .any(|text| mentions(text, "all") || mentions(text, "everyone"));
    let named: Vec<&BotRow> = live()
        .filter(|member| {
            texts.iter().any(|text| {
                handles(&member.name)
                    .iter()
                    .any(|handle| mentions(text, handle))
            })
        })
        .collect();
    if everyone || named.is_empty() {
        live().collect()
    } else {
        named
    }
}

/// Ways to @-mention a bot: its full name, without spaces, or its first word, lowercased.
fn handles(name: &str) -> Vec<String> {
    let lower = name.trim().to_lowercase();
    let squashed: String = lower.split_whitespace().collect();
    let first = lower
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let mut out = vec![lower, squashed, first];
    out.retain(|handle| !handle.is_empty());
    out.dedup();
    out
}

/// `@handle` in `text`, not followed by more of a Latin word (`@al` is not `@alice`). CJK names
/// are often followed straight by the message, so only ASCII letters end a match early.
fn mentions(text: &str, handle: &str) -> bool {
    let lower = text.to_lowercase();
    let needle = format!("@{handle}");
    lower.match_indices(&needle).any(|(at, _)| {
        lower[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_ascii_alphanumeric() || next == '_'))
    })
}

fn clip(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// What a member is told: who is in the room, what was said since it last spoke, and that it
/// may pass.
async fn member_prompt(
    hub: &ChatHub,
    group: &BotRow,
    members: &[BotRow],
    me: &BotRow,
    lane: &Lane,
) -> CoreResult<String> {
    let peers: Vec<&BotRow> = members.iter().filter(|member| member.id != me.id).collect();
    let peer_names = if peers.is_empty() {
        "just the user".to_string()
    } else {
        peers
            .iter()
            .map(|peer| peer.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let about = group.description.trim();
    let room = if about.is_empty() {
        format!("\"{}\"", group.name)
    } else {
        format!("\"{}\" — {about}", group.name)
    };
    let mut out = vec![format!(
        "You are {}, one participant in a group chat ({room}) with the user and {peer_names}.",
        me.name
    )];
    if !peers.is_empty() {
        out.push("Other participants in the room:".into());
        for peer in &peers {
            let about = peer.description.trim();
            out.push(if about.is_empty() {
                format!("- {}", peer.name)
            } else {
                format!("- {} ({about})", peer.name)
            });
        }
    }
    out.push(
        "You have your full toolkit here. Do the work first, then answer: your final reply of \
         this turn is the one message the room sees. Keep it short and conversational. \
         @-mention another participant only when you need them to act. If you have nothing new \
         worth adding, reply exactly \"(pass)\". Never reveal private one-on-one context."
            .into(),
    );
    out.push(String::new());

    let thread = lane.thread.as_deref();
    let seen = hub.db.chat_last_spoke(&lane.chat, thread, &me.id).await?;
    let mut header = format!("[Group chat: \"{}\" - with {peer_names}]", group.name);
    let mut lines = Vec::new();
    if let Some(root) = thread {
        header = format!(
            "[Group chat: \"{}\", in a thread - with {peer_names}]",
            group.name
        );
        if let (0, Ok(Some(root))) = (seen, hub.db.chat_entry(root).await) {
            lines.push(format!("(thread started on) {}", line(&root, members, me)));
        }
    }
    for entry in hub
        .db
        .chat_entries_after(&lane.chat, thread, seen, HISTORY_LINES)
        .await?
    {
        lines.push(line(&entry, members, me));
    }
    out.push(header);
    if lines.is_empty() {
        out.push("No new messages in the room since your last turn.".into());
    } else {
        out.push("New messages in the room (oldest first):".into());
        out.extend(lines);
    }
    out.push(String::new());
    out.push(format!(
        "It's your turn, {}. Reply to the room if you have something worth adding, or reply \
         exactly \"(pass)\" if you don't.",
        me.name
    ));
    Ok(out.join("\n"))
}

fn line(entry: &ChatEntry, members: &[BotRow], me: &BotRow) -> String {
    if entry.kind == EntryKind::User {
        return format!("User: {}", entry.text);
    }
    let author = entry.author.as_deref().unwrap_or_default();
    let text = clip(&entry.text, LINE_CHARS);
    if author == me.id {
        return format!("{} (you): {text}", me.name);
    }
    let name = members
        .iter()
        .find(|member| member.id == author)
        .map_or("A bot", |member| member.name.as_str());
    format!("{name}: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::store::now_ms;
    use crate::chat::turn::is_pass;
    use silver_protocol::chat::BotKind;

    fn bot(id: &str, name: &str) -> BotRow {
        BotRow {
            id: id.into(),
            kind: BotKind::Agent,
            name: name.into(),
            description: String::new(),
            instructions: String::new(),
            avatar_shape: "blob".into(),
            avatar_color: "blue".into(),
            provider: None,
            model: None,
            reasoning_effort: None,
            workspace_id: None,
            yolo: false,
            members: Vec::new(),
            pinned: false,
            epoch: 0,
            created_at: now_ms(),
        }
    }

    fn said(text: &str) -> ChatEntry {
        let mut entry = crate::chat::new_entry("g", None, EntryKind::User);
        entry.text = text.into();
        entry
    }

    fn who_is_left(members: &[BotRow], out: &[String], text: &str) -> Vec<String> {
        responders(members, out, &[said(text)])
            .into_iter()
            .map(|member| member.id.clone())
            .collect()
    }

    fn who(members: &[BotRow], text: &str) -> Vec<String> {
        who_is_left(members, &[], text)
    }

    #[test]
    fn mentions_pick_responders_and_no_mention_means_everyone() {
        let members = [bot("a", "Alice Chen"), bot("b", "Bob"), bot("c", "尼采課")];
        assert_eq!(who(&members, "hi all"), ["a", "b", "c"]);
        assert_eq!(who(&members, "@bob can you look"), ["b"]);
        assert_eq!(who(&members, "@alicechen and @Alice"), ["a"]);
        assert_eq!(who(&members, "@尼采課幫我整理"), ["c"]);
        assert_eq!(who(&members, "@bobby?"), ["a", "b", "c"]);
        assert_eq!(who(&members, "@all go"), ["a", "b", "c"]);
    }

    #[test]
    fn a_member_that_sat_out_is_not_asked_even_when_mentioned() {
        let members = [bot("a", "Alice"), bot("b", "Bob"), bot("c", "Cara")];
        let out = ["b".to_string()];
        assert_eq!(who_is_left(&members, &out, "hi all"), ["a", "c"]);
        assert_eq!(who_is_left(&members, &out, "@bob can you look"), ["a", "c"]);
        assert_eq!(who_is_left(&members, &out, "@cara and @bob"), ["c"]);
    }

    #[test]
    fn passes_are_recognised() {
        for pass in ["(pass)", " (Pass). ", "\"pass\"", "", "  "] {
            assert!(is_pass(pass), "{pass:?}");
        }
        assert!(!is_pass("I passed the test"));
    }
}
