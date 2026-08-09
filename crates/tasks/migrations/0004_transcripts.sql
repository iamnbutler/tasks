-- Per-session agent transcripts.
--
-- A table rather than per-session files: the read API is `?since=<seq>&limit=`,
-- which is exactly an indexed range scan on this primary key. A file would need
-- a parallel seq->offset index to answer the same question, plus its own cleanup
-- path and its own half-written-line failure mode. ON DELETE CASCADE means
-- transcript lifetime is free.
--
-- Deliberately NOT the event log: transcripts are a high-rate channel only an
-- open session-detail view subscribes to, while every client refetches on every
-- event. See docs/clients.md § "Event volume".
CREATE TABLE transcript_lines (
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,           -- dense, per session, assigned at persist time
    timestamp   TEXT NOT NULL,
    stream      TEXT NOT NULL,              -- stdout / stderr
    line        TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

-- Token usage / cost parsed from the stream-json `result` record. JSON because
-- the shape is Claude Code's, not ours; every field is optional so a renamed
-- key costs a null rather than a failed scout.
ALTER TABLE sessions ADD COLUMN agent_usage TEXT;
