-- agent_directions
--
-- One labelled channel for telling an agent what to do, distinct from the
-- `rationale` on a decision: a rationale explains a judgment to whoever reads
-- the ledger afterwards, directions are addressed to the agent and change what
-- it does. Six nullable columns, three pairs of (text, author).
--
-- `tasks.scout_directions` is *staged* — it aims the next Scout run and is
-- deliberately not cleared at dispatch, so a VM death or a `needs_revision`
-- return does not silently leave the retry unaimed. The `sessions` and
-- `builds` pairs are the run's own record: a copy taken when the run started,
-- so re-aiming a task tomorrow cannot rewrite what a run that already happened
-- was told.
--
-- The author is a text column rather than a foreign key for the same reason
-- `decisions.actor` is: it is attribution, and an unrecognized value decays to
-- `human` rather than dropping the text.
ALTER TABLE tasks ADD COLUMN scout_directions TEXT;
ALTER TABLE tasks ADD COLUMN scout_directions_author TEXT;

ALTER TABLE sessions ADD COLUMN directions TEXT;
ALTER TABLE sessions ADD COLUMN directions_author TEXT;

ALTER TABLE builds ADD COLUMN directions TEXT;
ALTER TABLE builds ADD COLUMN directions_author TEXT;
