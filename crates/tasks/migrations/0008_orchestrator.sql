-- Orchestrator MVP: a persistent, server-owned Claude Code conversation.
--
-- `orchestrator_messages` is the chat the human sees -- the app's Chat pane
-- reads it and POST /orchestrator/message appends to it. The *reasoning*
-- state (full agent transcript, tool calls) lives in Claude Code's own
-- session storage, keyed by `cc_session_id`; we resume that session on every
-- tick rather than reimplementing an agentic loop.
CREATE TABLE orchestrator_messages (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    role TEXT NOT NULL,          -- 'user' | 'assistant'
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Singleton, like `mode`.
CREATE TABLE orchestrator (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cc_session_id TEXT
);
INSERT INTO orchestrator (id, cc_session_id) VALUES (1, NULL);
