# Measure run budgets on two clocks, so a suspended host reads as a suspend and not as a timeout (#929)

Every run budget in the server was one `tokio::time::sleep(budget)`, which is
`Instant`-based, and an `Instant` does not advance while the machine is asleep.
On the laptop this pipeline runs on that produced #929: a build dispatched at
03:44, a host on battery from 04:22, a lid opened at 12:34, and a deadline
firing three and a half minutes later reporting `build timed out after 3600s` —
true in monotonic terms, wrong in every term a human uses. It held the serial
build lane for nearly nine hours and charged #909, #917 and #918 a build attempt
each for a closed lid, which CLAUDE.md's own rule ("a strike is charged for a
verdict, and for nothing else") forbids: the document defends charging a
`Timeout` precisely because it "had the entire budget", and this one had 38
minutes of it. This change introduces `crate::deadline` — a `Deadline` anchored
on *both* a monotonic and a wall-clock reading, expiring on whichever runs out
first — and threads it through `cancel::bounded` into both dispatchers and the
orchestrator tick. Because both anchors are kept, the gap between them at expiry
*is* the time the host was not running, so a suspend is a measured fact rather
than an inference: `ScoutError::Suspended` / `BuilderError::Suspended` /
`OrchestratorError::Suspended` each carry the `Expiry`, render their own
`exit_reason` clause ("the host was suspended for 8h13m of a 1h budget; only 38m
of it ran"), and classify as `FailureClass::Transport`, so `Strike::for_class`
waives and the existing waived-strike `Note` path names it.

Four properties are load-bearing and are pinned by tests. The monotonic reading
is the **floor** (`wall.elapsed().unwrap_or(awake).max(awake)`), so a wall clock
stepped by NTP — forwards or backwards — can only ever degrade to the behaviour
that shipped before, while a suspend is still caught. The deadline **polls** on
a 30s tick rather than sleeping the remainder, which is what makes it fire on
the *wake* instead of once the leftover monotonic budget finally drains; the
cost is nil, since `cancel::bounded` already polls the cancellations table every
5s under the same `select!`. `Timeout` is otherwise untouched — same variant,
same `secs: config.timeout.as_secs()` (never the expiry's, because a resumed
run's effective budget is the remainder and three integration assertions pin
specific numbers), same string — and the suspend clause deliberately never
contains "timed out", which a unit test asserts. And a suspended run is **killed
at the wake, not extended**: no agent's API connection survives an eight-hour
suspend, so handing the budget back would only hold the serial lane longer for a
run that is already dead; `caffeinate -s` stays the operational answer. Every
new test carries its negative half — a budget genuinely spent awake still
expires, still reads as a timeout and still charges — because a test in which
nothing is charged is indistinguishable from the cap having been switched off.
The change is entirely host-side: no image rebuild (nothing in
`crates/{scout,builder}-supervisor` or `images/` is touched and no new
`FailureClass` crosses the wire), no migration, and the only signature changes
(`cancel::bounded` taking a `&Deadline`, `Bounded::TimedOut` becoming a tuple
variant) are crate-internal. CLAUDE.md gains a `### Budgets and a host that
sleeps` section, a `BUILDER_TIMEOUT_SECS` row that was missing entirely, and the
strike bullet now reads "a `Timeout` that had the entire budget **awake**".
Nothing here is retroactive — clearing the strikes #929 already charged #909,
#917 and #918 stays a separate, human decision.

Verification: PASSED — `make test` (707 passed, 0 failed; the 7 LEAKs are the documented scout-timeout ones, `leak-timeout` is `result = "pass"` in `.config/nextest.toml`), plus `cargo clippy --workspace --all-targets` clean and `cargo fmt --all --check` clean
