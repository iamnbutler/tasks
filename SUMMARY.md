# Re-read the dispatch gates before each scout, not once per pass

`run::top_up` read the mode, the GitHub hold and the update hold once at the top
of a pass and then dispatched inside a loop bounded by `SCOUT_MAX_CONCURRENT`,
so a `POST /mode {"mode":"pause"}` landing after that read still permitted a
whole complement of scouts — each one a VM — in the same pass (#948). The three
checks now live in one private `dispatch_held(&Store, &GitHubHealth,
&UpdateWatch) -> Result<bool, StoreError>`, holding those checks and their
comments verbatim, and `top_up` calls it twice: once before the loop and once
inside it. Placement is the design decision. The in-loop read sits *after*
`next_dispatchable`, not before it — a human pauses and *then* queues work, so
any task the scan could see was committed after the pause was, and the read that
follows the scan therefore cannot miss that pause; it is also the last thing
before `in_flight.spawn`, with nothing awaited in between, so the outcome is
decided rather than raced. The pre-loop call is kept purely for cost: a paused
server ticking at `DISPATCH_TICK` must not pay for a `list_tasks` table scan
plus a `get_project` per row walked past, so steady-state paused cost is
unchanged. Nothing else moves — no new gate, no new config, no new state, no
signature change visible outside the module, and the error path is untouched, so
a failed store read still returns `Err` and never dispatches.

The build lane is deliberately left alone: it asks the same three questions in
its match guard ahead of `claim_next_queued_build` and claims at most one build
per pass, so it already re-reads them for every container it starts, and sharing
`dispatch_held` would mean restructuring a match guard around an `await`. That
argument is now written at **both** ends — in `dispatch_held`'s doc comment and
in a reciprocal comment at the guard — so whichever site someone touches when
adding a fourth reason to hold new work points at the other. On the test side,
`pause_blocks_new_dispatches` loses its 600 ms sleep so the pause and the queue
write are adjacent, and its comment now says what that ordering proves; the
sleep was papering over exactly the window this fix closes. Deterministic
coverage of the freshness itself is a new unit test,
`dispatch_held_answers_from_live_state_every_time`, which drives Play →
Pause/Stop/Play and a GitHub outage set and cleared between successive calls and
asserts each is seen by the very next read.

## Review feedback

- **Add a reciprocal comment at the build lane's dispatch guard naming
  `dispatch_held`, stating the two are deliberately separate and why.** Done —
  the comment at the `Ok(Mode::Play) if in_flight.builds() == 0 &&
  !github_hold(&health) && !update_hold` guard now names `dispatch_held`, says
  the lane claims at most one build per pass so it never had this bug, says
  unifying would mean restructuring a match guard around an `await`, and says a
  fourth reason to hold new work belongs in both places. `dispatch_held`'s doc
  comment carries the other half of the pair.
- **Do not attempt the unification here (it is filed separately).** Not
  attempted; the duplication stands, documented from both ends.
- **Keep the observation about `--when-idle` for whoever edits that paragraph
  with #961, rather than editing `CLAUDE.md` now.** Followed — `CLAUDE.md` is
  untouched. The observation, recorded here so it travels: unpausing still
  "hands the dispatcher a window to launch one last scout", but the window
  narrows from a whole complement to a single dispatch already past its gate.
- Placement of the in-loop read, keeping the pre-loop call for cost, keeping
  `dispatch_held` silent, and leaving the error path alone were all affirmed by
  the review and are unchanged from the spec.

## Directions

- **Add the build lane comment — it is required, not optional.** Done, as
  above; it is the only thing in this change beyond the spec's four items.
- **Do not widen this: one extracted function, one extra call site, one restored
  test, one unit test. No new gate, config, drain or `CLAUDE.md` edit.**
  Followed exactly. `dispatch_held` stays small so #961's predicate is one more
  early `return Ok(true)` inside it.
- **`crates/tasks/tests/scout.rs:667` is the known #958 flake; the 7 LEAK
  results are expected.** Noted. On this run
  `a_timed_out_scout_keeps_the_checkpoint_it_had_already_streamed` passed (as
  LEAK, which is documented), so nothing needed to be attributed to #958 and no
  assertion was weakened. The 7 leaky results are the documented scout/builder
  timeout tests.
- **Do not end the turn waiting on a background command.** Nothing is running;
  the suite was run in the foreground to completion.

No direction or feedback item conflicted with the spec.

Verification: PASSED — `make test` (777 tests run, 777 passed, 0 failed, 7 leaky — the documented scout/builder timeout tests — plus doctests), with `cargo fmt --all` and `cargo clippy --workspace --all-targets` clean.
