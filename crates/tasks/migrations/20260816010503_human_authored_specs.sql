-- `specs.session_id` becomes nullable: NULL is the tell that no Scout ran and
-- a human wrote the spec by hand (`POST /tasks/{id}/build-now`).
--
-- SQLite cannot drop a NOT NULL, so this is the copy/drop/rename dance 0021
-- did. The wrinkle 0021 did not have is that `specs` is a *parent*:
-- `spec_queue.spec_id` cascades off it and `build_specs.spec_id` references it
-- with no ON DELETE action at all. `DROP TABLE specs` with foreign keys
-- enforced runs an implicit `DELETE FROM specs`, which would cascade the whole
-- queue away and trip `build_specs`.
--
-- SQLite's own recipe for this says to turn foreign keys off around the swap,
-- but `PRAGMA foreign_keys` is a silent no-op inside a transaction and sqlx
-- runs each migration in one. `-- no-transaction` would buy the pragma at the
-- price of a half-migrated database on any failure, which is a bad trade for a
-- schema change that a boot depends on.
--
-- So the children are lifted out of the blast radius instead: copied into TEMP
-- tables, deleted, and re-inserted verbatim after the swap. Same effect as
-- suspending enforcement, still one atomic transaction.
--
-- The alternative considered and rejected was a sentinel `sessions` row to
-- point hand-written specs at. `sessions` is what reattach, the transcript
-- views and every running-work query enumerate, so a fake one would have to be
-- filtered out of all of them, forever, and any missed filter surfaces as a
-- phantom scout run rather than as an error.
CREATE TEMP TABLE spec_queue_carry AS SELECT * FROM spec_queue;
CREATE TEMP TABLE build_specs_carry AS SELECT * FROM build_specs;
DELETE FROM build_specs;
DELETE FROM spec_queue;

CREATE TABLE specs_new (
    id              TEXT PRIMARY KEY,
    -- NULL means no Scout ran: the spec was written by a human, and the
    -- review that would have judged a Scout's spec is the writing of it.
    session_id      TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    task_id         TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    content         TEXT NOT NULL,
    complexity      TEXT NOT NULL,                -- simple / medium / complex
    files_touched   TEXT NOT NULL,                -- JSON array
    created_at      TEXT NOT NULL
);

INSERT INTO specs_new (id, session_id, task_id, content, complexity, files_touched, created_at)
SELECT id, session_id, task_id, content, complexity, files_touched, created_at FROM specs;

DROP TABLE specs;
-- Renaming onto a name that `spec_queue` and `build_specs` still reference
-- while no table holds it: a foreign key pointing at a missing table is not a
-- schema parse error, so this needs no `PRAGMA legacy_alter_table`. Pinned by
-- a test rather than trusted.
ALTER TABLE specs_new RENAME TO specs;

CREATE INDEX specs_task_idx ON specs(task_id);

INSERT INTO spec_queue SELECT * FROM spec_queue_carry;
INSERT INTO build_specs SELECT * FROM build_specs_carry;

DROP TABLE spec_queue_carry;
DROP TABLE build_specs_carry;
