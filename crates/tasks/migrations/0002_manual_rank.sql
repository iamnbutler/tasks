-- Human-authoritative queue ordering. NULL means "unranked": those tasks sort
-- after every manually ranked task. The GitHub poller must never write this.
ALTER TABLE tasks ADD COLUMN manual_rank INTEGER;

CREATE INDEX tasks_manual_rank_idx ON tasks(manual_rank);
