-- `done` means shipped (#859).
--
-- Until now a successful build wrote `done` in the same transaction that
-- recorded `pr_number`, so `done` meant "a PR exists". Nothing ever closed the
-- issue, so `done` + open-issue accumulated; and a PR closed unmerged left its
-- task reading `done` forever having shipped nothing. A successful build now
-- parks its tasks in `awaiting_merge`, and `done` is written in exactly one
-- place — closure-derived retirement — so it always means "the issue is closed
-- upstream".
--
-- `tasks.state` is plain TEXT with no CHECK constraint, so the new state needs
-- no schema change. What it needs is the rows the old rule left behind: tasks
-- reading `done` whose issue is still open, which the new poll pass will
-- resolve one PR read at a time.
--
-- Both conditions in the WHERE are load-bearing. `gh_state = 'open'` because a
-- *closed* issue is a real retirement, whoever performed it, and must not be
-- reopened as pipeline state. The build join because a `done` task with no
-- succeeded build carrying a PR behind it was retired some other way, and this
-- migration has no business guessing at that.
UPDATE tasks
   SET state = 'awaiting_merge',
       updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
 WHERE state = 'done'
   AND gh_state = 'open'
   AND EXISTS (
       SELECT 1
         FROM specs s
         JOIN build_specs bs ON bs.spec_id = s.id
         JOIN builds b ON b.id = bs.build_id
        WHERE s.task_id = tasks.id
          AND b.status = 'succeeded'
          AND b.pr_number IS NOT NULL
   );
