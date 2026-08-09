-- Consecutive failed scout dispatches for a task. Persisted rather than kept in
-- the dispatcher's memory so a restart can't wipe the strike count and let a
-- poison task retry forever. Reset to 0 when a scout produces a spec. Like
-- manual_rank, the GitHub poller must never write it.
ALTER TABLE tasks ADD COLUMN dispatch_attempts INTEGER NOT NULL DEFAULT 0;
