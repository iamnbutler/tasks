# Make the salvaged-timeout scout test observe its checkpoint instead of racing it (#958)

`tests/scout.rs::a_timed_out_scout_keeps_the_checkpoint_it_had_already_streamed`
gave a scout a 3-second budget and then asserted the session came back
`ScoutStoppedEarly`, which is only true if a `NOTES.md` checkpoint reached the
host inside those three seconds. The supervisor's checkpoint watcher sleeps
first — at t=0 the agent has written nothing (`scout-supervisor/src/main.rs:445`)
— so the first checkpoint cannot arrive before a whole
`SCOUT_CHECKPOINT_INTERVAL_SECS`, which the test harness sets to 1s, and VM
allocation, clone, branch and agent startup come out of the same three seconds.
With siblings sharing the machine the budget went first, the run really did end
holding nothing, and `ScoutFailed` was the honest answer. The assertion was the
wrong thing, not the code.

The fix is entirely in that one file, with no production change, no
`.config/nextest.toml` entry and no migration. `dispatch` is now spawned;
`await_streamed_checkpoint` polls `list_sessions` + `get_scout_notes` until the
run's notes row exists — that row is written by the checkpoint sink
`Scout::follow` spawns, and the drain hands the sink its event in the same match
arm that sets `state.checkpoint`, so the row proves the dispatcher is holding a
checkpoint — and returns the session id, which the test then asserts is the
session it reads back. Only once the checkpoint is in the store does
`expire_the_budget` retire the deadline itself: `tokio::time::pause()` →
`advance(BUDGET + 1s)` → `resume()`, spanning a single `advance` and nothing
real, because a paused clock auto-advances whenever the runtime parks. That
seam is not a test-only knob smuggled into production code — `Deadline` anchors
on a `tokio::time::Instant` precisely so its poll loop still works under a
paused clock, as its own doc comment at `deadline.rs:129-134` says. The
assertions are unchanged (`Timeout`, `ScoutStoppedEarly`, an `exit_reason` still
containing "timed out", the notes, no spec, task back to `Queued`, the VM
deallocated), except that `matches!` now compares `secs` against
`BUDGET.as_secs()` so the two cannot drift. `CHECKPOINT_WAIT` (10s) stays
strictly under `BUDGET` (20s): that ordering is the whole guarantee, since the
run's own deadline can never fire while the harness is still watching, so a
machine too slow to stream a checkpoint fails on a named precondition rather
than on a verdict about salvage. `BUDGET` is capped at 20s because the clock
jump is `BUDGET + 1s` and must stay under sqlx's 30s acquire timeout, the test
pool's 60s health check and its 300s `vm_timeout`.

**What the test proves now, and what it no longer does.** It proves that a
deadline firing against a run *already holding* a checkpoint yields
`ScoutStoppedEarly` with the salvage kept. It no longer exercises a deadline
arriving naturally — that is what `a_scout_that_never_reports_back_times_out`
and `a_hung_scout_times_out_and_frees_its_slot` are for. The paused clock has
not weakened this test; it has moved it off the one thing it could not observe.
The 20s budget is also not the widened timeout the spec was told to prefer
something else over, and that is checked rather than argued: with the in-test
checkpoint interval temporarily raised to 5s — past the old 3s budget, standing
in for an arbitrarily slow host — the old test fails with the original panic
(`left: ScoutFailed / right: ScoutStoppedEarly`) and the new one passes in
5.13s, having waited for the checkpoint and then fired the deadline.
`crates/tasks/tests/common/mod.rs` was restored and is untouched in the diff.
The test still reports `LEAK` under nextest (the stub agent's `sleep 10`
outlives the run); that is the pinned, documented behaviour and is unrelated to
#958, which was an assertion failure.

## Review feedback

The review approved the spec with no required changes, and carried one item to
record here rather than to change:

- *"This test now proves that a deadline firing against a run already holding a
  checkpoint yields `ScoutStoppedEarly` with the salvage kept. It no longer
  exercises a deadline arriving naturally... Say so, so that nobody later reads
  the paused clock as having weakened the test."* — Done: it is the third
  paragraph above, and the same paragraph is now a doc comment on the test
  itself, naming the two tests that do cover a natural deadline.

## Directions

- *"This spec needs no changes — implement it as written."* — Done. The diff is
  the spec's shape exactly: one file, `await_streamed_checkpoint`,
  `expire_the_budget`, a spawned `dispatch`, and the same assertions.
- *"Keep the proof, not just the fix... if anything forces you away from the
  spec's shape, particularly `CHECKPOINT_WAIT` staying strictly under `BUDGET`,
  re-run the experiment."* — Nothing forced a departure (10s under 20s is
  preserved), but the experiment was re-run anyway, on this tree and this host;
  the result is in the third paragraph. The experiment was not shipped: the
  harness edit it needs was reverted.
- *"Say what the test now proves and what it no longer does."* — Done, in the
  third paragraph and in the test's doc comment.
- *"#968 and #969 are not yours. Do not touch `crates/scout-supervisor/`,
  `.config/nextest.toml` or `CLAUDE.md`."* — None of the three is touched; the
  diff is `crates/tasks/tests/scout.rs` alone. Nothing tempted me toward them.
- *"Run the suite in the foreground, and run `cargo test -p tasks --test scout`
  on its own as well, since one number cannot show this bug."* — Both run in the
  foreground; both numbers are below.
- *"#958 is not the LEAK."* — Noted, and stated as such above.

Verification: PASSED — `make test` (796 passed, 0 failed, 7 leaky, plus doctests) and `cargo test -p tasks --test scout` alone (8 passed, 2.67s; three further consecutive runs under 4 CPU spinners, 8 passed each); `cargo clippy --workspace --all-targets` clean; `cargo fmt --all --check` clean
