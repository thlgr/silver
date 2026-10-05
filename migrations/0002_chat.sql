-- Bot chat: a roster of named bots (and groups of them), the messages in each chat or thread,
-- and what the user has read. A bot's agent turns run in ordinary sessions (source 'chat'); these
-- tables hold only what the chat shows.

CREATE TABLE bots (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,             -- 'agent' or 'group'
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    instructions TEXT NOT NULL DEFAULT '',
    avatar_shape TEXT NOT NULL,
    avatar_color TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    workspace_id TEXT,
    yolo INTEGER NOT NULL DEFAULT 0,
    members TEXT NOT NULL DEFAULT '[]',   -- JSON array of bot ids, for a group
    pinned INTEGER NOT NULL DEFAULT 0,
    -- Bumped by "New session" and when the folder changes, so the bot starts a fresh session.
    epoch INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL           -- unix milliseconds
);

CREATE TABLE chat_entries (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    chat_id TEXT NOT NULL,
    thread_id TEXT,                 -- the root entry's id, for a reply
    kind TEXT NOT NULL,             -- 'user', 'agent', 'notice' or 'permission'
    author TEXT,
    text TEXT NOT NULL DEFAULT '',
    status TEXT,
    style TEXT,
    run_id TEXT,
    session_id TEXT,
    nonce TEXT,
    data TEXT NOT NULL DEFAULT '{}',      -- JSON: reactions, permission
    created_at INTEGER NOT NULL           -- unix milliseconds
);

CREATE INDEX chat_entries_lane ON chat_entries(chat_id, thread_id, seq);

-- The newest entry the user has seen in the main chat (thread_id '') or in one thread.
CREATE TABLE chat_reads (
    chat_id TEXT NOT NULL,
    thread_id TEXT NOT NULL DEFAULT '',
    read_seq INTEGER NOT NULL,
    PRIMARY KEY (chat_id, thread_id)
);
