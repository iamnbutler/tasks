-- Salvage from scout runs that ended without concluding (#835).
--
-- A scout that died before writing SPEC.md used to lose everything: no
-- checkpoint, no partial credit, and the next attempt re-derived from zero.
-- The obvious fix — report the partial spec — is worse than the bug, because a
-- half-explored spec entering the review queue looks finished. So there are two
-- files with two meanings: SPEC.md still means "I concluded", and NOTES.md
-- means "here is what I have so far". This table holds the second one.
--
-- A separate table rather than a column on `sessions`, because `GET /sessions`
-- must not carry a quarter-megabyte per interrupted run. Separate from `specs`
-- for a stronger reason: there must be no shape in which salvage reaches a
-- reviewer. Nothing joins this to `spec_queue`, and nothing ever should — the
-- only consumer is the next attempt's prompt, where it is quoted as an
-- explicitly unverified lead.
--
-- One row per session, upserted: each checkpoint supersedes the last, and the
-- final salvage supersedes the last checkpoint. Rows are written as checkpoints
-- ARRIVE, not at the end — two of #825's four failures were "orphaned by server
-- restart", and notes held only in memory would have died with the process.
-- The orphan sweep reads these rows to tell a session that checkpointed from
-- one that went silent.
CREATE TABLE scout_notes (
    session_id     TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    task_id        TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    reason         TEXT,                       -- why the run ended; NULL mid-run
    notes          TEXT NOT NULL,
    files_touched  TEXT NOT NULL,              -- JSON array
    updated_at     TEXT NOT NULL
);

-- `salvage_for_task` asks "the newest notes for this task", every dispatch.
CREATE INDEX scout_notes_task_idx ON scout_notes(task_id, updated_at);
