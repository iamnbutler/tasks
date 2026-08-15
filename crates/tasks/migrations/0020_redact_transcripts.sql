-- One-shot repairs that need real code, and the first one: scrub credentials
-- out of transcript lines written before the scrub moved onto the write path
-- (#840).
--
-- Agent stdout reached `transcript_lines` verbatim, so any git command that
-- echoed its remote — `git remote -v`, or most of git's own errors — wrote the
-- live `https://x-access-token:<token>@github.com/…` clone URL the server
-- handed its VM into a durable table served by `GET /sessions/{id}/transcript`.
-- The write path is fixed in `Store::append_transcript_lines`, which covers
-- everything from here on. It does nothing for rows already on disk, and those
-- rows can hold a token that is probably still valid, so the deployed instance
-- is not actually fixed at rest until they are rewritten.
--
-- A SQL-only sweep was rejected: SQLite has no regex, so it would mean a second
-- implementation of `redact` that could disagree with the first about where a
-- URL's authority ends, and that no test covers. Hence a marker row plus Rust —
-- `run_pending_maintenance` (store.rs) consumes it on the next boot, at either
-- constructor, and deletes it, so the scan happens once and never again.
--
-- The sweep is a mitigation, not an undo: it cannot un-serve anything already
-- read over the API, which is why it also tells the operator to rotate
-- GITHUB_TOKEN if it changed anything.
--
-- `pending_maintenance` is a general seam. A future repair that genuinely needs
-- code inserts a row here and adds an arm to `run_pending_maintenance`; a
-- marker that binary does not recognise is left in place rather than dropped,
-- because dropping it would lose a repair silently. Don't reach for this for
-- anything a migration can express in SQL.

CREATE TABLE pending_maintenance (
    name         TEXT PRIMARY KEY,
    requested_at TEXT NOT NULL
);

INSERT INTO pending_maintenance (name, requested_at)
VALUES ('redact_transcripts', '1970-01-01T00:00:00Z');
