-- The session ledger.
--
-- The orchestrator's accumulated context is the product, and until now its
-- loss was invisible: a failed `--resume` silently starts a fresh session
-- (orchestrator.rs), logged at warn, while the chat continues looking
-- seamless. One row per Claude Code session the orchestrator has lived in
-- makes that loss a fact you can read, and `last_context_tokens` makes the
-- resource itself measurable -- there is no way to choose a compaction
-- threshold, or to audit which memory regime produced a verdict, without it.
--
-- `summary` is unwritten for now: it is where owned rotation will store the
-- continuation note it seeds the next session with.
CREATE TABLE orchestrator_sessions (
    cc_session_id        TEXT PRIMARY KEY,
    started_at           TEXT NOT NULL,
    ended_at             TEXT,
    end_reason           TEXT,        -- 'resume_failed' | 'rotated'
    last_context_tokens  INTEGER,
    summary              TEXT,
    summary_generated_at TEXT
);

CREATE INDEX orchestrator_sessions_started_idx ON orchestrator_sessions(started_at);

-- Which memory regime produced a turn. Only assistant replies carry one --
-- user and event turns are input, written by the server, and belong to no
-- session; seam rows describe the boundary itself. NULL on every row written
-- before the ledger existed.
ALTER TABLE orchestrator_messages ADD COLUMN cc_session_id TEXT;

-- Adopt the session already in flight, so the ledger is not blind to the
-- conversation it was introduced into. Its true start time is unknowable --
-- the first message's timestamp is the closest honest answer.
INSERT INTO orchestrator_sessions (cc_session_id, started_at)
SELECT o.cc_session_id,
       COALESCE((SELECT MIN(created_at) FROM orchestrator_messages),
                strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
FROM orchestrator o
WHERE o.id = 1 AND o.cc_session_id IS NOT NULL;
