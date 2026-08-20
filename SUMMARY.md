# Measure and bound `ORCHESTRATOR_TARGET_DIR`, and close the source of its growth

The orchestrator's warm verification build directory had no bound and, worse, no
report: `CLAUDE.md` promised "~7.5 GB and nothing prunes it", a human hunting for
disk found 39 GB (#1010), and a measurement on 2026-08-20 found **51 GB** on a
filesystem with 74 GiB free, growing ~2 GB per verification. The growth was never
the warmth. Cargo keys an artifact on a metadata hash that **includes the source
path**, and the prompt told the agent a `git worktree` "costs you no extra
compilation" — true of the registry dependencies, whose hash does not include
the workspace path, and false of every workspace crate, so each fresh worktree
path added a complete new set of artifacts and the previous set was kept forever.
So this is three parts, in descending order of how much they matter. **Report
the size** on `/status`, `tasks status` and the Server window — and unlike the
three dispatch holds beside it, that row prints *whenever there is a reading*,
because a hold is an exception while this is a quantity that grows silently, and
a row appearing only once it was over its ceiling would reproduce #1010 exactly.
**Stop the bleeding**: `verification_section` now names one reused worktree
(`<data dir>/verify-worktree`, derived, not a knob) and spells out the
`reset --hard` + `clean -fd` + `checkout --detach` sequence that keeps it
reusable — a pull request is verified by merging the trunk into its head, so the
worktree arrives carrying last time's merge commit and a bare `git checkout`
refuses, and a wedged worktree means *no* verification at all, which routes every
batch to a human. **Bound what is left** with a graduated reclaim past
`ORCHESTRATOR_TARGET_BUDGET_GB` (20; `0` keeps the report and drops the reclaim,
the `TASKS_UPDATE_HOLD=off` shape): tier 1 removes every `<profile>/incremental`,
which is keyed to one worktree path and costs no warmth, and only if that leaves
it over does tier 2 empty the directory while keeping the directory itself.

`crates/tasks/src/verify_dir.rs` holds all of it: one in-memory dated reading on
the `github_health`/`pool_health` precedent, a 15-minute cadence claimed on the
*attempt* rather than on success, hardlinks counted once so the number agrees
with `du -sh`, and no store access — `run::maintain_verify_dir` appends the
`Note`. It runs from `orchestrator_loop` **before** each `tick()`, which is the
safety argument: that loop is the only thing that starts a process in there, so
a deletion cannot race a compile by construction rather than by a lock; while a
human has the session checked out it measures, reports and reclaims nothing. A
reclaim is `Actor::System` maintenance of a local cache with no charter gate,
affordable in a way reclaiming a rejected bundle is not, because everything here
is reproducible from the checkout — a deletion costs time, never work. The one
cost that must not be paid quietly is the wholesale tier making the next
verification cold, which leaves carve-out (b) undischarged and sends that batch
to a human, so it is announced on the feed and stays on `/status` for the boot.

## Review feedback

- **Drop the executables-only tier entirely, and do not leave it recorded as a
  future refinement.** Done — it is implemented nowhere and recorded nowhere as
  an idea. The measured numbers replaced it, with their date, in
  `verify_dir.rs`'s module doc and in the new `CLAUDE.md` bullet: `deps/`
  46.79 GB of which 35.24 GB is 208,468 codegen-unit `.o` files, executables
  6.14 GB (13%), `.rlib` + `.rmeta` 5.2 GB, `incremental/` 24.24 GB, total
  51 GB, 2026-08-20 — together with why the tier is wrong (macOS's default
  `split-debuginfo = "unpacked"` leaves the debuginfo *beside* the binaries, so
  evicting them frees 13% and leaves all 35 GB it was aimed at).
- **Measure the debuginfo level before shipping it; a measured "no" is a good
  outcome.** Measured, and it is a yes. `cargo test --workspace --no-run` into a
  scratch target dir: **6,263,737,442 bytes (6.26 GB)** at the default profile
  and **3,156,867,179 bytes (3.16 GB)** at `line-tables-only` — 3.11 GB, 49.6%,
  off one build. A deliberately failing test under `RUST_BACKTRACE=1` produced a
  **byte-identical** backtrace to the default build's, naming a file and a line
  in every frame. So `CARGO_PROFILE_DEV_DEBUG` and `CARGO_PROFILE_TEST_DEBUG` are
  both set to `line-tables-only` (both profiles, since `cargo test` builds under
  `test` and `cargo build` under `dev`), never `debug = 0`. Caveat stated rather
  than hidden: this builder is Linux, where debuginfo is embedded rather than
  split, so the *distribution* between `.o` files and binaries differs from the
  macOS host — the volume being removed is the same, and the direction is not in
  doubt, but the exact GB on that host will differ.
- **Set it in both places or neither.** `orchestrator::VERIFICATION_ENV` is the
  one list (`CARGO_INCREMENTAL=0` plus the two debuginfo settings); the child
  sets it beside `CARGO_TARGET_DIR`, `make verify-warm` sets the same three, and
  `verification_env_matches_the_makefile` reads the Makefile and fails when they
  drift. `the_agent_gets_a_warm_build_directory_and_a_command_ceiling_below_its_turn`
  pins the child half in both directions — including that none of the three is
  invented when there is no build directory.
- **`incremental/` is 24.24 GB, not the smaller figure the tiering implied.**
  Corrected everywhere it is stated. It is now explicit that tier 1 alone
  reclaims 24 of 51 GB on that host, and that with `CARGO_INCREMENTAL=0` on the
  child it is mostly clearing what accumulated before this shipped rather than
  something the pipeline keeps making.
- **The `gpui` artifacts are a dependency tree `make test` never builds.** Stated
  in the design bullet, as a measured fact with the consequence named: the
  steady-state size depends on whether app builds share this directory, which
  nobody has decided. Nothing here decides it.
- **"A cold verification reports `Verification: NOT RUN`" is the wrong model.**
  Agreed and not repeated. The orchestrator emits no `Verification:` trailer —
  that is the Builder's claim about its own run. Every place this change
  describes the cost says instead that the orchestrator's own run does not
  complete, so carve-out (b) is not discharged and the batch goes to a human.
- **Say the free-space number beside the first-boot note.** The design bullet
  records 51 GB against 74 GiB free. For whoever deploys this: the first boot
  will find the directory over its ceiling and reclaim it, which on that host
  means one wholesale pass and one cold verification. That is announced on the
  feed. To look before it happens, boot once with
  `ORCHESTRATOR_TARGET_BUDGET_GB=0`, read the size off `tasks status`, and choose
  the ceiling from the real number.
- **Recorded so it is not re-litigated** — all kept as approved: the in-memory
  dated reading, the 15-minute cadence, hardlinks counted once, `measure_due`
  claiming on the attempt, running before `tick()`, reclaiming nothing while the
  session is checked out, the wholesale tier keeping the directory, printing
  whenever there is a reading, `Note` rather than an obligation, and
  `ORCHESTRATOR_TARGET_BUDGET_GB=0` keeping the unswitchable report.

## Directions

- **Treat the measurements as data and put them in the doc with their date.**
  Done, in `verify_dir.rs` and `CLAUDE.md`, dated 2026-08-20.
- **Do not take the debuginfo instruction on faith.** Both builds were run and
  both numbers are above; the backtrace was checked against a control.
- **Delete the executables-only tier rather than deferring it.** Done.
- **Spell out the worktree sequence, because a wedged worktree is the failure
  direction that matters.** Done — `git worktree add --detach` once, then
  `fetch` / `reset --hard` / `clean -fd` / `checkout --detach` before each use,
  with the reason stated in the prompt itself, and pinned by the unit test on
  `verification_section`.
- **Read `orchestrator.rs` and `CLAUDE.md` as they are, not as the spec quotes
  them.** Done; #1020's rewrite of `verification_section` is not in this
  checkout, so the section rewritten here is the one on trunk.
- **Report the real test number, and use `cargo test --bin tasks-gpui` for the
  app half.** Below, and in the verification line.

## Not done, and why

- `doctor.rs` repeated the stale ~7.5 GB figure twice in its "verify target dir"
  check. It is not named in the spec, but leaving a corrected number in one file
  and a wrong one in another is the drift this change exists to stop, so both
  sentences now cite the measured 3.2 GB warm build and point at
  `tasks status` and `ORCHESTRATOR_TARGET_BUDGET_GB`.

## Verification run here

- `make test` — **991 tests, 991 passed** (6 slow, 7 leaky: the documented
  scout-timeout LEAKs), plus doctests, all green.
- `cd app-gpui && cargo test --bin tasks-gpui` — **255 passed, 0 failed**,
  including the two new `server_window` tests. `make app-test` is not used here
  per the earlier finding that linking the `tasks-menubar` test binary is
  OOM-killed in a builder VM.
- `cargo fmt --all` clean, `cargo clippy --workspace --all-targets` clean.
- The debuginfo measurement itself: two full `cargo test --workspace --no-run`
  builds into scratch target directories, 6.26 GB vs 3.16 GB, and a deliberately
  failing test run under each with `RUST_BACKTRACE=1` for the backtrace
  comparison.

Verification: PASSED — `make test` (991/991, plus doctests), `cd app-gpui && cargo test --bin tasks-gpui` (255/255), `cargo fmt --all`, `cargo clippy --workspace --all-targets`
