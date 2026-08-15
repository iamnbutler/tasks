-- The decisions ledger, and a cap on build retries.
--
-- Until now a review verdict left almost no trace: `spec_queue.feedback` is a
-- single mutable column the next verdict overwrites, and
-- `spec_queue_status_changed` carries no actor -- so "what was decided, by
-- whom, and why" was unanswerable an hour later. CLAUDE.md already calls for
-- append-only decisions; this is that table.
--
-- Two properties are load-bearing:
--
-- 1. A row is written in the SAME transaction as the state change it
--    authorizes. Events are appended after commit, which is fine for
--    telemetry and not fine for a record of authority -- the ledger must not
--    be able to disagree with the state it explains.
-- 2. `transcript_seq` points at the orchestrator turn whose prose is the
--    reasoning. It is backfilled when that turn's reply lands, because a
--    verdict is curled mid-turn: the explanation does not exist yet at the
--    moment the decision does. The ledger is an index into the wall of text,
--    not a replacement for it.
CREATE TABLE decisions (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    subject_kind   TEXT NOT NULL,        -- 'spec' | 'build'
    subject_id     TEXT NOT NULL,
    action         TEXT NOT NULL,        -- 'approve' | 'needs_revision' | 'reject' | 'request_build'
    actor          TEXT NOT NULL,        -- 'human' | 'orchestrator'
    rationale      TEXT,                 -- required of the orchestrator, optional for a human
    evidence       TEXT,                 -- JSON: what the decider checked
    transcript_seq INTEGER,              -- orchestrator_messages.seq holding the reasoning
    created_at     TEXT NOT NULL
);

CREATE INDEX decisions_subject_idx ON decisions(subject_kind, subject_id);
CREATE INDEX decisions_actor_idx ON decisions(actor);

-- Builds had no attempt counter at all: `finalize_build_failed` returns the
-- batch's specs to `approved`, so anything that automatically turns approved
-- specs into builds would retry a poison batch forever. Scouts have had
-- `MAX_DISPATCH_ATTEMPTS` since 0003; this is the same idea one diamond
-- along, and it has to exist before the dispatch loop that needs it.
ALTER TABLE spec_queue ADD COLUMN build_attempts INTEGER NOT NULL DEFAULT 0;
