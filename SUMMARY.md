# deadline.rs: say which clock the floor bounds, and correct the bound itself

`WAKE_KILL_FLOOR`'s doc comment — and the sentence `CLAUDE.md` copies from it —
claimed that "because the wall arm is disarmed below it, a run can outlive its
**wall-clock** budget by less than this floor, never more". Read as written,
about wall-clock elapsed, that is false and unboundedly so: `Expiry::remaining`
answers `None` as soon as `awake` reaches the budget, whatever the suspend is, so
a lid closed for three hours during a disarmed run's last tick fires three hours
past the wall budget and nothing caps it, because nothing caps a suspend. The
quantity that *is* bounded is the run's **awake execution past the point wall
elapsed reached the budget**, and that bound is strictly **under**
`WAKE_KILL_FLOOR` — not `WAKE_KILL_FLOOR + WALL_CLOCK_TICK` as #955 proposes,
which double-counts: neither branch of `remaining` ever answers with more than
the monotonic remainder (the armed branch returns `budget − elapsed`, and
`elapsed >= awake`), and `expired` sleeps `remaining.min(WALL_CLOCK_TICK)`, so
`awake` never passes `budget` and at most the suspend accumulated at that point
is left to spend — under the floor if the arm is disarmed there, and at most one
tick if it is armed, a tick being *less* than the floor rather than an addend to
it. **The issue title is therefore half wrong**, which is worth saying out loud
because the title is what a future reader greps for: the defect it names is real,
but the replacement bound it offers is not.

This is prose plus tests; no behaviour changes. The correction is written into
the two places the claim lives (`crates/tasks/src/deadline.rs` and `CLAUDE.md`'s
*Budgets and a host that sleeps*), each stating which regime the old sentence was
right about and where it generalised past it, and each keeping the two questions
apart — `WALL_CLOCK_TICK` bounds how long after a wake a doomed run stays parked
holding the serial lane, and is not a term in the overshoot bound at all.
`Expiry::remaining`'s doc comment gains the invariant the bound rests on, plus
the note that its check ordering is *not* what makes the wall-clock overshoot
unbounded, so nobody reorders the function looking for a bound that cannot exist.
Two tests pin both halves: `awake_execution_past_the_wall_budget_stays_under_the_floor`
(a second under the floor of nap with a tick of budget left and wall elapsed past
the budget — still disarmed, still handed its last tick, bounded by the suspend;
one more second arms the arm and it is over; and armed early, `wall_left <=
awake_left`) and `the_wall_clock_overshoot_at_the_firing_poll_is_unbounded` (a 1h
budget spent entirely awake with `elapsed` at 4h — `suspended()` is three hours
and it still reads as `"the 1h budget ran out awake"`, which is the point).

## Review feedback

- **Required: explain why the old sentence was plausible, not just that it is
  wrong — the reason it gives was right about the regime it names and the
  sentence generalised past it; say it in both copies.** Done, and it is the
  spine of both rewrites rather than a clause appended to them. Each copy quotes
  the old reason, grants that while the arm is disarmed the whole suspend is
  under the floor so the wall overshoot is under it too, and then names the case
  the clause silently excludes: a single nap at or past the floor *arms* the arm,
  and that nap is itself the overshoot. As the reviewer notes, this is the same
  disarmed-vs-armed split the derivation turns on, so the two halves of the prose
  now share one structure.
- **`unspent()` reading zero in the three-hour case stays untouched; pin the
  sentence in a test rather than change it.** Done — `wake_killed()` guards on
  `!unspent().is_zero()` first, so a fully-spent budget falls through both
  suspend sentences. The test asserts `unspent().is_zero()`,
  `!starved_by_suspend()`, `!wake_killed()` and the exact string, and its doc
  comment says the `suspended() == 3h` pairing looks like a bug and is not, so
  the intent is explicit rather than accidental.
- **The issue title is yours to correct; you have commented it on #955, and
  nothing for me to do.** Honoured — #955 is untouched here and the comment is
  not restated. The observation that the title is half wrong is stated in this
  summary on its own terms, as the spec had it, and not as a report of that
  comment.
- The re-derivation, the check of the `wake_killed` fall-through, and the notes
  on keeping the two questions apart carried no action; nothing was changed on
  their account.

## Directions

- **Prose plus two tests, no behaviour change; `remaining`, `expired`,
  `starved_by_suspend` and `wake_killed` stay exactly as they are.** Honoured —
  the only non-comment change in the diff is the two added `#[test]`s. The four
  functions are byte-identical apart from doc-comment lines above `remaining`.
- **Write the correction so it says which regime the old sentence was right
  about.** Same as the required review item above; done in both copies.
- **The two copies are `crates/tasks/src/deadline.rs:103` and `CLAUDE.md:688`;
  if you find a third, say so.** There is no third — `grep -n "never
  more\|outlive"` across the tree finds the claim in exactly those two files, and
  nothing renders either string to a user. One discrepancy worth naming rather
  than silently absorbing: on this branch the `CLAUDE.md` copy is at **line 729**,
  not 688 (`deadline.rs:103` is right). It is the same sentence in the same
  *Budgets and a host that sleeps* paragraph, so this reads as line drift rather
  than a different tree, but it is stated here because the direction asked for
  the discrepancy rather than the guess.
- **Suite in the foreground; `make test` is the suite; #958's LEAK is not to be
  chased or quieted.** Done — `make test` run in the foreground to completion,
  `cargo-nextest` present. 797 tests, 797 passed, 0 failed, plus the 3 doctests
  nextest does not run. Seven LEAK results, all the documented ones (the scout and
  build timeout/cancel tests); no assertion was weakened and nothing was chased.

Verification: PASSED — `make test` (cargo-nextest: 797 passed, 0 failed, 7 documented leaks; plus `cargo test --doc --workspace`: 3 passed), with `cargo fmt --all --check` and `cargo clippy --workspace --all-targets` clean
