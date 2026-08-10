# Configurable intake label filter for the GitHub poller (#761)

Adds an optional `TASKS_INTAKE_LABEL` env var that restricts GitHub intake to
open issues carrying that label. Unset — or blank, which is treated as unset so
a bare `TASKS_INTAKE_LABEL=` in a `.env` can't silently halt all intake —
preserves today's behaviour exactly: every open issue in every registered
project becomes a task. The filter is a new `IntakeFilter` enum in
`crates/tasks/src/github.rs` (`All` / `Label(String)`, matched
case-insensitively because GitHub itself refuses two labels differing only in
ASCII case), resolved once into `Config` and applied by `poll_once` to the
*upsert* half of the poll pass only. `poll_loop` logs at startup whether intake
is restricted and to what, since a typo would otherwise ingest nothing and say
nothing. The change also lifts the query's `labels(first: 20)` to a
`$labelFirst` variable of 100 — GitHub's own per-issue cap — because with a
filter in play a truncated label list stops being a cosmetically stale snapshot
and becomes an issue that is never ingested with nothing to explain why.

The load-bearing detail is *where* the filter runs: after the fetch, in
`poll_once`, never as a `labels:` argument on the GraphQL query.
`Store::reconcile_closed_issues` infers upstream closure from absence from the
open set, so it must keep receiving the complete open set; filtering in the
query would make every task whose issue merely lost the label look closed.
`open_numbers` is accordingly computed before the filter, with a comment saying
so. Three consequences follow and each has a test: an issue that *gains* the
label is ingested on the next poll as an ordinary first sighting; a task whose
issue *loses* the label is kept exactly as it is (same row, queue position,
state and `dispatch_attempts`) and simply stops having its snapshot refreshed,
because un-labelling is not a retraction mechanism — pulling work back is the
API's job; and such a task still tracks upstream closure correctly. Note that
turning the filter on does not retroactively purge tasks ingested before it was
set, and it saves no API cost — the poller still pages through every open issue,
deliberately. Covered by four unit tests in `github.rs` and six integration
tests in `tests/run.rs` (including one that exercises the
`Config` → `poll_loop` → `poll_once` wiring, not just the predicate);
`cargo fmt --all --check` is clean, `cargo clippy --workspace --all-targets`
adds no new warnings, and `cargo test --workspace` is green.
