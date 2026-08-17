# Three fixes: pool capacity, the store's write lock, and the orchestrator's ability to verify

Three independent specs, one branch, in order.

**#921 — `max_vms` gets a knob.** It was the one capacity number in the system
with no way to set it: a hardcoded `6` in `PoolConfig::default()`.
`VM_POOL_MAX_VMS` now sizes it, wired into **both** places a pool is
constructed — the stock `vm-pool` binary and `tasks vm-pool`, the one actually
deployed, which hand-builds its `ServiceConfig` around `ContainerRuntime` +
`TasksProtocol`. That is why `max_vms_from_env()` is public and separate from
`ServiceConfig::from_env()`: a knob only one entry point honours is worse than
no knob, because it is documented and ignored. A value that is not a positive
integer refuses to start rather than falling back — `0` binds the socket,
answers `status` cheerfully and fails every allocate, silently reproducing the
exhaustion the knob exists to configure. Alongside it the server now *reports*
the arithmetic on every vm-pool connect, off the `status` round trip the connect
path already made: `Capacity::assess` weighs `SCOUT_MAX_CONCURRENT +
BUILD_LANE_SLOTS` against the pool it found, and short or exactly-fitting is a
`warn!` naming the variable and the fix. A report, not a gate — nothing here can
resize a pool in another process. One correction to the issue is load-bearing
and is reflected throughout: **`buildkit` does not occupy a pool slot** (the
container runtime starts it as an ordinary host process the pool never
allocated), so the sum is scouts plus the serial build lane and nothing else,
and it bills to the host *memory* ledger instead. A test pins the exclusion. The
answer to the issue's open question is `SCOUT_MAX_CONCURRENT = 3` against the
default pool of 6.

**#926 — the reattach flake was never a test bug.** Every write transaction in
`store.rs` opened with a bare `Pool::begin()` — a *deferred* `BEGIN` — and every
one of them reads before it writes. A deferred transaction takes its read
snapshot at the first `SELECT` and only asks for the write lock at the first
`INSERT`, and SQLite refuses a *contended upgrade* **without consulting the busy
handler**, since a reader made to wait would deadlock the writer it waits on. So
the 5s `busy_timeout` `Store::open` sets for exactly this overlap never applied
to the case that mattered, and the loser failed instantly, rolling its whole
batch back. One `begin_write` helper (`BEGIN IMMEDIATE`) now backs all 24
transaction sites. The new regression test measures it: two `Store`s on one file
appending 100 lines each lose **82 of 200** appends with a deferred `BEGIN` and
none with this — confirmed in both directions by reverting the helper and
re-running, which reproduced the spec's measured number exactly. Three things
ride along, because the issue's real subject is a suite an agent can trust:
`retry_on_contention` for the two detached writers with no caller to return an
error to (a belt, not the fix); `TranscriptSink::rejected`, which makes a lost
batch loud at `error!` and is deliberately distinct from `dropped_total` — a
drop is backpressure announced in the transcript itself, a rejection is content
accepted and then lost leaving no hole, since `seq` is assigned at persist time;
and `common::capture_warnings`, so a failing integration assertion prints the
store's own explanation underneath it. Two further races in the same test, both
load-dependent and neither caused by the lock bug, are fixed too:
`reconcile_startup_except` returns its `ReconcileReport` so the test asserts on
the decision rather than on a row a spawned reattach may already have concluded,
and the "carried through to a spec" wait moves to the task state, which
`finalize_succeeded` writes last.

**#910 — the orchestrator can now produce the run it used to only ask for.**
`land_builds` ships `live`, but the agent could not run this suite, so a merge
decision rested on a typecheck and the Builder's own claim. The suite was never
the problem — warm, the workspace is ~565 tests in ~21s — it was *compilation*,
because a `git worktree` gets its own empty `target/`. The fix is three
variables on the child process **only**: `CARGO_TARGET_DIR` at a shared,
long-lived directory (`ORCHESTRATOR_TARGET_DIR`), and both bash timeouts derived
as half the turn, half being the statable guarantee that whatever a command
spent, at least that much turn remains to report it. Derived rather than
configured, since the invariant is a relationship between two numbers.
`<data dir>/.env` would have been the wrong home for any of it: every `tasks`
invocation reads that file, so a `CARGO_TARGET_DIR` there is inherited by `tasks
reload`'s own build of the server and silently redirects the Makefile's
`TEST_BIN_DIR`. The prompt half is the load-bearing half — the directory alone
would leave the fix inert, giving the agent somewhere warm to build beside a
standing instruction saying nothing re-runs its tests. `verification_section`
and `landing_section`'s new `Live` arm are generated from one computed
`can_verify`, so they cannot disagree about what the host can do, and the
directory is created once per boot so the prompt can never name one the agent
finds missing. This widens `land_builds` autonomy deliberately: the charter's
own principle is that what sends a batch back is unverifiability rather than
risk, and the orchestrator's own run is a check where the trailer is a claim.
Carve-out (c), the app-gpui rendering case, is untouched. `make verify-warm`
primes the directory.

No migration, no wire-protocol change, and no image rebuild — everything is
host-side. Both new environment variables default to the current behaviour, so
an operator who sets nothing sees only the new log lines and a longer
orchestrator turn budget (600s → 900s). Tests added: 4 in `vm-pool-service`, 4
in `tasks::run` for `Capacity`, 2 regression tests in `tasks::store` (the
two-writer guard and the contention classifier, both pinned against errors
SQLite really produced), 3 unit tests and 1 integration test for the
orchestrator — the last running a real child process that dumps its environment,
which is what pins the *scope*: with no target directory the child sees exactly
what the parent had, neither cleared nor invented.

Verification: PASSED — `make test` (695 tests run, 695 passed, 0 failed; the 7 LEAK results are the pre-existing scout/cancel timeout tests `.config/nextest.toml` scores as pass), plus `cargo test --doc --workspace`, `cargo clippy --workspace --all-targets` and `cargo fmt --all --check`, all clean. The reattach test from #926 additionally ran 25/25 green under a 4-way CPU-burner load on 4 cores.
