//! Persistence for the bot chat: bots, the entries of each chat and thread, and read marks.
//! Timestamps are unix milliseconds, and ids are bare UUIDs.

use crate::db::{Db, DbResult};
use serde::{Deserialize, Serialize};
use silver_protocol::chat::{
    BotKind, ChatEntry, EntryKind, LimitWindow, PermissionView, ThreadSummary,
};
use silver_protocol::{RunId, SessionId, WorkspaceId};
use std::collections::HashMap;
use tokio_rusqlite::rusqlite::{self, params, params_from_iter, OptionalExtension};

/// One bot as stored.
#[derive(Clone, Debug)]
pub struct BotRow {
    pub id: String,
    pub kind: BotKind,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub avatar_shape: String,
    pub avatar_color: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    pub yolo: bool,
    pub members: Vec<String>,
    pub pinned: bool,
    pub epoch: i64,
    pub created_at: i64,
}

/// The parts of an entry kept as JSON.
#[derive(Default, Serialize, Deserialize)]
struct EntryData {
    #[serde(default)]
    reactions: Vec<String>,
    #[serde(default)]
    permission: Option<PermissionView>,
    #[serde(default)]
    limits: Vec<LimitWindow>,
}

/// What a roster row shows besides the bot itself.
#[derive(Default)]
pub struct BotStats {
    /// The newest user message or finished reply in the main chat: who wrote it and what.
    pub last: Option<(EntryKind, Option<String>, String, i64)>,
    pub unread: u32,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const BOT_COLUMNS: &str = "id, kind, name, description, instructions, avatar_shape, avatar_color, \
    provider, model, workspace_id, yolo, members, pinned, epoch, created_at, reasoning_effort";

fn bot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BotRow> {
    let kind: String = row.get(1)?;
    let workspace: Option<String> = row.get(9)?;
    let members: String = row.get(11)?;
    Ok(BotRow {
        id: row.get(0)?,
        kind: BotKind::parse(&kind).unwrap_or(BotKind::Agent),
        name: row.get(2)?,
        description: row.get(3)?,
        instructions: row.get(4)?,
        avatar_shape: row.get(5)?,
        avatar_color: row.get(6)?,
        provider: row.get(7)?,
        model: row.get(8)?,
        workspace_id: workspace.and_then(|id| id.parse().ok()),
        yolo: row.get(10)?,
        members: serde_json::from_str(&members).unwrap_or_default(),
        pinned: row.get(12)?,
        epoch: row.get(13)?,
        created_at: row.get(14)?,
        reasoning_effort: row.get(15)?,
    })
}

const ENTRY_COLUMNS: &str = "seq, id, chat_id, thread_id, kind, author, text, final, status, \
    style, run_id, session_id, nonce, data, created_at";

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatEntry> {
    let kind: String = row.get(4)?;
    let run: Option<String> = row.get(10)?;
    let session: Option<String> = row.get(11)?;
    let data: String = row.get(13)?;
    let data: EntryData = serde_json::from_str(&data).unwrap_or_default();
    Ok(ChatEntry {
        seq: row.get(0)?,
        id: row.get(1)?,
        chat_id: row.get(2)?,
        thread_id: row.get(3)?,
        kind: EntryKind::parse(&kind).unwrap_or(EntryKind::Notice),
        author: row.get(5)?,
        text: row.get(6)?,
        is_final: row.get(7)?,
        status: row.get(8)?,
        style: row.get(9)?,
        run_id: run.and_then(|id| id.parse::<RunId>().ok()),
        session_id: session.and_then(|id| id.parse::<SessionId>().ok()),
        nonce: row.get(12)?,
        reactions: data.reactions,
        thread: None,
        permission: data.permission,
        limits: data.limits,
        created_at: row.get(14)?,
    })
}

fn entry_data(entry: &ChatEntry) -> String {
    serde_json::json!({
        "reactions": entry.reactions,
        "permission": entry.permission,
        "limits": entry.limits,
    })
    .to_string()
}

/// `?,?,?` for an `IN` list of `count` values, starting at parameter `from`.
fn placeholders(from: usize, count: usize) -> String {
    (from..from + count)
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(",")
}

impl Db {
    pub async fn chat_bots(&self) -> DbResult<Vec<BotRow>> {
        self.call(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {BOT_COLUMNS} FROM bots ORDER BY created_at, id"
            ))?;
            let rows = stmt.query_map([], bot_from_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    pub async fn chat_bot(&self, id: &str) -> DbResult<Option<BotRow>> {
        let id = id.to_string();
        self.call(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {BOT_COLUMNS} FROM bots WHERE id = ?1"),
                    params![id],
                    bot_from_row,
                )
                .optional()?)
        })
        .await
    }

    /// Insert a bot, or replace the one with its id; returns it.
    pub async fn save_chat_bot(&self, bot: BotRow) -> DbResult<BotRow> {
        self.write(move |conn| {
            conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO bots ({BOT_COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
                ),
                params![
                    bot.id,
                    bot.kind.as_str(),
                    bot.name,
                    bot.description,
                    bot.instructions,
                    bot.avatar_shape,
                    bot.avatar_color,
                    bot.provider,
                    bot.model,
                    bot.workspace_id.map(|id| id.to_string()),
                    bot.yolo,
                    serde_json::to_string(&bot.members).unwrap_or_else(|_| "[]".into()),
                    bot.pinned,
                    bot.epoch,
                    bot.created_at,
                    bot.reasoning_effort,
                ],
            )?;
            Ok(bot)
        })
        .await
    }

    /// Delete a bot with every entry of its chat and its read marks.
    pub async fn delete_chat_bot(&self, id: &str) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM chat_entries WHERE chat_id = ?1", params![id])?;
            tx.execute("DELETE FROM chat_reads WHERE chat_id = ?1", params![id])?;
            tx.execute("DELETE FROM bots WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Store a new entry and return it with its `seq`.
    pub async fn chat_insert_entry(&self, mut entry: ChatEntry) -> DbResult<ChatEntry> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO chat_entries (id, chat_id, thread_id, kind, author, text, final, \
                 status, style, run_id, session_id, nonce, data, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    entry.id,
                    entry.chat_id,
                    entry.thread_id,
                    entry.kind.as_str(),
                    entry.author,
                    entry.text,
                    entry.is_final,
                    entry.status,
                    entry.style,
                    entry.run_id.map(|id| id.to_string()),
                    entry.session_id.map(|id| id.to_string()),
                    entry.nonce,
                    entry_data(&entry),
                    entry.created_at,
                ],
            )?;
            entry.seq = conn.last_insert_rowid();
            Ok(entry)
        })
        .await
    }

    /// Write back what an entry can change after it exists; returns it.
    pub async fn chat_update_entry(&self, entry: ChatEntry) -> DbResult<ChatEntry> {
        let data = entry_data(&entry);
        self.write(move |conn| {
            conn.execute(
                "UPDATE chat_entries SET text = ?2, final = ?3, status = ?4, style = ?5, \
                 data = ?6 WHERE id = ?1",
                params![
                    entry.id,
                    entry.text,
                    entry.is_final,
                    entry.status,
                    entry.style,
                    data
                ],
            )?;
            Ok(entry)
        })
        .await
    }

    pub async fn chat_entry(&self, id: &str) -> DbResult<Option<ChatEntry>> {
        let id = id.to_string();
        self.call(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {ENTRY_COLUMNS} FROM chat_entries WHERE id = ?1"),
                    params![id],
                    entry_from_row,
                )
                .optional()?)
        })
        .await
    }

    /// The message a client already sent under `nonce`, if it arrived.
    pub async fn chat_entry_by_nonce(
        &self,
        chat_id: &str,
        nonce: &str,
    ) -> DbResult<Option<ChatEntry>> {
        let (chat, nonce) = (chat_id.to_string(), nonce.to_string());
        self.call(move |conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {ENTRY_COLUMNS} FROM chat_entries \
                         WHERE chat_id = ?1 AND nonce = ?2"
                    ),
                    params![chat, nonce],
                    entry_from_row,
                )
                .optional()?)
        })
        .await
    }

    pub async fn chat_delete_entry(&self, id: &str) -> DbResult<()> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute("DELETE FROM chat_entries WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    /// The newest `limit` entries of one lane older than `before`, oldest first. A lane is the
    /// main chat (`thread` none) or one thread.
    pub async fn chat_entries(
        &self,
        chat_id: &str,
        thread: Option<&str>,
        before: Option<i64>,
        limit: u32,
    ) -> DbResult<Vec<ChatEntry>> {
        let (chat, thread) = (chat_id.to_string(), thread.map(str::to_string));
        self.call(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM chat_entries \
                 WHERE chat_id = ?1 AND thread_id IS ?2 AND seq < ?3 \
                 ORDER BY seq DESC LIMIT ?4"
            ))?;
            let rows = stmt.query_map(
                params![chat, thread, before.unwrap_or(i64::MAX), i64::from(limit)],
                entry_from_row,
            )?;
            let mut out: Vec<ChatEntry> = rows.collect::<rusqlite::Result<_>>()?;
            out.reverse();
            Ok(out)
        })
        .await
    }

    /// The entries of one lane after `seq`, oldest first.
    pub async fn chat_entries_after(
        &self,
        chat_id: &str,
        thread: Option<&str>,
        after: i64,
        limit: u32,
    ) -> DbResult<Vec<ChatEntry>> {
        let (chat, thread) = (chat_id.to_string(), thread.map(str::to_string));
        self.call(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM (SELECT * FROM chat_entries \
                 WHERE chat_id = ?1 AND thread_id IS ?2 AND seq > ?3 AND kind IN ('user', 'agent') \
                 AND final = 1 ORDER BY seq DESC LIMIT ?4) ORDER BY seq"
            ))?;
            let rows = stmt.query_map(
                params![chat, thread, after, i64::from(limit)],
                entry_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// The `seq` of what `author` last said in a lane, or 0.
    pub async fn chat_last_spoke(
        &self,
        chat_id: &str,
        thread: Option<&str>,
        author: &str,
    ) -> DbResult<i64> {
        let (chat, thread, author) = (
            chat_id.to_string(),
            thread.map(str::to_string),
            author.to_string(),
        );
        self.call(move |conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM chat_entries \
                 WHERE chat_id = ?1 AND thread_id IS ?2 AND kind = 'agent' AND author = ?3",
                params![chat, thread, author],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Summaries of the threads on `roots`, which are entries of `chat_id`.
    pub async fn chat_thread_summaries(
        &self,
        chat_id: &str,
        roots: Vec<String>,
    ) -> DbResult<HashMap<String, ThreadSummary>> {
        if roots.is_empty() {
            return Ok(HashMap::new());
        }
        let chat = chat_id.to_string();
        self.call(move |conn| {
            let marks = placeholders(2, roots.len());
            let mut stmt = conn.prepare(&format!(
                "SELECT e.thread_id, e.kind, e.author, e.created_at, \
                        (e.kind = 'agent' AND e.final = 1 AND e.seq > COALESCE(r.read_seq, 0)) \
                 FROM chat_entries e \
                 LEFT JOIN chat_reads r ON r.chat_id = e.chat_id AND r.thread_id = e.thread_id \
                 WHERE e.chat_id = ?1 AND e.thread_id IN ({marks}) AND e.kind IN ('user', 'agent') \
                 ORDER BY e.seq"
            ))?;
            let args = std::iter::once(&chat).chain(roots.iter());
            let mut rows = stmt.query(params_from_iter(args))?;
            let mut out: HashMap<String, ThreadSummary> = HashMap::new();
            while let Some(row) = rows.next()? {
                let root: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let author: Option<String> = row.get(2)?;
                let summary = out.entry(root).or_insert_with(|| ThreadSummary {
                    count: 0,
                    last_at: 0,
                    authors: Vec::new(),
                    unread: 0,
                });
                summary.count += 1;
                summary.last_at = row.get(3)?;
                summary.unread += u32::from(row.get::<_, bool>(4)?);
                let who = if kind == "user" {
                    Some("user".to_string())
                } else {
                    author
                };
                if let Some(who) = who.filter(|who| !summary.authors.contains(who)) {
                    summary.authors.push(who);
                }
            }
            Ok(out)
        })
        .await
    }

    /// What each roster row shows: its newest message and how many replies are unread.
    pub async fn chat_bot_stats(&self) -> DbResult<HashMap<String, BotStats>> {
        self.call(|conn| {
            let mut out: HashMap<String, BotStats> = HashMap::new();
            let mut last = conn.prepare(
                "SELECT chat_id, kind, author, text, created_at FROM chat_entries \
                 WHERE seq IN (SELECT MAX(seq) FROM chat_entries WHERE thread_id IS NULL \
                               AND kind IN ('user', 'agent') AND final = 1 GROUP BY chat_id)",
            )?;
            let mut rows = last.query([])?;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(1)?;
                out.entry(row.get(0)?).or_default().last = Some((
                    EntryKind::parse(&kind).unwrap_or(EntryKind::User),
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ));
            }
            let mut unread = conn.prepare(
                "SELECT e.chat_id, COUNT(*) FROM chat_entries e \
                 LEFT JOIN chat_reads r ON r.chat_id = e.chat_id \
                      AND r.thread_id = COALESCE(e.thread_id, '') \
                 WHERE e.kind = 'agent' AND e.final = 1 AND e.seq > COALESCE(r.read_seq, 0) \
                 GROUP BY e.chat_id",
            )?;
            let mut rows = unread.query([])?;
            while let Some(row) = rows.next()? {
                let count: i64 = row.get(1)?;
                out.entry(row.get(0)?).or_default().unread = u32::try_from(count).unwrap_or(0);
            }
            Ok(out)
        })
        .await
    }

    /// Mark everything now in a lane as read. True when that moved the mark.
    pub async fn chat_mark_read(&self, chat_id: &str, thread: Option<&str>) -> DbResult<bool> {
        let (chat, thread) = (chat_id.to_string(), thread.map(str::to_string));
        self.write(move |conn| {
            let changed = conn.execute(
                "INSERT INTO chat_reads (chat_id, thread_id, read_seq) \
                 VALUES (?1, COALESCE(?2, ''), \
                         (SELECT COALESCE(MAX(seq), 0) FROM chat_entries \
                          WHERE chat_id = ?1 AND thread_id IS ?2)) \
                 ON CONFLICT (chat_id, thread_id) DO UPDATE SET read_seq = excluded.read_seq \
                 WHERE excluded.read_seq > chat_reads.read_seq",
                params![chat, thread],
            )?;
            Ok(changed > 0)
        })
        .await
    }

    /// After a restart nothing is still being written: finish half-written replies, and expire
    /// the approvals no run is waiting on any more.
    pub async fn chat_recover(&self) -> DbResult<()> {
        self.write(|conn| {
            conn.execute(
                "DELETE FROM chat_entries WHERE kind = 'agent' AND final = 0 AND text = ''",
                [],
            )?;
            conn.execute("UPDATE chat_entries SET final = 1 WHERE final = 0", [])?;
            conn.execute(
                "UPDATE chat_entries SET data = json_set(data, '$.permission.status', 'expired') \
                 WHERE kind = 'permission' AND json_extract(data, '$.permission.status') = 'pending'",
                [],
            )?;
            conn.execute(
                "UPDATE chat_entries SET status = 'cancelled' WHERE status = 'queued'",
                [],
            )?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::new_entry;

    async fn database() -> Db {
        let dir = std::env::temp_dir().join(format!("silver-chat-store-{}", uuid::Uuid::now_v7()));
        let db = Db::open(&dir.join("state.db")).await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    async fn add(db: &Db, kind: EntryKind, thread: Option<&str>, text: &str) -> ChatEntry {
        let mut entry = new_entry("chat", thread, kind);
        entry.text = text.into();
        entry.author = (kind == EntryKind::Agent).then(|| "bot".into());
        db.chat_insert_entry(entry).await.unwrap()
    }

    #[tokio::test]
    async fn replies_are_unread_until_their_lane_is_read() {
        let db = database().await;
        add(&db, EntryKind::User, None, "hi").await;
        let reply = add(&db, EntryKind::Agent, None, "hello").await;
        let unread = |db: &Db| {
            let db = Db::clone(db);
            async move { db.chat_bot_stats().await.unwrap()["chat"].unread }
        };
        assert_eq!(unread(&db).await, 1);
        assert!(db.chat_mark_read("chat", None).await.unwrap());
        assert!(
            !db.chat_mark_read("chat", None).await.unwrap(),
            "nothing new to read"
        );
        assert_eq!(unread(&db).await, 0);

        // A thread is read on its own, and the main chat's mark does not cover it.
        add(&db, EntryKind::Agent, Some(&reply.id), "in the thread").await;
        assert_eq!(unread(&db).await, 1);
        let summary = db
            .chat_thread_summaries("chat", vec![reply.id.clone()])
            .await
            .unwrap();
        let summary = &summary[&reply.id];
        assert_eq!((summary.count, summary.unread), (1, 1));
        assert_eq!(summary.authors, ["bot"]);
        db.chat_mark_read("chat", Some(&reply.id)).await.unwrap();
        assert_eq!(unread(&db).await, 0);
    }

    #[tokio::test]
    async fn a_page_is_the_newest_entries_oldest_first() {
        let db = database().await;
        for n in 0..5 {
            add(&db, EntryKind::User, None, &n.to_string()).await;
        }
        let page = db.chat_entries("chat", None, None, 3).await.unwrap();
        assert_eq!(
            page.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["2", "3", "4"]
        );
        let older = db
            .chat_entries("chat", None, Some(page[0].seq), 3)
            .await
            .unwrap();
        assert_eq!(
            older.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["0", "1"]
        );
    }

    #[tokio::test]
    async fn a_restart_finishes_what_was_half_done() {
        let db = database().await;
        let mut partial = new_entry("chat", None, EntryKind::Agent);
        partial.text = "half a reply".into();
        partial.is_final = false;
        let partial = db.chat_insert_entry(partial).await.unwrap();
        let mut empty = new_entry("chat", None, EntryKind::Agent);
        empty.is_final = false;
        let empty = db.chat_insert_entry(empty).await.unwrap();
        let mut queued = new_entry("chat", None, EntryKind::User);
        queued.status = Some("queued".into());
        let queued = db.chat_insert_entry(queued).await.unwrap();

        db.chat_recover().await.unwrap();
        assert!(db.chat_entry(&partial.id).await.unwrap().unwrap().is_final);
        assert!(db.chat_entry(&empty.id).await.unwrap().is_none());
        let queued = db.chat_entry(&queued.id).await.unwrap().unwrap();
        assert_eq!(queued.status.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn removing_a_workspace_leaves_its_bots_without_one() {
        let db = database().await;
        let now = chrono::Utc::now();
        let root = std::env::temp_dir();
        let workspace = db
            .create_workspace(silver_core::workspace::Workspace {
                id: WorkspaceId::new(),
                name: "app".into(),
                canonical_root: root.clone(),
                root,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        let bot = db
            .save_chat_bot(BotRow {
                id: "bot".into(),
                kind: BotKind::Agent,
                name: "Reviewer".into(),
                description: String::new(),
                instructions: String::new(),
                avatar_shape: "blob".into(),
                avatar_color: "blue".into(),
                provider: None,
                model: None,
                reasoning_effort: None,
                workspace_id: Some(workspace.id),
                yolo: false,
                members: Vec::new(),
                pinned: false,
                epoch: 0,
                created_at: now_ms(),
            })
            .await
            .unwrap();
        db.delete_workspace(workspace.id).await.unwrap();
        let bot = db.chat_bot(&bot.id).await.unwrap().unwrap();
        assert_eq!(bot.workspace_id, None);
    }
}
