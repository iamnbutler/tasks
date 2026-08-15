-- Transcripts get a second owner: builds.
--
-- A build that ran its whole budget and committed nothing used to leave
-- nothing behind but timestamps -- `drain_build_events` consumed every
-- Progress line into a `debug!`. Rather than a second table and a second
-- writer, `transcript_lines` grows an exclusive arc: exactly one of
-- `session_id` / `build_id` is set, enforced by the CHECK.
--
-- The obvious alternative -- one `(owner_kind, owner_id)` pair -- is less SQL
-- but drops the ON DELETE CASCADE 0004 leaned on ("transcript lifetime is
-- free"), because SQLite has no polymorphic foreign key. Two nullable columns
-- keep both cascades. NULLs compare distinct in a SQLite unique index, which
-- is exactly what lets the unused side of the arc repeat freely while
-- `(owner, seq)` stays unique on the side that is set.
--
-- Copy-rename rather than two ALTERs: SQLite cannot add a CHECK to an
-- existing table, and the old composite PRIMARY KEY cannot be dropped.
-- Existing rows are session-owned, and their seqs come across untouched --
-- `seq` restarts at 1 per owner, so a build's first line is seq 1 no matter
-- what its specs' scout sessions recorded.
CREATE TABLE transcript_lines_new (
    session_id  TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    build_id    TEXT REFERENCES builds(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,           -- dense, per owner, assigned at persist time
    timestamp   TEXT NOT NULL,
    stream      TEXT NOT NULL,              -- stdout / stderr
    line        TEXT NOT NULL,
    CHECK ((session_id IS NULL) <> (build_id IS NULL))
);

INSERT INTO transcript_lines_new (session_id, build_id, seq, timestamp, stream, line)
SELECT session_id, NULL, seq, timestamp, stream, line FROM transcript_lines;

DROP TABLE transcript_lines;
ALTER TABLE transcript_lines_new RENAME TO transcript_lines;

CREATE UNIQUE INDEX idx_transcript_lines_session_seq
    ON transcript_lines(session_id, seq);
CREATE UNIQUE INDEX idx_transcript_lines_build_seq
    ON transcript_lines(build_id, seq);
