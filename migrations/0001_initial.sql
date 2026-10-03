-- silver schema.
--
-- Applied once, inside a transaction, to an empty database (user_version 0).

CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL,
    canonical_root_path TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NULL REFERENCES workspaces(id),
    source TEXT NOT NULL,
    external_key TEXT NULL,
    title TEXT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    -- A model pinned for the session's runs; an explicit request model still wins. NULL or
    -- blank means the daemon default.
    model_override TEXT,
    yolo_mode INTEGER NOT NULL DEFAULT 0,
    -- The provider the model override belongs to, so resuming routes there even after another
    -- provider was activated. NULL follows the active provider.
    provider_override TEXT,
    -- The preset a session runs with; NULL is the built-in minimal preset.
    preset TEXT,
    -- The standing /goal as JSON ({objective, status, used, max}); NULL when none.
    goal TEXT,
    -- 'entered' (turned on since the last run), 'active' or 'exited' (left since the last run);
    -- NULL when off.
    plan_mode TEXT,
    -- none|minimal|low|medium|high; NULL or blank means the daemon default.
    reasoning_effort TEXT
);

CREATE UNIQUE INDEX sessions_external_key_scope
ON sessions(source, external_key, COALESCE(workspace_id, '__global__'))
WHERE external_key IS NOT NULL;

CREATE INDEX sessions_scope_updated
ON sessions(COALESCE(workspace_id, '__global__'), updated_at DESC);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    workspace_id TEXT NULL REFERENCES workspaces(id),
    status TEXT NOT NULL,
    model TEXT NOT NULL,
    created_at TEXT NOT NULL,
    started_at TEXT NULL,
    finished_at TEXT NULL,
    error_code TEXT NULL,
    error_message TEXT NULL,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    -- From the Idempotency-Key request header. Unique per session when present, so a retried
    -- POST /v1/runs returns the original run; NULL keys are exempt from the index.
    idempotency_key TEXT,
    -- Advisory list-price estimate, never billing-accurate. NULL when the model is unpriced or
    -- the provider reported no usage.
    cost_usd REAL
);

CREATE INDEX runs_session_created ON runs(session_id, created_at DESC);
CREATE INDEX runs_status ON runs(status);

CREATE UNIQUE INDEX runs_idempotency_session_key
ON runs(session_id, idempotency_key)
WHERE idempotency_key IS NOT NULL;

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    run_id TEXT NULL REFERENCES runs(id),
    role TEXT NOT NULL,
    content_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    -- The plain-text projection the FTS indexes read. content_json is deliberately not
    -- indexed: its JSON scaffolding ("text", "type", argument keys) would make every message
    -- a hit for common code words. Tool rows are truncated to 8192 characters.
    text TEXT NOT NULL DEFAULT '',
    tool_name TEXT,
    finish_reason TEXT,
    token_count INTEGER
);

CREATE INDEX messages_session_created ON messages(session_id, created_at);

-- External-content FTS5 indexes over (role, text). message_fts_src exposes the implicit rowid
-- the triggers and the MATCH join key on. unicode61 serves word recall and BM25 ranking; the
-- trigram index answers interior-substring queries (CJK, "caten" in "concatenate") and is only
-- consulted when unicode61 finds nothing.
CREATE VIEW message_fts_src AS
    SELECT rowid, role, text FROM messages;

CREATE VIRTUAL TABLE message_fts USING fts5(
    role,
    text,
    content='message_fts_src',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER messages_fts_ai AFTER INSERT ON messages BEGIN
    INSERT INTO message_fts(rowid, role, text) VALUES (new.rowid, new.role, new.text);
END;

CREATE TRIGGER messages_fts_ad AFTER DELETE ON messages BEGIN
    INSERT INTO message_fts(message_fts, rowid, role, text)
    VALUES ('delete', old.rowid, old.role, old.text);
END;

CREATE TRIGGER messages_fts_au AFTER UPDATE OF role, text ON messages BEGIN
    INSERT INTO message_fts(message_fts, rowid, role, text)
    VALUES ('delete', old.rowid, old.role, old.text);
    INSERT INTO message_fts(rowid, role, text) VALUES (new.rowid, new.role, new.text);
END;

CREATE VIRTUAL TABLE message_fts_trigram USING fts5(
    role,
    text,
    content='message_fts_src',
    content_rowid='rowid',
    tokenize='trigram'
);

CREATE TRIGGER messages_fts_trigram_ai AFTER INSERT ON messages BEGIN
    INSERT INTO message_fts_trigram(rowid, role, text) VALUES (new.rowid, new.role, new.text);
END;

CREATE TRIGGER messages_fts_trigram_ad AFTER DELETE ON messages BEGIN
    INSERT INTO message_fts_trigram(message_fts_trigram, rowid, role, text)
    VALUES ('delete', old.rowid, old.role, old.text);
END;

CREATE TRIGGER messages_fts_trigram_au AFTER UPDATE OF role, text ON messages BEGIN
    INSERT INTO message_fts_trigram(message_fts_trigram, rowid, role, text)
    VALUES ('delete', old.rowid, old.role, old.text);
    INSERT INTO message_fts_trigram(rowid, role, text) VALUES (new.rowid, new.role, new.text);
END;

CREATE TABLE tool_calls (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    name TEXT NOT NULL,
    arguments_json TEXT NOT NULL,
    risk TEXT NOT NULL,
    status TEXT NOT NULL,
    output_json TEXT NULL,
    created_at TEXT NOT NULL,
    finished_at TEXT NULL
);

CREATE INDEX tool_calls_run ON tool_calls(run_id, created_at);

CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    tool_call_id TEXT NOT NULL,
    arguments_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    decided_at TEXT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX approvals_run ON approvals(run_id);

CREATE TABLE run_events (
    run_id TEXT NOT NULL REFERENCES runs(id),
    sequence INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_id, sequence)
);

CREATE TABLE memory_changes (
    id TEXT PRIMARY KEY,
    run_id TEXT NULL REFERENCES runs(id),
    workspace_id TEXT NULL REFERENCES workspaces(id),
    file TEXT NOT NULL,
    operation TEXT NOT NULL,
    before_hash TEXT NOT NULL,
    after_hash TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX memory_changes_scope ON memory_changes(workspace_id, created_at DESC);

-- One filesystem snapshot per path captured before a destructive edit. snapshot_dir points at
-- the shadow store holding the bytes; the row is a small index entry. run_id is NULL when the
-- snapshot is not tied to an admitted run. kind is free-form (for example 'pre_write').
CREATE TABLE checkpoints (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    run_id TEXT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    bytes INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    snapshot_dir TEXT NULL
);

CREATE INDEX checkpoints_session_created
ON checkpoints(session_id, created_at DESC);

-- One advisory list-price cost observation per model call, so spend can be summed over a
-- wall-clock window. Never billing-accurate.
CREATE TABLE spend_events (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    cost_usd REAL NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX spend_events_created
ON spend_events(created_at);

-- The user's presets: a named tool and skill selection a session runs with. The built-in
-- presets (minimal, pi) live in code.
CREATE TABLE presets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    tools TEXT NOT NULL,   -- JSON array of tool names
    skills TEXT NOT NULL   -- JSON SkillFilter: {"except": [...]} or {"only": [...]}
);

-- One row per ingested file, scoped to its workspace, plus the extracted text split into
-- ~2000-character chunks. Two FTS5 indexes over the chunks mirror the message index. Ingestion
-- is explicit (search_documents with a path), so the doubled write cost is paid only for a
-- document the model was actually asked about.
CREATE TABLE documents (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    indexed_at TEXT NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (workspace_id, path)
);

CREATE TABLE document_chunks (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    text TEXT NOT NULL
);

CREATE INDEX document_chunks_document ON document_chunks(document_id, ordinal);

CREATE VIEW document_fts_src AS
    SELECT rowid, text FROM document_chunks;

CREATE VIRTUAL TABLE document_fts USING fts5(
    text,
    content='document_fts_src',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER document_chunks_fts_ai AFTER INSERT ON document_chunks BEGIN
    INSERT INTO document_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TRIGGER document_chunks_fts_ad AFTER DELETE ON document_chunks BEGIN
    INSERT INTO document_fts(document_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
END;

CREATE TRIGGER document_chunks_fts_au AFTER UPDATE OF text ON document_chunks BEGIN
    INSERT INTO document_fts(document_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
    INSERT INTO document_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE VIRTUAL TABLE document_fts_trigram USING fts5(
    text,
    content='document_fts_src',
    content_rowid='rowid',
    tokenize='trigram'
);

CREATE TRIGGER document_chunks_trigram_ai AFTER INSERT ON document_chunks BEGIN
    INSERT INTO document_fts_trigram(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TRIGGER document_chunks_trigram_ad AFTER DELETE ON document_chunks BEGIN
    INSERT INTO document_fts_trigram(document_fts_trigram, rowid, text)
    VALUES ('delete', old.rowid, old.text);
END;

CREATE TRIGGER document_chunks_trigram_au AFTER UPDATE OF text ON document_chunks BEGIN
    INSERT INTO document_fts_trigram(document_fts_trigram, rowid, text)
    VALUES ('delete', old.rowid, old.text);
    INSERT INTO document_fts_trigram(rowid, text) VALUES (new.rowid, new.text);
END;
