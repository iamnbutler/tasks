# Recover PR #863's work, and stop a stacked "merged" PR reading as shipped

PR #863 — the whole of #859: `TaskState::AwaitingMerge`, the `watch_merges` poll
pass, close-on-merge under a new `Actor::System` with the merge commit as
evidence, `unwind_unmerged_build`, `ORDER_TERMINAL_LAST_SQL` and its migration —
reads MERGED on GitHub and is on no branch that ships. Its content is recovered
in full from `refs/pull/863/head`, which GitHub keeps forever even though the
merge commit and the build branch are both unreachable now (that pull ref is the
recovery mechanism for any lost PR — not the merge commit, and not the branch).
The single payload commit is cherry-picked onto current `main` and reconciled
against the six PRs that landed since: the `MIGRATOR` move from #871, the
draggable-table rewrite of the queue section from #887 — where "Awaiting merge"
returns as a sixth band, deliberately `Reorder::Fixed` because it shares
`tasks.manual_rank` with the two draggable bands and a pull request, not the
list, decides its place — the factored `state::is_picked_up` predicate, and the
`row_menu.rs` added by #885, which the cherry-pick could not know about. The
migration is renamed `20260815164324_awaiting_merge.sql`, the UTC instant of the
original commit, which satisfies #871's guard and is safe precisely because the
only database that ever applied it was a build VM that no longer exists.

The recovery would otherwise have reintroduced the bug that lost it. The
recovered pass decided "shipped" from `pr.merged` alone — but `merged` is a
statement about the PR's *base*, and this pipeline stacks builds routinely.
**PR #863 was `merged: true`.** Had the #859 fix been running when it merged, it
would have closed #859 as completed and written `done` for work that shipped
nothing: the exact failure #859 exists to prevent, one level up. So
`watch_merges` now gates on `run::shipped` — merged *and* the merge commit is an
ancestor of the trunk (`SCOUT_BASE_BRANCH`). `base_ref == trunk` short-circuits,
so the ordinary unstacked case costs no extra API call at all; only a stacked PR
spends a `GET /compare/{trunk}...{sha}`, whose operand order is load-bearing
(compare reads head relative to base, so reachable is `identical` or `behind`,
and reversing it inverts the verdict). Every unreadable answer returns false,
because the two mistakes are not symmetric: staying parked costs one call on the
next poll and is undone by it, while concluding wrongly writes `done` over work
that shipped nothing and no pass ever revisits `done`. A batch that merged but
has not reached the trunk **stays parked rather than unwinding**, which is what
makes both manual merge orders safe — merge the base first and the commit is
already reachable, merge the dependent first and a later poll finds it once the
base lands — and reachability is monotone, so polling can never un-ship
something it already concluded had landed. Nothing auto-unwinds a stranded
batch; the new `ObligationKind::LandBatch` makes it loud instead, and since it
is the first obligation whose subject is a build id rather than a spec id,
`format_obligations` branches before constructing a `SpecId` from one and briefs
it with `Brief::for_stranded_build`, which spells out the trap when the base is
not the trunk.

Tests: the merge-watcher suite gains a `/compare/{basehead}` route on its fake
GitHub, recording every comparison asked for — which is what lets one test
assert the unstacked case really is free — plus cases for a merged PR whose
commit never reached the trunk (parked, not unwound, nothing closed), the
opposite stack order landing on a later poll, a merge naming no commit, and the
stranded-batch obligation. `cargo fmt`, `cargo clippy --workspace
--all-targets`, `make test` (549 passed, plus the three documented expected
LEAKs), `make app-check` and `make app-test` (99 passed) are all clean.
