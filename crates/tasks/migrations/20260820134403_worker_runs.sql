-- Worker runs: labor out of the orchestrator's conversation lane (#1053).
--
-- The orchestrator was three jobs multiplexed onto one serial turn lane —
-- judge, laborer, front desk — and the laborer half (composition
-- verification, suite runs, investigation) routinely ate the 900s turn the
-- human was waiting behind. A worker is that labor moved onto its own serial
-- lane: a fresh, disposable headless agent the server spawns on the host, per
-- job, whose result text returns to the conversation as a server-written
-- `[worker <job>]` turn.
--
-- A row per run rather than an in-memory job, for the reasons `cancellations`
-- is a table: the durable cancel path reads `(kind, id)` rows, a worker
-- interrupted by a restart has to be reportable at the next boot, and the
-- report belongs somewhere a human can re-read after the conversation moves
-- on.
CREATE TABLE workers (
    id           TEXT PRIMARY KEY,
    job          TEXT NOT NULL,              -- short label; heads the report turn
    prompt       TEXT NOT NULL,              -- the job, free text
    status       TEXT NOT NULL,              -- queued | running | succeeded | failed | cancelled
    created_at   TEXT NOT NULL,
    started_at   TEXT,
    completed_at TEXT,
    exit_reason  TEXT,
    report       TEXT                        -- full result text; the turn carries a bounded copy
);

-- Transcripts get a third owner. Same move as 0021 (builds), for the same
-- reason: one table and one writer rather than a copy per owner. Copy-rename
-- rather than ALTER, because SQLite cannot change the CHECK that enforces the
-- exclusive arc. NULLs compare distinct in a SQLite unique index, so the two
-- unused sides of the arc repeat freely while (owner, seq) stays unique on
-- the side that is set.
--
-- The streaming is the point, not a nicety: a worker running a 60-minute
-- suite that dies at test 800 has to leave "these three failures so far"
-- behind, not silence — the same property that puts a Scout's NOTES.md on a
-- 30s checkpoint. Nothing collected at the end survives an end that never
-- comes.
CREATE TABLE transcript_lines_new (
    session_id  TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    build_id    TEXT REFERENCES builds(id) ON DELETE CASCADE,
    worker_id   TEXT REFERENCES workers(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,           -- dense, per owner, assigned at persist time
    timestamp   TEXT NOT NULL,
    stream      TEXT NOT NULL,              -- stdout / stderr
    line        TEXT NOT NULL,
    CHECK (
        (session_id IS NOT NULL) + (build_id IS NOT NULL) + (worker_id IS NOT NULL) = 1
    )
);

INSERT INTO transcript_lines_new (session_id, build_id, worker_id, seq, timestamp, stream, line)
SELECT session_id, build_id, NULL, seq, timestamp, stream, line FROM transcript_lines;

DROP TABLE transcript_lines;
ALTER TABLE transcript_lines_new RENAME TO transcript_lines;

CREATE UNIQUE INDEX idx_transcript_lines_session_seq
    ON transcript_lines(session_id, seq);
CREATE UNIQUE INDEX idx_transcript_lines_build_seq
    ON transcript_lines(build_id, seq);
CREATE UNIQUE INDEX idx_transcript_lines_worker_seq
    ON transcript_lines(worker_id, seq);

-- Without this row the capability grants nothing: `Store::charter_entry`
-- reads a missing row as `off`, so the enum variant alone would produce a
-- capability that is silently refused, and the failure would look like a bug
-- in `authorize` rather than a missing migration.
--
-- `live` and uncapped, like the other ten. The charter is a kill switch, not
-- a promotion ladder, and what makes this one safe to ship live is that a
-- worker conveys labor, not authority: its report is input the orchestrator
-- weighs, its default command has no route to the pipeline API and no GitHub
-- credential, and every gated write still happens where the server can
-- attribute it. The cost of a bad dispatch is bounded host CPU time,
-- stoppable under `cancel_runs`.
INSERT INTO orchestrator_charter (capability, level, params, updated_at) VALUES
    ('dispatch_workers', 'live', NULL, '1970-01-01T00:00:00Z');
