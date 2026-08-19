# The per-scout dispatch-hold read is now unskippable, not merely present

`top_up` re-reads the four standing dispatch holds once per scout (#948),
because each iteration of its loop starts a VM and a pause landing mid-pass
must stop the *next* one rather than merely the next pass. That fix was correct
and invisible to the suite: deleting the in-loop `dispatch_held` call left every
test green, `pause_blocks_new_dispatches` included. `dispatch_held_answers_from_live_state_every_time`
could not catch it either — a test of a predicate never observes a caller that
stopped calling it, which is the whole of #973.

The iteration-shaped integration test the issue proposes is not available, and
the Scout measured that rather than arguing it: nothing is awaited between the
hold read and `in_flight.spawn`, so "while the first scout is in flight" lasts
microseconds and a probe built to that shape fails identically against the
correct code and against the mutant. So the rule is pinned **structurally**, on
the `server::ledgered` precedent. A new `crates/tasks/src/dispatch_gate.rs`
answers *what is next* and *may I start it* in one call (`next_scout`), and the
only thing `top_up` can dispatch is a `Cleared` whose fields are private to that
module. `next_dispatchable` moves in with it and becomes **private** — that is
the load-bearing half, not the new enum: `run.rs` now has no route to a
`(Task, Project)` at all (it no longer calls `list_tasks`, and `scout.dispatch`
has exactly one call site), so a pass that starts a VM without re-reading the
holds cannot be written rather than being written and caught. `dispatch_held`
moves over verbatim with **all four** reasons — mode, GitHub, updates, and the
vm-pool hold that is already on this base — and stays the one place they live;
`pool_hold`/`announce_pool` follow it, with `pool_hold` `pub(crate)` for the
build lane's match guard, whose comment is repointed at the new home so the two
sites still name each other.

Two unit tests in the gate's own `mod tests` hold the two properties, one
mutation each, and neither catches the other's:
`a_hold_that_lands_after_the_pass_began_stops_the_next_scout` replays #948's
scenario (two queued tasks, take the first `Start` and reserve it in `skip`
exactly as `top_up` does, commit the hold between the two turns) with one leg
per reason and its release; `the_holds_are_read_after_the_scan_not_before_it`
puts every hold in force with the only task in `Backlog` and requires `Drained`,
which only a scan that ran first can say. `dispatch_held_answers_from_live_state_every_time`
and `next_dispatchable_skips_a_paused_repo_without_starving_the_queue` move over
too, the former gaining the update-hold and pool-hold legs it never had.

**Both mutations were applied to the finished tree and confirmed.** Mutation A
— replacing the `if dispatch_held(..)` inside `next_scout` with `if false` —
fails `a_hold_that_lands_after_the_pass_began_stops_the_next_scout` on "the
second scout of the same pass must see the pause (#948)"; the other three pass.
Mutation B — hoisting that call back above `next_dispatchable` — fails
`the_holds_are_read_after_the_scan_not_before_it`; the other three pass. Neither
touches `dispatch_held_answers_from_live_state_every_time`, which is #973's
complaint reproduced as an experiment and why keeping it was not enough on its
own.

## Review feedback

- **There are four holds, not three — `dispatch_held` must move with all
  four.** Done. The vm-pool hold was already on this base (#967, via
  `pool_health`), and `dispatch_held` moved with all four reasons intact plus
  `pool_hold`/`announce_pool`. The spec's three-hold description was read as
  stale and the code on the branch was taken as the newer fact.
- **Add the pool hold and its release as a fourth leg of
  `a_hold_that_lands_after_the_pass_began_stops_the_next_scout`.** Done — it
  has four legs, not three. One deviation to state plainly: the pool leg drives
  the record through `PoolHealth::observe(&Ok(PoolStatus{available: 0, ..}))`
  rather than by filling a real pool. `pool_hold` claims a probe at most once
  per `PROBE_INTERVAL` (5s), so a test that flipped a real pool's occupancy
  would be waiting on a wall clock rather than on the gate; the first probe of
  each test *is* spent against a real vm-pool over a real `ClientHandle` (no
  mocks — `NoRuntime`, per the house rule), and what the leg then measures is
  `dispatch_held` reading the record, which is the part at issue. The same
  treatment is used for the pool leg added to
  `dispatch_held_answers_from_live_state_every_time`, and the reason is written
  down at the `full()` helper.
- **Read `run.rs` and `CLAUDE.md` as they are on the branch; the spec's line
  numbers and its description of `dispatch_held` predate #979.** Done — see the
  stale-premise list below.
- **Make `Held` vs `Drained` earn its place at the call site.** Done. `top_up`
  now emits a distinct `debug!` per variant — "a dispatch hold landed mid-pass"
  versus "no eligible task left in the queue" — so the distinction is
  load-bearing for a reader answering "why is the pipeline idle", not only for
  the ordering test. The `NextScout` doc comment says so, and says what
  collapsing it to `Option<Cleared>` would cost.
- **Preserve `next_dispatchable` being private.** Done; it is private to
  `dispatch_gate` and its doc comment says that widening it back reopens the
  hole.
- **Preserve `dispatch_held_answers_from_live_state_every_time`.** Kept, moved,
  and extended with the update and pool legs.
- **Preserve the corrected comment on `pause_blocks_new_dispatches`.** Done.
  The assertions are unchanged; the comment now says the test pins the
  pass-level rule (which the pre-loop read alone satisfies), says why adjacency
  does not demonstrate the mid-pass window is closed, and names
  `a_hold_that_lands_after_the_pass_began_stops_the_next_scout` as where the
  other property is pinned.

## Directions

- **This build is stacked on `build/build_639333ce38b7410cb53312106f739978`;
  move what is actually there and preserve #967's behaviour.** Done. #967's
  pool hold is inside the relocated `dispatch_held` as its fourth reason,
  `pool_hold`'s probe-claim comment and `announce_pool` moved with it
  unmodified, and the build lane keeps reading `pool_hold` directly through the
  new path. Nothing pre-#967 was restored.
- **Name any stale premise in `SUMMARY.md`.** Four, all resolved in the code's
  favour: (a) the spec says `dispatch_held` has three reasons — it has four on
  this base; (b) it says `MAX_DISPATCH_ATTEMPTS` and `github_hold` become
  `pub(crate)` — true, and `pool_hold`/`announce_pool` had to move as well,
  which the spec does not mention; (c) it says the fourth-leg reasons are
  `Stop`, GitHub and image staleness — the pool leg is added; (d) its
  "834 tests run" measurement is against a different tree, and this branch runs
  894. The `Task`-becomes-test-only pitfall and the `large_enum_variant`
  suppression both held exactly as described.
- **Confirm each test fails against its mutant before calling it done, and say
  so.** Done, and said above with the failing assertion for each. I did this by
  editing the finished tree, running `cargo test -p tasks --lib dispatch_gate`,
  and restoring from a saved copy; the restored tree is what was committed and
  what `make test` ran against.
- **Keep the property in front of you: no surviving route from `run.rs` to a
  `(Task, Project)`.** Checked rather than assumed — `run.rs` contains no
  `list_tasks` call, and `scout.dispatch` has exactly one call site, fed by
  `cleared.into_parts()`.
- **Run `make test` and put the real outcome in the trailer.** Done; the
  trailer below is the real result.

Verification: PASSED — `make test` (894 tests run: 894 passed, 0 skipped; plus `cargo test --doc --workspace`, all green). `cargo fmt --all` and `cargo clippy --workspace --all-targets` are clean.
