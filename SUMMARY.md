# Split `SUSPEND_FLOOR` into a wake-kill threshold and a waiver threshold (#944)

`crates/tasks/src/deadline.rs` had one 60-second `SUSPEND_FLOOR` answering two
different questions. Because the deadline fires on `max(wall, awake)`, a run
whose host napped *at all* can never reach its budget awake — so any nap over a
minute, anywhere inside an hour, waived the strike for a run that spent 59 of
its 60 minutes working. The two questions are now two constants with two
arguments. `WAKE_KILL_FLOOR` (5 minutes) is **availability**: it gates the
wall-clock arm of the expiry itself, so below it the wall arm is disarmed and
the run drains its monotonic budget exactly as it did before the module existed
— which is right, because the in-VM supervisor already re-invokes an agent whose
API connection dropped (`{SCOUT,BUILDER}_MAX_RESUMES`), which is precisely what a
short nap causes, and killing the run throws that recovery away. The gap is
cumulative since the deadline started, and disarming below the floor means a run
can outlive its wall-clock budget by less than that floor, never more.
`WAIVED_BUDGET_SHARE` (a quarter) is **accountability**: read as how much of the
budget went unspent awake (`budget − awake`), it — and not "did the host sleep" —
is what picks `Suspended` over `Timeout` at all three consumers. A fraction
rather than a flat ten minutes because every budget it reads against is
configurable and the reattach paths derive much shorter ones, where a flat floor
would make the waiver quietly unreachable. No second "was that really a suspend"
test is needed: an expiry only happens with `elapsed >= budget`, so
`unspent <= suspended` always, and two clocks read microseconds apart leave
nothing unspent.

Mechanically: `Expiry` gains `unspent()`, `host_slept()` becomes
`starved_by_suspend()` (the rename is the point — a predicate that returns
`false` for a host that demonstrably slept must not be called `host_slept`), and
the firing decision moves out of `expired()` into a private
`Expiry::remaining()`, so the gate and the sleep interval are one expression and
cannot drift. The split creates a third state — wake-killed but not starved, a
long nap arriving late — so `Display` grows a third sentence for it (the two
existing sentences are byte-identical; none contains "timed out"), the scout's
expiry note appends the expiry clause only when something went unspent, and the
builder's unguarded timeout arm now binds and logs the measurement. The
discriminating test the #943 suite lacked is included: nap for 61 seconds, then
burn the full remaining budget awake → still charged (it panics with
`starved_by_suspend`'s body swapped back to the old floor). Five more cover the
middle state's sentence and verdict, the same suspend arriving early being
waived, `remaining()` at four versus five minutes of nap read at the same
wall-clock instant, and the quarter boundary from both sides. `deadline.rs`'s
and CLAUDE.md's claim that a clock adjustment "can only ever degrade to the
behaviour that shipped before" is corrected: a step backwards is neutralised by
`max()`, a step forwards is not, the design cannot distinguish it from a lid,
and what bounds the bill is `WAKE_KILL_FLOOR`.

Verification: PASSED — `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` (clean), `make test` → 738 tests run, 738 passed, 7 leaky (the documented scout-timeout/cancel leak set, unchanged)
