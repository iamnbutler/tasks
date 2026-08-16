# Stop When Idle, in the CLI and the Server menu

`tasks stop` grows `--when-idle` (with `--drain-timeout SECS`), reusing
`reload`'s drain loop and the *same* `InFlight::is_destructible()` predicate, so
Restart When Idle and Stop When Idle cannot disagree about what idle means. Exit
codes carry over too — 3 and 4 mean what they mean in `reload` (5 has no meaning
for a stop and is never returned), and `make stop STOP=--when-idle` mirrors
`make restart RELOAD=--when-idle` from its own variable, since `stop` rejects
`--force`/`--no-build` and a shared one would turn a typo into a usage error.
Plain `tasks stop` is unchanged: immediate and ungated, because it is the
counterpart of `reload --force`, the thing `make stop` and the reload path
already rely on, and the documented way through both new refusals.

The one decision that had to be made explicitly is what happens to the mode, and
it is a correctness question rather than a matter of taste: **a stop leaves
dispatch paused**. The only slot in which it could write the mode back is
*before* the SIGTERM — after it there is no server to `POST /mode`, and nothing
in this module may open the store to do it directly — and unpausing a server that
is still running hands the dispatcher a window to launch one last scout, which is
precisely the unattended VM the feature exists to prevent. So it stays paused,
and says so in the help, in the drain output, on the way out (with the `curl`
that undoes it) and in the app. A drain *timeout* still restores the mode,
because nothing was stopped and a no-op must not have side effects, and an idle
server is never paused at all — a wait that did not happen must leave no trace.
`ModeAfterDrain` is the single place that asymmetry is written down, so a third
caller of `drain` has to answer the question rather than inherit an answer.

The app gets a matching **Stop When Idle…** menu item and window button beside
their restart counterparts, and — because the Server window has been polling
`/status` all along — an immediate **Stop** with work in flight now raises a
three-way confirmation (Wait, then stop / Stop anyway / Cancel) with the work
named and aged and both halves of the trade stated, rather than ending the
process under it. `Stop Server…` gained its ellipsis accordingly. That prompt is
raised from a poll up to 5s stale, so it is a courtesy and never a lock; if the
work lands while it is up the question collapses and the click is dropped, since
stopping on a question nobody is being asked any more would be worse. Three
verdicts fork on `Op::stops()`: a waited-out stop reports the paused pipeline, a
stop that gave up says "nothing was stopped", and exit 3 for a stop means the
server would not say what is in flight — a different refusal, so it offers Try
again / Stop anyway rather than the restart's wording.

Covered by five new end-to-end tests against real servers (drain-then-stop with
the mode read back out of the store afterwards, an idle stop that leaves the mode
alone, a timeout that stops nothing and puts the mode back, the exit-3 refusal
against a pidfile that answers nothing, and the flags `stop` still rejects) plus
unit tests for the new wording and the confirmation rule. `make test` 557/557 (plus doctests) and
`app-gpui`'s 103 pass; clippy and `cargo fmt` clean on both trees. Note that the
spec's third strand — the `orchestrator_nudge_loop` shutdown bug that made every
graceful stop wait out `STOP_GRACE` — had already landed on `main` (the guard and
its regression test `a_shutdown_mid_burst_stops_the_nudge_loop` are in the tree),
so nothing was needed here; the new `--when-idle` stop tests finish in ~3s, which
is the evidence it is working.
