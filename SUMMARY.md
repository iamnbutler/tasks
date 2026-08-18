## `tasks drain` / `tasks resume` — the drain the pool restart and the image rebuild never had

The hold-new-work half of #961 shipped as `updates::UpdateWatch` and the
kill-active-work half as `POST /runs/cancel-all`. This is the middle: a
**drain**. `tasks reload` can afford its gate to be a courtesy — `resume_in_flight`
re-attaches to every live VM — while the two *host* acts have no such recovery
and had no gate at all: restarting **vm-pool** on the same socket (the successor
stops its predecessor's containers off the orphan ledger, which is #961's three
orphans) and **`make images`**. `tasks drain` pauses dispatch, waits for
in-flight scouts and builds to land, and **keeps holding** until `tasks resume`,
because what happens next is work this process can neither do nor observe. It
never signals the server, which is what makes it usable *before* a pool restart.
No new server-side hold: mode `pause` **is** the hold, already read by all three
places that select work, and `reload.rs` splits `drain` into `pause_dispatch` +
`wait_for_drain_point` so one pause rule and one wait loop stay in the binary,
with `--cancel-scouts`' cancels in the gap between them (cancel first and the
dispatcher starts a replacement within the tick). It pauses an idle pipeline too
— the deliberate inversion of `stop --when-idle` — because an idle pipeline
nobody holds starts a scout on the next tick, into the pool that is about to go
down. `ModeAfterDrain::HeldForMaintenance` is the third variant its own doc
comment asked a third caller to supply.

`make drain` / `make resume` / `make check-quiesced` are the targets, and
`make images` now runs the gate *first*, as sub-makes rather than prerequisites
(`make -j` gives prerequisites no ordering, and the gate must precede the build);
`FORCE=1` skips it. `--check` refuses a **playing** pipeline as well as a busy
one, passes untouched with nothing serving, and touches neither the mode nor any
run. Both edges go on the event feed: what a drain leaves behind is a `pause`
byte-identical to a human's, so `POST /mode` grew an optional `note` and the
drain, its timeout unwind and `tasks resume` each append an `EventPayload::Note`
— the edge on the feed, the standing answer in the mode, and deliberately
nothing between them. `UpdateWatch::announce` gains the same treatment (a `Note`
per edge, source `update-watch`, claimed under the guard and written after it is
released), which was #961 §5's outstanding item. A cancel is a durable row the
dispatcher following the run concludes, so `--cancel-scouts` cannot guarantee
the drain point arrives; the code says so in a comment and the output repeats
the server's own `CancelAck.concluded` rather than flattening "asked" and
"stopped" into one word. 12 integration tests in `crates/tasks/tests/reload.rs`,
4 unit tests in `reload.rs`, 2 in `updates.rs`.

## Review feedback

- **1. `--check` must fail on a *playing* pipeline, not only a busy one.** Done,
  and it is the load-bearing change: `Drained::Clear` now requires
  `!is_destructible()` **and** a mode that is not `Play`, read off the `/status`
  body `--check` already fetches (no extra call). A playing server is `Busy`
  (exit 3) naming `tasks drain`; `NotServing` still passes untouched. It has its
  own integration test —
  `a_check_refuses_a_playing_pipeline_with_nothing_in_flight` — plus the pure
  predicate test `a_check_refuses_a_playing_pipeline_as_well_as_a_busy_one`.
- **2. The drain's pause must be distinguishable from any other pause.** Done, on
  the feed: `SetMode` gained an optional `note` and `set_mode` appends an
  `EventPayload::Note` (source `mode`) whenever one is sent, independently of
  whether the mode moved — a drain of an already-held pipeline changes nothing
  and is still the fact somebody arriving later needs. The drain edge, the
  `tasks resume` edge **and** the timeout unwind each carry one (the unwind is an
  edge too: it says nothing is held). The standing-answer half is deliberately
  not built, and the gap is named in the CLAUDE.md bullet: the edge is on the
  feed, the standing answer is the mode, and there is nothing between them.
  *Deviation from the spec, which said "no wire type":* `reload.rs` must never
  open the store (`Store::open` runs migrations), so the CLI's only route to the
  feed is the server, and the mode-setting act is the edge. The field is
  `#[serde(default)]`, so every existing caller sends the request it always did.
- **3. Do not write "`make images` destroys in-flight VMs" into CLAUDE.md.** Done
  — the bullet says in as many words that what a `container build` does to an
  already-running container is *not established here and is deliberately not
  written down as though it were*, and gives the checkable reason instead: a
  scout dispatched while the rebuild is in flight starts in the **old** image,
  the #909 staleness `UpdateWatch` exists to prevent and the one case it cannot
  see, since the identity it reads is only ever observed from a run that has
  already started. Only the vm-pool restart is stated as established (off the
  orphan ledger). I did not attempt to establish the rebuild claim from the
  runtime — this host has no `container` CLI — so nothing is asserted about it.
- **Kept as specified:** `--cancel-scouts` is strictly opt-in and never the
  default; it routes through `POST /sessions/{id}/cancel` rather than removing
  the VM (#876); and the "a cancel does not guarantee the drain point" paragraph
  now lives in the code, on `cancel_running_scouts`, with the integration test
  that ends in exit 4 for exactly that reason.

## Directions

- **Run `make test` in the foreground and paste the counts.** Done — see the
  trailer. cargo-nextest 0.9.143 was present; nothing was substituted or
  backgrounded.
- **LEAK noise.** 7 leaky tests, all the documented scout/cancel timeout ones;
  not chased. The recorded count disagreeing with CLAUDE.md is #969 and was left
  alone.
- **#958 (`ScoutFailed` where `ScoutStoppedEarly` was expected).** Not observed
  in this run — 816 passed, 0 failed — so there was nothing to report or avoid
  fixing.
- **The `--check`-on-a-playing-pipeline change needs its own integration test.**
  Done, named above, and it asserts the mode is still `play` afterwards: a check
  refuses, it does not hold.

Two further judgement calls worth naming, neither in the spec. `pause_dispatch`
leaves a mode that is not `Play` **exactly as it is** rather than writing
`pause`: `Stop` is tighter than `Pause` (it stops the poller too), so "pausing" a
stopped pipeline would quietly turn intake back on in the name of holding it —
the note still records the hold, travelling with the mode already in force, and
`tasks resume` reports the mode it found so a `stop → play` promotion is never
silent. And `drain --check` against a live pid that will not answer `/status`
refuses (exit 3) like the drain proper, since "quiesced" about a server we cannot
see into is the wrong direction to be wrong in.

Verified live as well as in tests: the gate passes with nothing serving, exits 3
against a playing-but-idle server with the advice, is skipped by `FORCE=1`, and a
drain/resume cycle against a real server left both notes on the feed.

Verification: PASSED — `make test` (cargo-nextest: 816 tests run, 816 passed, 4
slow, 7 leaky, 0 skipped; then `cargo test --doc --workspace`: 3 doctests passed,
0 failed). Also `cargo clippy --workspace --all-targets` and `cargo fmt --all`
clean, and `make app-check` clean.
