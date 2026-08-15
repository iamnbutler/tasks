# `Done` means shipped: an `awaiting_merge` state and a poller-driven close-on-merge

`Store::finalize_build_succeeded` moved tasks `building → done` in the same
transaction that recorded `pr_number`, so `done` meant "a PR exists" rather than
"the work shipped". Nothing ever closed the issue, so `done` + open-issue
accumulated; and a PR closed unmerged left its task reading `done` forever
having shipped nothing. This adds one state and one poll pass to take those two
facts apart. A successful build now parks its batch in a new
`TaskState::AwaitingMerge`, and a new per-project pass (`run::watch_merges`,
running after the existing retirement pass) reads each unresolved pull request
at decision time: merged → the server closes the issue as `completed` under the
`retire_work` capability, with the merge commit as evidence and a new
`Actor::System` on the ledger row, and the *next* poll retires the task through
the ordinary closure-derived path; closed unmerged → `unwind_unmerged_build`
returns the specs to `approved` with a build attempt charged (blocking them at
`MAX_BUILD_ATTEMPTS`) and the tasks to `ready_to_build`; still open → nothing,
and the next poll asks again. `done` is therefore written in exactly one place
and always means "the issue is closed upstream". Nothing rebuilds by itself —
builds are only ever dispatched explicitly — so unwinding restores the option to
rebuild rather than triggering one, and the succeeded build row is left untouched
because it did succeed. The PR answer is never cached: the cost is one REST call
per open Builder PR per poll, bounded by the fact that builds are serial.

Two smaller pieces ride along. Task listings (`list_tasks` and
`list_active_tasks`, documented as ordering identically) gain a leading
`ORDER BY` term that sorts terminal states last, spelled out in
`TaskState::ORDER_TERMINAL_LAST_SQL` next to `is_terminal()` with a unit test
that fails when the two drift; only the terminal group is pulled out, since
grouping the whole pipeline by state would override `manual_rank`. And migration
`0024` converts existing `done` + open-issue rows that have a PR behind them
into `awaiting_merge` so the new pass resolves them — a closed issue is a real
retirement and is left alone, as is a `done` task with no build behind it.
`awaiting_merge` is live work: it counts as active, stays visible in the default
task listing even once its issue closes, and appears in the app's Queue between
Building and Up next. Tests: five integration cases in `crates/tasks/tests/merges.rs`
driving `poll_once` against a loopback GitHub serving both GraphQL and REST
(merged across two polls, closed-unmerged, an *open* PR carrying GitHub's
speculative `merge_commit_sha`, charter `off`, charter `shadow` across two
polls), plus store unit tests for the batch listing, the strike cap, terminal
sort order, `awaiting_merge` retirement and the migration itself. Verified with
`cargo test --workspace`, `cargo clippy --workspace --all-targets` and
`cargo fmt --check`; `app-gpui` (excluded from the workspace) could not be
compiled in this environment — it needs `pkg-config`/fontconfig, which are not
installed — so its three changes are a `TaskState` match arm, a `matches!` arm
and a new Queue group, made by hand.
