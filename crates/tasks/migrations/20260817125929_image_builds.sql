-- image_builds
--
-- What each VM image is running, as last observed by a run that started inside
-- it. A VM exists only while a run is inside it, so the `Started` event is the
-- only moment there is to ask — nothing polls an image.
--
-- One row per image *reference* (`agent:v1`), not per observation: the
-- question is "what is in there now", and an append-only log of the same
-- answer per dispatch would be noise. `observed_at` and `run_id` say when it
-- was last confirmed and by which run, so a reading can be traced back to a
-- transcript.
--
-- `version` and `commit_sha` are nullable because their *absence* is the
-- signal: an image built before there was an identity to send reports none,
-- which is strictly staler than any version it could have reported.
--
-- `commit_sha`, not `commit` — COMMIT is a SQLite keyword.
--
-- No verdict column. Freshness is a comparison against the running server's
-- own build, and the server is replaced far more often than the images are, so
-- a stored verdict would be stale the moment the next binary booted. It is
-- computed at read time in `Store::image_builds`.
CREATE TABLE image_builds (
    image       TEXT PRIMARY KEY,
    role        TEXT NOT NULL,
    version     TEXT,
    commit_sha  TEXT,
    observed_at TEXT NOT NULL,
    run_id      TEXT
);
