-- Task lifecycle rename: one state per stage, queue membership is explicit.
--
--   new        -> backlog        (ingested, not picked up; never dispatched)
--   spec_ready -> in_review      (spec produced, awaiting a verdict)
--   queued     -> ready_to_build (approved spec parked for a Builder run)
--
-- `queued` is re-purposed to mean "explicitly added to the scout queue"; no
-- existing row carries that meaning, so the old value must be remapped first.
-- scouting / done / rejected are unchanged.
UPDATE tasks SET state = 'ready_to_build' WHERE state = 'queued';
UPDATE tasks SET state = 'backlog'        WHERE state = 'new';
UPDATE tasks SET state = 'in_review'      WHERE state = 'spec_ready';
