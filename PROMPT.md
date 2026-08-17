You are a Builder in the Double Diamond architecture.

You are implementing 3 approved spec(s). Each was written by a Scout that already explored the work by implementing it once in a throwaway branch you cannot see — the spec is the distilled result. Trust its pitfalls; verify its claims against the code in front of you.

## Spec 1 of 3: Raising SCOUT_MAX_CONCURRENT walks into an undocumented max_vms = 6, and running out reads as the work failing (#921)

## Spec: Give `max_vms` a knob (`VM_POOL_MAX_VMS`), and make the pool's capacity arithmetic visible before it is hit

### Summary

`max_vms` was the one capacity number in the system with no way to set it: a `6`
hardcoded in `PoolConfig::default()`, changeable only by editing the constant and
rebuilding. This adds `VM_POOL_MAX_VMS` (default 6, so nothing changes silently), wires it
into **both** places a pool is constructed — the stock `vm-pool` binary and `tasks vm-pool`,
which is the one actually deployed — and refuses to start on a value that is not a positive
integer rather than quietly falling back. Alongside it, the arithmetic that governs
`SCOUT_MAX_CONCURRENT` is written down in `CLAUDE.md`, and the tasks server now *reports* it
on every vm-pool connect: a pool too small for this server's configuration, or one that fits
it with no slack, is a `warn!` naming `VM_POOL_MAX_VMS`, off the `status` round trip the
connect path already makes. One correction to the issue is load-bearing and is reflected
throughout: **`buildkit` does not occupy a pool slot**, so the sum is scouts + the serial
build lane, and nothing else.

### Implementation Approach

**The knob (vm-pool tree).**

- `crates/vm-pool/pool/src/lib.rs` — extract `pub const DEFAULT_MAX_VMS: usize = 6;` and have
  `PoolConfig::default()` use it. Its doc comment is where "what counts as a slot" is
  defined (a VM this pool allocated; buildkit is not one), and `PoolConfig::max_vms` gets a
  doc comment on why to leave slack.
- `crates/vm-pool/service/src/lib.rs` — add `pub const MAX_VMS_ENV = "VM_POOL_MAX_VMS"`, a
  `thiserror` `ConfigError::MaxVms { value }`, `pub fn max_vms_from_env() -> Result<usize,
  ConfigError>`, and `ServiceConfig::from_env()` built on it. Add `thiserror.workspace = true`
  to that crate's `Cargo.toml`.
  - Resolution lives in `max_vms_from_env`, **public and separate from
    `ServiceConfig::from_env`**, because the service has two entry points and a knob only one
    of them honours is worse than no knob — it is documented and ignored.
  - It is **not** in `Default::default()`. `default()` is what tests and embedders build
    configs with; one that reads the ambient environment lets a developer's shell decide what
    a test asserts. Same reasoning tasks' `CLAUDE.md` gives for never loading `.env` inside
    `Config::from_env`.
  - Parsing is a private pure `max_vms_from(Option<String>)` so the tests never touch the
    process environment.
- `crates/vm-pool/service/src/main.rs` — `ServiceConfig::from_env()?` plus an `info!("pool
  capacity", max_vms, var)` before anything binds.
- `crates/tasks/src/main.rs::vm_pool()` — **the one that matters.** It hand-builds its
  `ServiceConfig` (it needs `ContainerRuntime` + `TasksProtocol`, so it cannot use the stock
  binary), so it calls `max_vms_from_env()?` into `PoolConfig { max_vms, ..default() }` and
  logs the same `pool capacity` line. Add `VM_POOL_MAX_VMS` to `USAGE`, and extend the
  `SCOUT_MAX_CONCURRENT` line with the arithmetic.

**The report (tasks tree).**

- `crates/tasks/src/reattach.rs` — split the classification out of `attach_support` into
  `pub fn support_of(&PoolStatus) -> AttachSupport`, so a caller can answer both questions
  from one `status` reply. `attach_support` keeps its signature and its `Err` handling.
- `crates/tasks/src/run.rs` — `report_attach_support` becomes `report_pool(&client,
  &config)`: one `status()` call, then the existing attach-support line, then
  `Capacity::assess(status.total, config.scout_max_concurrent)`. A `status` that errors keeps
  the existing "cannot hand work back" `warn!` (built from `AttachSupport::Unknown(e)`) and
  skips the capacity half rather than guessing — `status` is the oldest command in the
  protocol, so a pool that will not answer it will not answer anything better.
- `Capacity` is a three-variant enum (`Short` / `NoSlack` / `Slack`) over
  `scout_max_concurrent + BUILD_LANE_SLOTS`, with `report_capacity` picking the log level.
  Pure and unit-tested; the I/O stays in `report_pool`.
- **A report, not a gate.** Nothing here can resize a pool in another process, and refusing
  to dispatch would turn a survivable misconfiguration into an outage. `NoSlack` is a `warn!`
  and not an `info!` on purpose: it dispatches fine today and exhausts on the first leak.

**The documentation.**

- `CLAUDE.md` — a `### Pool capacity` subsection under *Running*: the two ledgers, the
  recommended ceiling, and the fact that `VM_POOL_MAX_VMS` is read by `tasks vm-pool` and not
  by the server, so changing it means restarting the *pool*. Plus a `VM_POOL_MAX_VMS` row in
  the config table and a rewritten `SCOUT_MAX_CONCURRENT` row.
- `crates/vm-pool/CLAUDE.md` — a `## Configuration` section: the knob, what a slot is, the
  three implementation rules, and "leave slack". No app vocabulary — the scouts/builder
  arithmetic stays on the tasks side, per the dependency-direction rule.
- `docs/vm-pool.md` — one bullet under *Pool*.

**The answer to the issue's open question** ("what is the largest `SCOUT_MAX_CONCURRENT` this
pool is meant to support?"): **3** against the default pool of 6 — 4 of 6 with two slots of
slack. 4 scouts is 5 of 6 and 5 is 6 of 6, where any leak is immediate exhaustion. To go
higher, raise `VM_POOL_MAX_VMS`, restart `tasks vm-pool`, and check the memory ledger first.

### Discovered Pitfalls

1. **There are two entry points and the stock binary is not the deployed one.**
   `crates/vm-pool/service/src/main.rs` is `NoRuntime + ShellProtocol` and cannot carry
   TasksProtocol. The pool that runs scouts is `tasks vm-pool`
   (`crates/tasks/src/main.rs`), which hand-builds a `ServiceConfig` around
   `ContainerRuntime`. Wiring the knob only into `ServiceConfig::from_env()` would ship a
   documented variable that does nothing on the host it was written for. This is why
   `max_vms_from_env()` is public and separate.

2. **The issue's arithmetic is wrong in one term — `buildkit` is not a pool slot.**
   `max_vms` gates exactly one thing: `vms.len() >= self.config.max_vms`, where `vms` is the
   pool's own map of VMs *it* allocated. `buildkit` is started by apple/container to service
   `container build` (`ImageStore::build` spawns the CLI as an ordinary host process); it is
   never allocated, never in that map, and there is no `container ls` reconciliation anywhere
   in the tree. So the slot sum is `SCOUT_MAX_CONCURRENT + 1`, the steady state at defaults is
   3 of 6 rather than 4 of 6, and buildkit belongs on the *host memory* ledger (where, with
   the VM shapes, the defaults reserve ≈22 GB — the wall a small machine hits first). Do not
   copy the issue's "4 of 6" into the docs or into `Capacity`; a test pins the exclusion.

3. **`0` has to be refused, not clamped.** A pool with `max_vms: 0` binds its socket, answers
   `status` cheerfully, and fails *every* allocate with `pool exhausted` — it silently
   reproduces the exact failure this issue is about. Same for a typo: falling back to 6 runs a
   capacity nobody chose. Empty/whitespace reads as unset, which is different from wrong.

4. **Do not test this with `std::env::set_var`.** It is `unsafe` in edition 2024 and races
   every other thread in the test binary; nextest runs tests in threads. Hence the pure
   `max_vms_from(Option<String>)` seam.

5. **Do not add a second `status` round trip.** `report_attach_support` already made one per
   connect; folding capacity in beside it is why `reattach::support_of` exists.

6. **`.env` reaches `tasks vm-pool`.** `main` loads env files before subcommand dispatch, so
   `VM_POOL_MAX_VMS` can live in `.env` — and the real environment still outranks it.

### Blockers & Dependencies

None. Nothing here changes the wire protocol, no migration, no image rebuild. The default is
unchanged, so an operator who sets nothing sees only the new log lines.

Two adjacent problems found while doing this, neither of which blocks it:

- **`pool exhausted` still costs the task a strike, and this change does not fix that.** It is
  the literal reading of the issue's title. `Scout::dispatch`'s `allocate` returns
  `ClientError` → `ScoutError::Client` → `ScoutError::failure_class` → `FailureClass::Verdict`,
  so `record_outcome` charges a `dispatch_attempt`; three exhaustions in a row `reject` a
  perfectly good task. `BuilderError::failure_class` does the same to `build_attempts`.
  Waiving it is *not* a one-line change and must not be attempted as a rider: exhaustion is
  distinguishable from every other `ClientError` only by the string `"pool exhausted"`, and
  `CLAUDE.md` forbids deciding off reason text ("a reason is prose written for a human").
  Doing it properly means a structured error kind on the wire — `ServiceEvent::Error` carries
  only `message` — which is a protocol revision with its own skew story. **Worth filing as its
  own issue.**
- **`VM_POOL_SOCKET` is read by the client and by `tasks vm-pool`, but the *stock* service
  binary ignores it** and always binds `/tmp/vm-pool.sock`. Harmless today (nothing deploys
  the stock binary), and deliberately left alone — folding it into `from_env()` would change
  the stock binary's behaviour for anyone with that variable exported.

### Complexity

Medium — the code is small and mechanical, but it spans two trees with a dependency rule
between them, and the one non-obvious step (the second entry point) is the one that decides
whether the feature works at all.

### Notes

- Verified live on the scout VM, not just in tests: `VM_POOL_MAX_VMS=six` and `=0` each exit 1
  with `Error: VM_POOL_MAX_VMS must be a positive integer …`; `=9` logs `pool capacity
  max_vms=9` and listens; a server with `SCOUT_MAX_CONCURRENT=4` against a pool of 3 logs the
  `Short` warn (`needed=5 total=3`), with `=2` against 3 the `NoSlack` warn, and against 6 the
  `info!` with `spare=3`.
- `make test`: 654 passed, 0 failed. `cargo clippy --workspace --all-targets` and
  `cargo fmt --all` clean. The 6 LEAK results are the pre-existing scout/cancel timeout tests
  that `.config/nextest.toml` scores as pass.
- Tests added: 4 in `vm-pool-service` (unset/empty → default, positive integers, the refusal
  set including `0` and `-1`, the message naming variable and value) and 4 in `tasks::run`
  (`Capacity` at the shipped defaults — asserted against `DEFAULT_MAX_VMS` and
  `DEFAULT_SCOUT_MAX_CONCURRENT` rather than literals, so it moves when they do — the
  buildkit exclusion, shortfall including the degenerate `total = 0`, and the exact fit).
- The `warn!` wording matters as much as the check: both messages name the variable *and* the
  fix, and the `Short` one names the alternative (lower `SCOUT_MAX_CONCURRENT`), because the
  operator reading it is the person who can act and this is the last moment they can.

## Spec 2 of 3: `make test` fails spuriously about 1 run in 3: the reattach transcript assertion is flaky, and agents read a red suite as a verdict on their own work (#926)

## Spec: Take the write lock up front — every store transaction reads before it writes, so a deferred `BEGIN` loses the race unretryably

### Summary

`make test`'s reattach flake is not a test bug. Every write transaction in
`crates/tasks/src/store.rs` opened with `Pool::begin()` — a bare, *deferred*
`BEGIN` — and every one of them reads before it writes
(`append_transcript_lines` reads `MAX(seq)`; `reconcile_orphaned_work_except`
runs three SELECTs; `claim_next_queued_build` reads the head of the queue). A
deferred transaction takes its read snapshot at the first `SELECT` and only
asks for the write lock at the first `INSERT`, and SQLite refuses a *contended
upgrade* **without consulting the busy handler** — a reader made to wait would
deadlock the writer it is waiting on. So the 5s `busy_timeout` that
`Store::open` sets for exactly this overlap never applied to the case that
matters, and the loser failed instantly with `SQLITE_BUSY` (5) or
`SQLITE_BUSY_SNAPSHOT` (517), rolling its whole batch back. Two `Store`s on one
file, appending 100 lines each 1ms apart, lost **82–96 of 200 appends**; with
`BEGIN IMMEDIATE` they lose none. In the transcript writer that loss was
swallowed as a `warn!`, and a test process installs no subscriber, so the
missing reattach marker had no explanation attached — which is how a one-line
lock bug got read as a bug in the code under test. Fix: one `begin_write`
helper (`BEGIN IMMEDIATE`) behind all 24 transaction sites, a retry for the two
background writers whose errors have nobody to return to, and the failure made
loud. Two *further* races in the same test — both independent of the lock bug,
both reproduced under load here — are fixed alongside it, because the issue's
real subject is a suite an agent can trust.

### Implementation Approach

**1. The root cause — `crates/tasks/src/store.rs`.**
- Add a free `async fn begin_write(pool: &SqlitePool) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, StoreError>`
  returning `pool.begin_with("BEGIN IMMEDIATE")` (sqlx 0.8 has `Pool::begin_with`;
  it `exec`s the statement eagerly and verifies the connection ended up in a
  transaction), plus a private `Store::begin_write(&self)` that forwards to it.
- Replace all 24 sites: 23 × `self.pool.begin().await?` → `self.begin_write().await?`,
  and the one free-function site in `sweep_transcript_credentials` →
  `begin_write(pool).await?`. Every one of them is a write transaction; none
  should stay deferred.
- Extend `Store::open`'s doc comment. It currently claims WAL + `busy_timeout`
  make overlap wait its turn; that is half the story and the missing half is
  what this issue cost. Say that a busy timeout only helps statements the busy
  handler is consulted for, and that a deferred read→write upgrade is not one.

**2. Contention retry for the writers nobody is downstream of — `store.rs`,
`transcript.rs`, `scout.rs`.**
- `StoreError::is_contention()`: true for `Sqlx(Database(e))` whose `code()`
  parses as an integer with `code & 0xff` in `{5, 6}`. sqlx reports SQLite's
  *extended* code, so match the primary code in the low byte rather than
  enumerate spellings (`5`, `517`, `261`, …).
- `pub async fn retry_on_contention<T, F, Fut>(write: F)`: up to 3 attempts,
  50ms then 200ms, `warn!` on each retry. Safe by construction — the writes it
  wraps are whole transactions, so a rejected one wrote nothing and a retry
  cannot double a line or skip a `seq`.
- Use it in exactly two places: the transcript writer task
  (`spawn_transcript_writer`) and the scout checkpoint writer
  (`spawn_checkpoint_writer`). Both are detached tasks with no caller to return
  an error to. Everything else must keep propagating — a retry in a request
  path only makes a visible failure slower.
- This is a belt, not the fix. Say so in the comment, or the next reader will
  take the retry for the remedy and wonder why `begin_write` is there.

**3. Make the loss loud — `crates/tasks/src/transcript.rs`.**
- `TranscriptSink` gains `rejected: Arc<AtomicU64>`, shared with the writer
  task, distinct from `dropped_total` (that one is deliberate backpressure the
  sink announces in the transcript itself; this one is content accepted and
  then lost, leaving no hole because `seq` is assigned at persist time).
- The writer bumps it and logs at `error!` with `lines = batch.len()`;
  `flush` clones the `Arc` *before* dropping the sink and reads it *after*
  awaiting the writer, then `error!`s the total.
- The unit test that constructs a `TranscriptSink` by hand needs the new field.

**4. A subscriber the tests can read — `crates/tasks/tests/common/mod.rs`.**
- `capture_warnings() -> Warnings`, a `LazyLock` that `try_init`s a
  `tracing_subscriber::fmt` at `Level::WARN` writing into a shared
  `Arc<Mutex<Vec<u8>>>`, and `Warnings::seen() -> String` (or `"(none)"`,
  which is itself informative: it rules the log out).
  `tracing-subscriber` is already a `[dependencies]` entry of `tasks`, so it is
  available to the integration tests without touching `Cargo.toml`.
  `try_init` and a shared buffer, because under plain `cargo test` several
  tests share one process and only the first gets to install a subscriber.
- `a_restart_reattaches_to_a_scout_instead_of_orphaning_it` calls it first
  thing and puts `warnings.seen()` in the marker assertion's failure message.
  With it, that failure now prints
  `ERROR tasks::transcript: persisting transcript lines failed; the batch is
  lost … (code: 5) database is locked` right under the missing line.

**5. Two more races in the same test — `crates/tasks/src/run.rs`,
`crates/tasks/tests/reattach.rs`.** Both are load-dependent and neither is
caused by the lock bug; both were reproduced here under a CPU-burner load.
- `reconcile_startup_except` now returns `ReconcileReport` instead of `()`
  (`reconcile_startup` swallows it with `?; Ok(())`; the `run()` call site is
  `.await?;` and is unaffected). The test asserts the report is
  `Default::default()` instead of reading the session row back as `Running` and
  the task as `Scouting`. **4 failures in 40 loaded runs** before
  (`left: ScoutSucceeded, right: Running`): `resume_in_flight` *spawns* the
  reattach, everything it needs is already in the pool's event log, so on a
  loaded machine it concludes the row before the next line executes. The report
  is race-free and a stronger statement of the invariant — reconciliation wrote
  nothing off.
- The "carried through to a spec" wait becomes `task.state == InReview` instead
  of `!list_specs().is_empty()`. **4 failures in 300 loaded runs** before
  (`left: Scouting, right: InReview` at the task-state assertion):
  `finalize_succeeded` writes the spec first, the queue entry second and the
  task state last, so "a spec exists" is the earliest of the three watermarks
  and everything asserted after it was racing the rest of the finalize.

### Discovered Pitfalls

- **`Pool::begin_with` is sqlx 0.8+.** Present in the pinned `sqlx = "0.8"`.
  It rejects a custom statement when already inside a transaction (SQLite needs
  a `SAVEPOINT` there) — irrelevant here, nothing nests.
- **`BEGIN IMMEDIATE` is not a general "retry everything" licence.** It removes
  the *unretryable* class. A residual `SQLITE_BUSY` is still possible —
  SQLite documents that it may skip the busy handler whenever waiting could
  deadlock — which is why step 2 exists and why it is scoped to the two
  invisible writers.
- **The in-memory store is `max_connections(1)`**, so no unit test on
  `open_in_memory` can ever show this. The regression test has to be
  file-backed with two `Store`s. That is also the honest model: a `reload`
  hand-over really does put two *processes* on one file, where no in-process
  lock would help.
- **Foreign keys are on.** sqlx enables `PRAGMA foreign_keys=ON` by default, so
  a transcript-lines test needs a real project → task → session seeded first.
- **`ReconcileReport` derives `PartialEq`/`Default` already** — no derive
  changes needed for the report assertion.
- **The `#[tokio::test]` default runtime is current-thread.** The two-writer
  regression test is written `flavor = "multi_thread", worker_threads = 2`;
  it does reproduce on current-thread (sqlx runs each connection on its own
  thread), but don't rely on that.
- **Verify the tree before believing a measurement.** Two of the load
  experiments during this scouting run were invalidated by a scripted revert
  whose `str.replace` silently matched nothing, and one by a hardcoded test-
  binary hash that had gone stale next to a newer build. `grep -n begin_with
  crates/tasks/src/store.rs` and pick the binary by mtime.

### Blockers & Dependencies

None. Touches no migration, no wire type, no image, and nothing under
`crates/vm-pool/`. The `reconcile_startup_except` signature change is internal
to the `tasks` crate.

### Complexity

Medium — the code change is small and local, but the reasoning about why a
busy timeout did not apply is the load-bearing part, and there are three
independent flakes in one test to keep apart.

### Notes

- **Regression tests.** `store::tests::two_stores_on_one_file_both_get_their_transcript_lines_in`
  is the guard for the root cause: two `Store`s on one tempfile, 100 appends
  each 1ms apart, asserting no rejections *and* dense 1..200 seqs. It fails
  deterministically without the fix (`82 of 200 appends were rejected`) and
  passes with it, so it is worth writing before the fix.
  `store::tests::a_contended_write_is_told_apart_from_a_broken_one` pins the
  classifier against an error SQLite really produced — manufacture it with the
  shape `begin_write` prevents: a deferred `pool.begin()`, a `SELECT`, someone
  else's commit, then an `INSERT` (→ 517). Also assert a `NotFound` and a
  `SELECT nope FROM tasks` are *not* contention.
- **Verification.** `make test` green ×3 (648 tests). `cargo fmt` and
  `cargo clippy --workspace --all-targets` clean. The reattach test ran 300×
  and the whole reattach binary 120× under a 6-way CPU-burner load on 4 cores:
  0 failures. Before the fixes, the same loop produced 4/40 (`Running`), 4/300
  (`InReview`) and 1/~440 (the transcript marker, the reported one).
- **This machine reproduces the reported flake at ~0.2%, not 1 in 3**, so the
  end-to-end loop is corroboration and the deterministic store-level test is
  the proof. The one live failure that was captured names its own cause in the
  log, which is what change 4 buys.
- **This was never only a test bug.** The transcript writer and the checkpoint
  writer commit side by side for the whole of every scout, and a `reload`
  hand-over overlaps two processes. Anything that lost the race lost a whole
  batch silently — and for the checkpoint writer that batch is the salvage a
  cut-short run is judged on, which is the one artefact `NOTES.md` streaming
  exists to protect.
- `crates/tasks/tests/transcript.rs` is the only other test asserting on
  transcript *content* through this path (issue asked). Lower exposure, same
  failure mode, no change needed once the root cause is fixed.

## Spec 3 of 3: The orchestrator verifies in a cold scratch worktree it cannot afford, so it can only ever merge on a typecheck (#910)

## Spec: Give the orchestrator a warm build directory and a command ceiling below its turn

### Summary

`land_builds` ships `live`, but the orchestrator could not run this repository's
test suite, so the only evidence available to a merge decision was a typecheck
and the Builder's own `Verification:` claim. The cause was never the suite —
warm, the whole workspace is 565 tests in ~21s — it was *compilation*: verifying
that N pull requests compose means checking them out somewhere, a `git worktree`
gets its own empty `target/`, and a cold workspace debug build is minutes before
a single test runs. Three things compounded it: a 600s turn budget against
Claude Code's own 600s per-command ceiling (so a command could consume the whole
turn and leave nothing to report in — the observed "killed before writing
output"), and, when the agent avoided the worktree, contention with
rust-analyzer for the live checkout's build-directory lock. The fix is three
environment variables set on the orchestrator's **child process only** —
`CARGO_TARGET_DIR` pointed at a shared, long-lived build directory, and
`BASH_DEFAULT_TIMEOUT_MS`/`BASH_MAX_TIMEOUT_MS` derived as half the turn budget
— plus the prompt changes that make the new capability real: a generated
verification section, and a `land_builds` carve-out that no longer asserts
"nothing re-runs its tests for you" on a host where something does.

### Implementation Approach

**`crates/tasks/src/run.rs`**
- New `ORCHESTRATOR_TARGET_DIR` env var → `Config.orchestrator_target_dir:
  Option<PathBuf>`, resolved by a new `Config::orchestrator_target_dir()`
  defaulting to `<data dir>/verify-target` (constant
  `DEFAULT_ORCHESTRATOR_TARGET_DIR`).
- `DEFAULT_ORCHESTRATOR_TIMEOUT_SECS` **600 → 900**. Fifteen minutes leaves room
  for the one cold build; it is bounded above by `OBLIGATION_REMINDER` (30 min),
  so a turn can never outlast the interval at which the pipeline re-states what
  it is owed. This is explicitly *with* the warm directory, not instead of it —
  alone it would spend more wall-clock on the same cold build.
- `orchestrator_loop` resolves and `create_dir_all`s the directory **once per
  boot**, and passes `target_dir: Some(_)` only when `orchestrator_workdir`
  names a checkout *and* the mkdir succeeded. A failure `warn!`s and degrades to
  `None`. Doing it here rather than per-turn is what keeps the prompt honest: it
  can never name a directory the agent will find missing.

**`crates/tasks/src/orchestrator.rs`**
- `OrchestratorConfig.target_dir: Option<PathBuf>`. `invoke()` sets
  `CARGO_TARGET_DIR` (when `Some`) and both bash-timeout variables on the child,
  next to the existing `env_remove` calls.
- New `command_budget(turn) = (turn / 2).max(MIN_COMMAND_BUDGET.min(turn))`,
  with `MIN_COMMAND_BUDGET = 60s`. Derived rather than configured — a second
  knob is a second thing to get wrong, and *half* is the statable guarantee:
  whatever a command spent, at least that much turn is left to report it.
- New `verification_section(target_dir, turn) -> String`, **empty when
  `target_dir` is `None`** — same shape as `degradation_section`, so an
  undirected environment grows no heading. It names the directory, forbids
  overriding/`cargo clean`ing/deleting it, says it follows the agent into a
  `git worktree`, splices in both budgets, states that backgrounding buys
  nothing (the child dies with the turn), points at `make test`/nextest and asks
  for the missing-tool case to be *named* rather than silently downgraded to
  plain `cargo test`, and tells the agent that a genuinely cold first build
  spanning two turns is expected rather than a failure.
- `landing_section(charter, can_verify)` — the `Live` arm gains a second
  variant. Carve-out (b) becomes "no passing run backs it **and you could not
  make one**", with the instruction to check the PR out and run the suite, and
  handing over reserved for when a run genuinely could not be produced.
  (a) and (c) are unchanged. `Shadow`/`Off` are unchanged.
- `system_prompt` now takes `&OrchestratorConfig` instead of five positional
  parameters (it would have been seven). `can_verify = workdir_is_checkout &&
  target_dir.is_some()` is computed once and read by both dependent sections, so
  they cannot disagree about what the host can do.

**`crates/tasks/src/brief.rs`** — `verification_line`'s "nothing downstream
re-runs them" / "nothing downstream will make one" become "no automated check
…". Narrow but necessary: leaving a sentence that says the run will not happen,
beside a landing section that says go make one, is the two-sources-of-truth
failure this codebase legislates against. What the *pipeline* does not do and
what its *reader* cannot do are different facts; the brief now states only the
first.

**`Makefile`** — `make verify-warm` primes the directory so the first merge
decision is not also the first cold build. Path resolution mirrors
`Config::orchestrator_target_dir`; the recipe sets `CARGO_TARGET_DIR` **inline
on the one command**, never as a make-level export, because an escaped one would
redirect `TEST_BIN_DIR` and the suites would look for binaries nothing built.

**`CLAUDE.md`** — two env-table rows, `make verify-warm` under Running, and a
new load-bearing bullet after "An open PR is chased like every other stage".

### Discovered Pitfalls

- **Claude Code's timeout semantics, read out of the shipped binary** (v2.1.233,
  `/usr/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`): the default
  per-command timeout is **120000ms** and the ceiling **600000ms**, both
  overridable, and the ceiling is computed as
  `max(BASH_MAX_TIMEOUT_MS, effective_default)` — so setting only the default
  raises both. Both are set explicitly anyway. Setting only the *max* would have
  left un-annotated commands at 120s, which is the majority of them.
- **`<data dir>/.env` is the wrong home for this.** That file is read by every
  `tasks` invocation, so a `CARGO_TARGET_DIR` there would be inherited by
  `tasks reload`'s own build of the server and would silently redirect the
  Makefile's `TEST_BIN_DIR` (`CARGO_TARGET_DIR ?= target` at the top of the
  Makefile derives it). The setting belongs to the verification, so it is scoped
  to the one child process.
- **There is no `off` switch, deliberately.** Every value here is a path, so a
  sentinel that could also be a directory name is a worse ambiguity than the one
  it resolves. `ORCHESTRATOR_TARGET_DIR=<checkout>/target` restores the old
  behaviour exactly, and is the escape hatch.
- **`?=` is wrong in the Makefile.** It treats an exported-but-empty variable as
  set, while the server's `env_string` filters empty out; the two would then
  disagree and `verify-warm` would warm a directory nothing uses. The target
  uses `$(if $(strip ...))`. There is no `export` directive in the Makefile, so
  the new variables do not leak into `make serve`'s environment — verified.
- **`Config` and `OrchestratorConfig` are constructed positionally in five test
  files.** Adding a field is a compile error in `crates/tasks/src/run.rs` (test
  cfg), `tests/common/mod.rs`, `tests/run.rs`, `tests/reattach.rs` and
  `tests/orchestrator.rs` (×2). All updated.
- **The existing prompt test asserts `!p.contains('$')`** (a command with a
  shell variable in it is not statically verifiable under `Bash(curl:*)`), so
  any new prompt prose must be dollar-free.
- **A pre-existing flake, not a regression from this work:**
  `tasks::reattach a_restart_reattaches_to_a_scout_instead_of_orphaning_it`
  fails roughly 1 run in 3. Confirmed by stashing every change and reproducing
  it on unmodified HEAD; `make test-ci` reports it green as `FLAKY 3/3`. Worth
  its own issue (it asserts a transcript index on a bounded replay,
  `crates/tasks/tests/reattach.rs:313`) — do not chase it here.

### Blockers & Dependencies

None. No migration, no image rebuild, no vm-pool change — everything is
host-side and takes effect on the next server start. `cargo-nextest` needs to be
installed on the host for `make test` to work, which the issue reports is now
the case; the prompt asks the agent to *name* the refusal rather than silently
fall back if it ever is not.

### Complexity

Medium

### Notes

- **The prompt half is the load-bearing half.** Adding the target directory
  without changing `landing_section` would leave the fix inert: the agent would
  have somewhere warm to build and a standing instruction saying "nothing
  re-runs its tests for you", which is what made the typecheck the ceiling. Both
  sections are generated from the same computed `can_verify`, per the standing
  rule that anything the prompt claims about the environment is read off the
  environment.
- **This does widen `land_builds` autonomy**, and that is the point of the issue
  rather than a side effect: the charter's own principle is that what sends a
  batch back to a human is unverifiability, not risk. An orchestrator's own run
  is *stronger* evidence than the Builder's trailer — a check rather than a
  claim. Carve-out (c) (the app-gpui rendering case) is untouched and still
  routes to a human, and handing over remains available whenever a run genuinely
  could not be produced.
- **Verifying a composition stays the orchestrator's own work, not a new
  Builder-shaped VM run.** The directions asked this explicitly. The evidence
  has to reach the merge decision inside the turn that makes it, and a VM-shaped
  "do these N PRs compose and pass" run would need its own run kind, its own
  artifact, its own charter capability and its own answer to the Scout/Builder
  barrier — all to deliver what a `git worktree` plus a warm directory now
  deliver in seconds. If compositions ever grow past what a 15-minute turn can
  hold, that is the moment to revisit it, and it is a separate issue.
- Expect ~7.5 GB on disk for the shared directory once warm. Nothing prunes it,
  by design — the warmth is the whole value, and `cargo clean` on it puts the
  problem back.
- Tests added: `verification_is_described_only_where_it_is_possible`,
  `a_command_can_never_outlast_the_turn_that_reports_on_it`,
  `a_host_that_can_run_the_suite_is_told_to_run_it_before_handing_over` (unit),
  and `the_agent_gets_a_warm_build_directory_and_a_command_ceiling_below_its_turn`
  (integration, a real child process that dumps its environment — this is the
  one that pins the *scope*: with `target_dir: None` the child sees exactly what
  the parent had, neither cleared nor invented).
- Verified green: `cargo fmt`, `cargo clippy --workspace --all-targets`,
  `cargo nextest run --workspace --no-fail-fast` (675/675), `cargo test --doc
  --workspace`.

## Your job

1. Implement every spec above, in order, as one coherent change in the cloned repo (cwd). You are on the right branch already.
2. Run the project's tests / lint / typecheck — get them green.
3. Commit your work with clear messages (a git identity is configured).
4. Write `SUMMARY.md` in the repo root: one or two paragraphs describing the change, suitable as a pull request body. Do not use GitHub closing keywords (`Closes #N`, `Fixes #N`) — the server links the issues itself.
5. End `SUMMARY.md` with one line saying whether you actually ran the tests, in exactly this shape:
`Verification: PASSED — <the command you ran>`
`Verification: FAILED — <the command, and what failed>`
`Verification: NOT RUN — <why not>`
Report what actually happened. Nothing re-runs this suite for you downstream, so this line is the only evidence anyone has that the change works — claiming a run you did not make is the one thing here that cannot be caught later, and "NOT RUN" costs the batch a look from a human rather than costing you anything.
6. Do NOT push and do NOT open a PR — the server does both.
