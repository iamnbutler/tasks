-- A spec a human wrote by hand has no scout session (#869).
--
-- `POST /tasks/{id}/build-now` writes a spec for a task whose issue body
-- already *is* the specification, so there is no Scout run behind it and
-- nothing for `specs.session_id` to point at. A NULL there is the tell, and
-- it is the only one: everything downstream — the queue entry, the Builder
-- prompt, `POST /builds` — is unchanged, because a spec is text and a Builder
-- cannot tell who typed it.
--
-- The alternative was a sentinel `sessions` row, and it is worse. `sessions`
-- is what reattach, the transcript views and every "what is running" query
-- enumerate, so a fake one would have to be filtered out of each of them, for
-- good, by everyone who ever adds another such query.
--
-- SQLite cannot drop a NOT NULL, so this is the copy/drop/rename dance. The
-- wrinkle 0021 did not have is that `specs` is a **parent**: `spec_queue`
-- cascades off it and `build_specs` references it with no ON DELETE action at
-- all. `DROP TABLE specs` with foreign keys enforced runs an implicit
-- `DELETE FROM specs`, which would cascade the review queue away and trip
-- `build_specs` on the way.
--
-- SQLite's own recipe for this says to turn foreign keys off around the swap,
-- but `PRAGMA foreign_keys` is a silent no-op inside a transaction and sqlx
-- runs every migration in one. `-- no-transaction` would buy the pragma at
-- the price of a half-migrated database on any failure, which is a bad trade
-- for a schema this load-bearing. So the children are lifted into temp
-- tables, emptied, and re-inserted verbatim after the swap: same effect, and
-- still atomic.
CREATE TEMP TABLE spec_queue_carry AS SELECT * FROM spec_queue;
CREATE TEMP TABLE build_specs_carry AS SELECT * FROM build_specs;
DELETE FROM spec_queue;
DELETE FROM build_specs;

CREATE TABLE specs_new (
    id              TEXT PRIMARY KEY,
    -- NULL means no Scout ran: a human wrote this spec and approved it in the
    -- same act. Not nullable for a scout's benefit — a scouted spec always
    -- has one.
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
ALTER TABLE specs_new RENAME TO specs;

-- Dropped with the old table, so it has to come back by hand.
CREATE INDEX specs_task_idx ON specs(task_id);

INSERT INTO spec_queue SELECT * FROM spec_queue_carry;
INSERT INTO build_specs SELECT * FROM build_specs_carry;
DROP TABLE spec_queue_carry;
DROP TABLE build_specs_carry;
