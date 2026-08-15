-- Two capabilities the charter was missing, because the endpoints behind them
-- did not exist.
--
-- The pipeline dead-ended at "PR open". A Builder opened one and nothing in
-- the system could ever comment on it, merge it, or close it — not because
-- that authority had been withheld, but because `GitHubClient` had exactly
-- three write methods and none of them was any of those. The orchestrator
-- would review a PR, reach a verdict, and hand it back as prose for a human
-- to re-read and re-type. Work done, discarded at the last step: the same
-- waste `shadow` produced, arriving by a different road.
--
--   * `comment_on_work` — say something on an issue or a PR. The lightest
--     write here; its recourse is deleting a comment.
--   * `land_builds` — decide a Builder PR's fate. `dispatch_builds` starts
--     the run, this finishes it, and the two are deliberately separate: the
--     failure modes are unrelated and so is the evidence each one needs.
--   * `curate_work` — revise work already filed: rewrite an issue's body,
--     change its labels. Separate from `capture_work` because it rewrites
--     rather than appends. A bad capture leaves a bad issue behind; a bad
--     edit destroys a good one. The endpoint answers that by reading the
--     current text before writing and storing it on the decision unasked,
--     because "the orchestrator edited #835" is not an auditable record —
--     the diff is.
--
-- Both ship `live` and uncapped, like the other five (see 0016). `land_builds`
-- is the first capability whose recourse is a revert rather than an edit, and
-- it is worth being clear that this is the reason it ships live rather than an
-- exception to it. A merge gate would put the human in the loop on every
-- Builder PR, which is exactly the attention this system exists to give back;
-- what makes it safe is the same thing that makes the rest safe — a decisions
-- row under every merge, naming the actor, the rationale, and the SHA. If it
-- misbehaves, demote it, and the reasoning is still on record.
--
-- The handlers add one requirement beyond the charter: an autonomous merge or
-- abandon is refused outright without a stated rationale. Not a permission
-- check — the capability is live either way — but a merge whose ledger row
-- reads only "merged" is a row nobody can review afterwards, and the whole
-- argument for acting first rests on the review being possible later.

INSERT INTO orchestrator_charter (capability, level, params, updated_at) VALUES
    ('comment_on_work', 'live', NULL, '1970-01-01T00:00:00Z'),
    ('land_builds',     'live', NULL, '1970-01-01T00:00:00Z'),
    ('curate_work',     'live', NULL, '1970-01-01T00:00:00Z');
