# Tasks (v2)

A human-in-the-loop platform that orchestrates coding agents (headless Claude
Code) to get project work done, built around the Double Diamond architecture
(issue #744): parallel Scout exploration → spec queue → serial Builder
implementation.

## Load-bearing design rules

- **The Scout/Builder information barrier is inviolable.** Builders never see
  Scout code — the spec is the deliverable. Specs are text, so a Builder run
  can batch N specs into one branch. Never propose reusing Scout branches.
- **Salvage is never a spec.** A Scout writes two files with two meanings:
  `SPEC.md` means "I concluded", `NOTES.md` means "here is what I have so
  far". Notes stream back as checkpoints during the run (the VM is destroyed
  at the deadline, so nothing collected at the end survives) and land in
  `scout_notes` — one row per session, no `Spec`, no queue entry, no review
  path. Their only consumer is the next attempt's prompt, where they are
  quoted as explicitly unverified leads. Reporting a partial spec *as* a spec
  would be worse than losing the run, because a half-explored spec in the
  review queue looks finished. Promoting notes into a spec stays a human act.
  A human may also skip the Scout outright — `POST /tasks/{id}/build-now`
  writes a spec by hand for a task whose issue body already is one, and its
  `specs.session_id IS NULL` is the tell. It changes nothing below: the spec is
  text, so the Builder cannot distinguish it. What it skips is not only the
  scouting but the **review**, since there is no independent artifact to rule
  on — hence a single `author_spec` decision row rather than an `approve`,
  which would claim a second opinion that does not exist. It is human-only and
  refuses the orchestrator outright rather than being charter-gated: authoring,
  approving and dispatching one's own work with no second opinion anywhere in
  the loop is a different autonomy from `dispatch_builds`, and if it is ever
  granted it wants its own named capability.
- **Never persist a GitHub-owned fact** (PR mergeable/SHA/CI, issue
  open-closed, labels). Query at decision time. Persist only Tasks-owned
  state plus append-only decisions keyed to immutable SHAs. GitHub writes go
  through the server, never through agents.
- **`done` means shipped, and it is written in exactly one place.** A build
  that opens a PR has made a claim, not a delivery, so it parks its batch in
  `awaiting_merge`; the only thing that writes `done` is closure-derived
  retirement, so `done` always means "the issue is closed upstream". Each poll
  reads the unresolved PR (`watch_merges`) and either closes the issue as
  completed — through the server, under `retire_work`, with the merge commit
  as evidence — or unwinds the batch back to `ready_to_build` with a build
  attempt charged. Unwinding restores the *option* to rebuild; nothing
  dispatches a build by itself, which is what keeps that safe. The cost is one
  REST call per open Builder PR per poll, forever, and that is the right
  price: the moment it is cached in a `last_checked` column, the thing being
  cached is a GitHub-owned fact with a timestamp on it.
- **A PR's `merged` means "reached its base", never "shipped" — the pipeline
  stacks builds routinely.** A build based on another build's branch reads
  `merged: true` the instant that branch takes it, whether or not the branch
  ever lands, so `watch_merges` resolves `awaiting_merge` on whether the merge
  commit is an **ancestor of the trunk** (`SCOUT_BASE_BRANCH`) and never on
  `merged`. `run::shipped` is that predicate: `base_ref == trunk`
  short-circuits, so the ordinary unstacked case costs no extra call, and only
  a stacked PR spends a `GET /compare/{trunk}...{sha}` — which reads *head
  relative to base*, so reachable is `identical` or `behind`, and reversing the
  operands inverts the verdict. Every unreadable answer is `false`, because the
  two mistakes are not symmetric: staying parked costs one call on the next
  poll, while concluding wrongly writes `done` over work that shipped nothing
  and no pass revisits `done`. This is not hypothetical — **PR #863 was
  `merged: true` and its work never reached `main`**, which is the failure
  above, one level up. A batch that has merged but not reached the trunk
  therefore **stays parked rather than unwinding**, and that is what makes both
  manual merge orders safe: merge the base first and the dependent's commit is
  already reachable, or merge the dependent first and a later poll finds it
  once the base lands. Reachability is monotone, so polling can never un-ship
  something it concluded had landed. Nothing auto-unwinds a stranded batch;
  `ObligationKind::LandBatch` makes it loud instead, and it is the first
  obligation whose subject is a **build** id rather than a spec id.
- **An open PR is chased like every other stage, and the default is to land
  it.** The pipeline used to dead-end at PR open: the `land_batch` bullet ended
  "landing it is the human's" while the charter shipped `land_builds` **live**,
  so every parked batch was reported and none was merged. That sentence is now
  *generated* from the charter row (`orchestrator::landing_section`), the way
  the authority and workdir sections are — the fix for a prompt contradicting
  the charter is never a better sentence, it is one source. What sends a batch
  back to a human is **unverifiability, not risk**, because "hand it over when
  in doubt" is what the old sentence effectively said and doubt is unbounded.
  So there are exactly three carve-outs, named as the whole list: GitHub would
  refuse the merge, the build reported no passing test run of its own, or
  nothing runnable here could have checked it. The third exists because this
  repository has **no `.github/workflows` and no branch protection**, so
  `mergeable_state` can only ever be `clean` or `dirty` and GitHub's verdict is
  structurally incapable of objecting to a change that does not work —
  `Landing::Clear::describe()` says so in words, and a test pins that clause
  and the absence of "ready to merge". Read `mergeable_state` and never
  `mergeable` alone: `false` there means a conflict and nothing else, so a red
  PR reads ready. The only evidence that a change works is therefore the
  Builder's own run, which it states as a `Verification: PASSED|FAILED|NOT RUN`
  trailer in `SUMMARY.md` — a trailer and not a column, because the summary is
  already stored and already the PR body, so one sentence serves the human
  reading the PR and the brief reading it back with no migration, no
  `BuildEvent` field and no image rebuild in between. It is a **claim**, not a
  check, and the brief attributes it as one; every batch parked before it was
  asked for parses as `Unreported`, which reads as "no run on record" and goes
  to a human, which is the direction a mistake here has to fall. All of this
  lands on the *brief* rather than in the obligation: refining an obligation's
  **kind** after `Store::obligations` returns would lose its
  `(kind, subject_id)` reminder row and nag every tick instead of every thirty
  minutes, and it would cost a GitHub read per parked PR per tick rather than
  one per obligation actually surfaced. Mergeability is never cached — that is
  persisting a GitHub-owned fact with a timestamp on it.
- **Bulk intake never auto-dispatches, and queue membership is explicit.**
  `tasks.manual_rank` is set only via the API; the GitHub poller must never
  write it. Ingested issues land in `backlog` and are never dispatched — only
  explicitly queued tasks (`POST /tasks/{id}/queue` or `/scout`) reach a
  Scout, and picked-up work stays picked up (failures and `needs_revision`
  return to `queued`, not `backlog`). The invariant is that **bulk intake must
  not become bulk work**: adding a repo with 11,000 issues must not turn into
  11,000 Scout runs and 11,000 PRs nobody chose. It is not a human-judgment
  gate on any individual task, so deliberate per-task queueing by an
  accountable actor is fine — the orchestrator may do it when `queue_tasks` is
  live in the charter. The invariant is upheld by the pipeline's shape, not by
  rate limits: backlog never dispatches, `SCOUT_MAX_CONCURRENT` bounds scouts,
  and builds are serial. `POST /tasks/{id}/build-now` sits inside that shape
  rather than beside it: it is per-task, human-only, and one call is one build.
- **What the orchestrator may do lives in `orchestrator_charter`, never in a
  prompt.** Nine independently switchable capabilities (`capture_work`,
  `curate_work`, `comment_on_work`, `retire_work`, `queue_tasks`,
  `dispatch_builds`, `cancel_runs`, `auto_review_specs`, `land_builds`), each
  `off` | `shadow` | `live`, human-writable only. The system prompt's
  authority section is *generated* from those rows every turn and the server
  enforces the same rows on the endpoints — one statement of authority, and
  not one a long conversation can talk itself out of. Adding a capability is
  two edits, not one: the enum variant alone grants nothing, because
  `Store::charter_entry` reads a missing row as `off` — the migration's
  `INSERT` is what makes it real, and without it the refusal looks like a bug
  in `authorize`. **All nine ship `live`
  and uncapped** — the charter is a kill switch, not a promotion ladder, and
  the point of the system is that work moves without being asked. What makes
  that safe is the `decisions` ledger under every write: audit and recourse
  after the fact, never pre-approval. `shadow` (server behaviour, not an
  instruction: the call is accepted, the decision is recorded with
  `enforced = 0`, nothing is applied) exists only for **demotion** — a
  capability caught misbehaving, whose reasoning is still worth reading. It is
  never a probation period on the way to `live`; that costs the human more
  attention than just letting the thing act, and attention is the scarce
  resource here. The human is never gated — this governs autonomy, not the
  owner.
- **The charter only binds what the server can attribute, so attribution must
  work under the *tightest* agent permissions.** A write the server cannot
  attribute is recorded as the human's — and the human is never gated, so a
  broken credential does not fail closed, it *escalates*. The orchestrator's
  token therefore reaches it as a server-written `curl -K` config file (0600,
  under the data dir), which is a statically verifiable command under
  `--allowedTools Bash(curl:*)`. Never move that credential into argv (`ps`),
  the prompt (persisted), the environment (an agent under a static allowlist
  cannot expand `$VAR`), or the agent's workdir (a repo checkout it commits
  from). An `X-Tasks-Actor` that is present but does not verify is a 403, not
  a demotion to human.
- **Dependency direction:** `crates/vm-pool/*` are pure infrastructure and
  must never depend on tasks crates. App vocabulary enters vm-pool only
  through the `AppProtocol` generic (see `crates/tasks-protocol`). vm-pool
  stays independently publishable.
- **Agent engine is Claude Code / the Agent SDK — never a home-rolled agentic
  loop.** The server consumes Claude Code's typed output (stream-json, hooks,
  MCP tools, structured outputs); it does not reimplement the loop.
- **A dead API connection is resumed in the supervisor, never re-dispatched
  from the host.** Agent processes die intermittently at ~380s elapsed (#845)
  when the connection drops mid-response — below the agent, in the VM's network
  path, so nothing here can prevent it. What the supervisor can do is re-invoke
  the agent with `--resume <session_id>`, read out of the stream-json it is
  already forwarding: same conversation, same worktree, same `NOTES.md`. A
  host-side retry would get a new VM and a fresh clone and keep none of the
  three — and for a Builder that worktree *is* the implementation. The
  classifier and every guard live in `crates/tasks-protocol/src/agent_run.rs`,
  and **the guards are the load-bearing part**, because the failures you must
  not retry look superficially like the one you must: an OOM kill meets the
  same limit with a larger conversation, a missing terminal record means the
  host is deallocating this VM right now, and a command that already selects a
  session belongs to the operator. Two other rules hold that shape: read the
  session id, never inject one (`--session-id` would overwrite the operator's
  command), and never restate the task in the resume prompt — the task is above
  it in the conversation, and re-sending it is how a resume silently becomes a
  restart. A transport death also names itself in the terminal reason; "SPEC.md
  not found" or "no commits" alone reads as a verdict on work that was never
  judged. `dispatch_attempts` is still charged for one — see #845 for the
  remaining piece, which must key off a classification field on the event, not
  a string match on the reason text.
- **A build is whichever tip the reconciliation chooses, and the head is read
  out of the bundle — in that order.** `git rev-parse HEAD` and
  `refs/heads/<branch>` are the same commit only while HEAD stays symbolically
  attached to the host-chosen branch; a rebase, a `git checkout <sha>` or a
  branch of the agent's own detaches it, and the branch ref silently stops
  tracking the work. That is #891, where a finished build was discarded for
  `bundle tip … does not match the reported head …`. Both halves of the fix are
  load-bearing: `reconcile_checkout` decides which tip the build *is* (by
  ancestry, with a rebase guard ahead of it, since git parks HEAD on a partial
  replay while the branch still holds the complete history), and `package_bundle`
  then reads `head_sha` back out of the bundle, so there is **one** value where
  there were two that had to agree. Reading from the bundle alone makes the
  error disappear while shipping a stale tip; reconciling alone leaves the
  two-values problem. And the reconciliation runs **before the sweep**, because
  the sweep lands on whatever HEAD is: on a stranded checkout, sweep-first
  manufactures a divergence no ancestry rule can undo, and the PR ships the
  sweep with none of the implementation. Whatever the decision chooses against
  rides the bundle as `refs/abandoned/<branch>` and is never pushed, so no arm
  of it can lose a commit. The server's tip check stays, and now measures
  transport integrity rather than racing the VM — and it no longer costs the
  build, because the VM is deallocated before egress runs, so a rejected bundle
  is written to `<scratch_root>/rejected/` (deliberately never swept) with the
  `git fetch` that recovers it named in the failure reason.
- **A cancel interrupts the dispatcher's drain; it never just removes the VM.**
  `POST /sessions/{id}/cancel` and `POST /builds/{id}/cancel` write a durable
  `cancellations` row, and `crate::cancel::bounded` — the `tokio::select!` the
  wall-clock deadline already used, with a third arm — is what wakes the drain,
  which then tears the VM down through the path it always used. Killing the
  container by hand (or calling `deallocate` and nothing else) is precisely the
  bug (#876): the drain is parked on a vm-pool stream that will never produce
  another event, so the row stays `running`, the serial build lane stays
  occupied, and nothing tells the operator the cancel took. The request is a
  store row rather than a channel because the process taking the request need
  not be the one following the run — a run picked back up by
  `resume_in_flight` was never subscribed to anything. A cancelled run is
  `cancelled`, never `failed`: `exit_reason` names the actor and the rationale
  (the only thing that later tells a deliberate stop from a crash), no dispatch
  attempt or build strike is charged, and the work returns — a build's specs to
  `approved`, and a scout's task to **`backlog`**, the one exception to
  "picked-up work stays picked up", because leaving it `queued` has the
  dispatch loop start a replacement scout within the tick. A cancelled scout
  keeps its salvage, with the cancel's rationale stamped onto the notes'
  `reason`. The `biased` ordering in `bounded` is load-bearing: an outcome
  already in hand is never discarded for a cancel that arrived in the same
  poll, so cancelling a run that finishes in the same breath is honest rather
  than destructive.

## Project structure

- `crates/tasks/` — the server binary: SQLite store, event log, GitHub
  polling (read-only intake), scout dispatcher, HTTP API + SSE
- `crates/tasks-api/` — wire types for the HTTP API (models, events,
  request/response bodies), shared by the server and native clients.
  Dependency-light (serde/chrono/uuid) on purpose; enums are strict —
  clients ship from this repo, so skew is a build error, not a runtime
  fallback
- `crates/tasks-protocol/` — ScoutCommand/ScoutEvent, the `AppProtocol` impl
  shared between server and Scout VMs
- `crates/build-stamp/` — `build.rs` helper that stamps a build identity
  (`0.1.<commit count>` + short SHA, env-overridable) into a binary. Used by
  the server, `tasks-client` and `app-gpui`; one implementation on purpose,
  since `GET /version` compares those numbers across processes
- `crates/scout-supervisor/` — PID 1 inside Scout VMs: clone, branch, run the
  agent, report the spec back
- `crates/vm-pool/` — vendored VM infrastructure (protocol, pool, service,
  client, supervisor). Has its own CLAUDE.md and TODO.md; conventions there
  apply within it (notably: no mocks, real processes in tests)
- `images/` — container image definitions. `base`, `agent` and `automation`
  are vm-pool's own (it stays independently publishable); `scout` and
  `builder` are Tasks'. A tool a Tasks crate needs goes in the latter two,
  duplicated, rather than once in `agent` — app vocabulary does not enter
  vm-pool's images any more than it enters its code
- `docs/plans/` — implementation plans; `docs/vm-pool.md` — vm-pool spec
- `spec` for the platform: issue #744 + docs/plans/2026-08-09-v2-resume.md

## Conventions

- Tests use real processes and real SQLite (in-memory or tempfile). No mocks.
  HTTP tests bind real servers on `127.0.0.1:0`.
- **Tests exec binaries; they never build them.** A `cargo build` inside a
  test blocks on the build-directory lock, so a background `cargo check`
  (rust-analyzer, another terminal) stalls the whole suite. For a binary in
  the test's own package use `env!("CARGO_BIN_EXE_<name>")`; for one from
  another package use `common::workspace_bin(name)` in `crates/tasks/tests`,
  which reads `TASKS_TEST_BIN_DIR` (exported by `make test`) and only builds
  as a memoized fallback. vm-pool has its own copy of this —
  `vm_pool_test_support::supervisor_binary()`, reading `VM_POOL_TEST_BIN_DIR`
  — deliberately, so vendored infrastructure stays independently testable;
  don't merge the two.
- **A new migration is named for a UTC instant, never for the next free
  number.** `make migration NAME=build_transcripts` writes
  `crates/tasks/migrations/20260815030411_build_transcripts.sql`
  (`YYYYMMDDHHMMSS`, UTC, **digits only**) — don't hand-roll one by copying
  the file next to it and adding one to the number. That number is read off a
  tree that cannot see its sibling branches, so two of them pick `0024`, and
  the collision exists only after the merge, where it surfaces as a boot
  failure in a process that has already taken the port. Three facts make the
  switch additive: 0001–0023 keep their versions and checksums (sqlx records
  both, so an applied migration can never be renamed), a 14-digit stamp sorts
  after any four-digit sequence number, and sqlx parses the text before the
  first `_` as an `i64` — so `20260815T030411_…` is a hard compile error, and
  a name it cannot split at all is silently skipped and simply never runs.
  `crates/tasks/src/migrations.rs` owns `MIGRATOR`, documents the rule, and
  holds the guard tests that make a violation red in your branch.
- Errors: `thiserror` enums per module. Logging: `tracing`.
- Rust edition 2024, `cargo fmt` + `cargo clippy --workspace --all-targets`
  clean before committing.

## Running

```sh
make serve                             # build, take over, log to this terminal
make restart                           # build, take over, background it
make restart RELOAD=--when-idle        # ...but wait out in-flight scouts first
make status / make stop
make stop STOP=--when-idle             # ...but wait out in-flight scouts first
cargo run -p tasks -- add-project owner/repo
make migration NAME=lower_snake_case   # new migration, stamped with the UTC now
make test                              # see Tests below
```

`serve` runs the Diamond 1 loop (`crates/tasks/src/run.rs`): GitHub intake,
scout dispatch bounded by `SCOUT_MAX_CONCURRENT`, and the HTTP API. Mode gates
*new* work only — `Pause`/`Stop` never interrupt a scout already in flight.
Both dependencies degrade rather than crash: no `GITHUB_TOKEN` disables
polling, an unreachable vm-pool disables dispatch and reconnects periodically,
and the API stays up either way.

**A boot does not resume the mode; it takes `TASKS_DEFAULT_MODE` (default
`pause`) and overwrites the stored one** (`apply_startup_mode`, before the
listener binds). Starting a server is therefore never the same act as resuming
dispatch: a crash, a `launchd` `KeepAlive` or an infrastructure problem brings
the pipeline back quiet, and `pause` rather than `stop` keeps intake and the
API alive while it is. The one exception is a deliberate upgrade — see below.
If you want a host to come back dispatching, the honest way to say so is
`TASKS_DEFAULT_MODE=play` on that host, not re-reading the column. The column
itself stays: it is still the live mode for the rest of the process's life
(`GET /mode`, `POST /mode`, and the three loops that read it every tick), it
has only stopped being consulted at boot — **do not delete it** without first
moving mode into process memory. Two details are load-bearing and cheap to
"clean up" by accident: the boot breadcrumb is a `Note` and not `ModeChanged`,
because `ModeChanged` is nudge-worthy and would spend an orchestrator turn on
every restart; and the transition happens *before* `server::bind`, so no
client — and no `reload` verifying a swap — can observe the previous run's
mode.

### Upgrading a running server

`tasks reload` (alias `restart`, `crates/tasks/src/reload.rs`) is the upgrade
loop the make targets drive: **build, report, gate, drain, swap, verify**, in
that order. A failed build costs nothing because nothing has been signalled
yet; "did it come up?" and "did the schema move?" are answered by `GET /status`
on the *new* pid rather than assumed. It refuses by default when a scout or a
build is in flight (`--when-idle` waits for a drain point and pauses dispatch
for the wait, `--force` swaps anyway); an owed orchestrator turn is reported
but never blocks, since the obligation loop keeps producing input and the
answered watermark means a restart mid-turn only costs one turn. Exit codes: 3
busy, 4 drain timed out, 5 the swap did not land.

**An upgrade is the one path that carries the mode over, and it carries it in
the child's environment.** `ModeHandover` snapshots the running server's mode
*before* the drain (the pause `--when-idle` installs is the tool, not the
intent), passes it to the new process as `TASKS_DEFAULT_MODE` — for the spawned
and the `--foreground`-exec'd server alike — and then verifies against the new
pid's `/status` that it came up in it, reporting the `curl` that fixes it if
not. It is not a `POST /mode` after the boot, for three independent reasons: a
POST leaves a window in which the new server runs in its configured default
(with `TASKS_DEFAULT_MODE=play` against a paused old server, that window
*dispatches*), `--foreground` execs so there is no "later" to restore anything
in, and the real environment outranks every `.env`. A cold start, and a
`--force` swap of a server too wedged to answer `/status`, carry nothing —
unknown resolves to quiet, never to dispatching. `reload` resolves
`TASKS_DEFAULT_MODE` as step **0**, before the build: an unusable value is a
hard `serve` startup error, and discovering that after the SIGTERM turns a typo
into an outage.

**`tasks stop --when-idle` waits on the same predicate, and differs in exactly
one lasting way.** Idle is `InFlight::is_destructible()` and there is one of it,
so Restart When Idle and Stop When Idle cannot disagree about what they are
waiting for; `--drain-timeout SECS` and exit codes 3 and 4 mean what they mean
in `reload` (5 has no meaning for a stop and is never returned). What differs is
the mode: **a stop leaves dispatch paused**, and says so in the help, in the
drain output, on the way out and in the app. Not taste — the only slot in which
a stop could write the mode back is *before* the SIGTERM, and unpausing a server
that is still running hands the dispatcher a window to launch one last scout,
which is precisely the unattended VM the flag exists to prevent (nothing here
may open the store to do it afterwards, for the reason below, and after the
SIGTERM there is no server to `POST /mode`). The drain *timeout* still restores
the mode, because nothing was stopped and a no-op must not have side effects; an
idle server is never paused at all, for the same reason. `ModeAfterDrain` is the
single place that asymmetry is written down, so a third caller of `drain` has to
answer the question rather than inherit an answer. Plain `tasks stop` is
unchanged — immediate and ungated, because it is the counterpart of
`reload --force`, the thing `make stop` and the reload path already rely on, and
the documented way through both new refusals. The GUI is where the missing
question lives: an immediate **Stop** with work in flight raises a three-way
prompt (wait / stop anyway / cancel) off the Server window's existing `/status`
poll — up to 5s stale, so it is a courtesy and never a lock.

Nothing in `reload` opens the store — `Store::open` runs migrations, so a
supervisor that opened the database would apply the new schema before the new
binary booted, masking the failure it exists to catch. `<data dir>/tasks.pid`
is a discovery record, not a lock: liveness is re-derived from the OS
(`ps`, where a `Z` state is dead), so a killed server leaves nothing to clean
up by hand. This is not a service manager — no supervision, no
restart-on-crash; point `launchd`/`systemd` at `tasks serve` if you want one.

### Restarts and work in flight

**A restart does not cost the work in flight.** Scouts and builds run under
their own supervisors inside VMs that vm-pool (a separate daemon) keeps alive,
so the only thing a restart loses is the event stream. Boot is `resume_in_flight`
— attach to every still-`running` session/build that names a live VM
(`ServiceCommand::Attach`, bounded replay, see `crates/tasks/src/reattach.rs`)
— and only then `reconcile_startup`, which writes off what is genuinely gone.
A reattach *always concludes its row*, including when it cannot resume;
reconciliation skips rows it owns, so one that returned leaving a row `running`
would strand it. The orchestrator's turn is a local child and cannot be
reattached: shutdown waits it out instead, and an interrupted one is reported
in the feed at the next boot. Shutdown holds the HTTP port through the whole
drain (so a restart is a hand-over, not an outage) and releases it last, which
means a successor waits for this process to exit before it can bind.

**The drain is bounded at every stage, and it names whatever it walked away
from.** `poll`, `nudge` and `obligations` used to be awaited *unbounded* and
unnamed, while scouts/builds and the orchestrator turn already had
`SHUTDOWN_GRACE` leashes. That asymmetry is what turned a one-line bug (#883:
`orchestrator_nudge_loop` never observing the shutdown flag, because
`watch::Receiver::changed()` marks a value seen when it *returns*, so a
shutdown consumed by an inner `select!` parks the outer loop forever) into a
75s SIGKILL with nothing in `serve.log` — and cost a scout run to diagnose.
`run::drain_background` now awaits those three under one shared
`BACKGROUND_GRACE` (10s) deadline and returns the names that did not finish,
one `warn!` each. Shared, not per-task, so the whole drain is **10 + 30 + 30 =
70s and still fits inside the 75s** `reload` allows before it SIGKILLs; and
loops after the deadline are still asked, so the log names the loop that is
stuck rather than whichever handle happened to be awaited first. Abandoning
these three is safe *by construction*, which is why they and not scouts get the
short leash: a poll is idempotent, obligations are recomputed from state every
pass, and a nudge is a latency optimization the answered watermark makes good.
The `warn!` wording is deliberate — these loops return on a flag, so there is
no legitimate reason for one to still be running at 10s, and a reader who takes
the message for ordinary shutdown noise is the failure mode that put #883 in
front of a scout.

**vm-pool is upgraded separately, and it goes first.** It is a long-lived
daemon that a server restart does not restart, so a freshly built server
routinely talks to the binary vm-pool was started with. `serde(default)` makes
an added *field* survive that skew (`seq` on `vm_app`, `protocol_version` on
`pool_status`); it cannot make an added *command* survive it, because an old
service rejects the whole line at decode time and the client sees an ordinary
`ClientError::Service` — indistinguishable, without matching serde's message
text, from a real failure to attach to a VM that does exist. So `attach` is
gated: `reattach::attach_support` asks `status` once per boot and reads the
`protocol_version` it reports (absent ⇒ `PRE_VERSIONING`), and
`resume_in_flight` returns empty **before claiming any row** if the answer is
too old, unanswerable, or unreachable — `ResumedWork` membership is a promise
to conclude the row, so a claim made against a pool that cannot decode `attach`
would fail the run rather than lose the stream. The remaining cost is the
one-time restart itself: whatever vm-pool is holding is lost once (the event
log is in memory), and the leaked VMs are collected by the sweep on the next
connect. `dispatch_loop` logs the skew on every connect, because the bill
otherwise only arrives at the next restart, by which point the work it costs is
already in flight.

### Tests

```sh
make test        # prebuild + cargo-nextest (default profile) + doctests
make test-ci     # same, --profile ci: no fail-fast, retries, quieter slow threshold
make test-cargo  # plain `cargo test --workspace`, no prerequisites
```

`make test` needs `cargo install cargo-nextest --locked`; `make test-cargo` is
the fallback if you don't have it, and is also what keeps the build-on-demand
path in `workspace_bin` honest. Both nextest targets prebuild the supervisor
binaries and export `TASKS_TEST_BIN_DIR` so no test shells out to cargo.

Two gotchas worth knowing. **nextest does not run doctests** — silently, with
no skip count in its summary — so both targets end with `cargo test --doc
--workspace`; anything else that runs the suite must too. And the scout
timeout tests (three of them) leave a stray child holding the output pipe, so
they report as LEAK; that is expected (`leak-timeout` is `result = "pass"`), and the profile
deliberately keeps the period short rather than waiting the leak out, which
would cost seconds and hide a real leak. Tuning lives in
`.config/nextest.toml`.

**`app-gpui` is not a workspace member, so none of the above touches it — and
it *can* be compiled and tested from a Linux agent VM**, which was long
assumed otherwise:

```sh
make app-check   # cargo check --all-targets, ~1 minute cold
make app-test    # the app's own unit tests
cd app-gpui && cargo test   # the same thing, when the deps below are present
```

Neither needs a display or a Mac. The build wants five packages —
`pkg-config libfontconfig-dev libxkbcommon-dev libxkbcommon-x11-dev
libxcb1-dev` — which `images/{scout,builder}/Dockerfile` install, so in a VM
off a current image all three commands above are plain cargo. Where they are
absent the make targets fall back to what they always did:
`RUST_FONTCONFIG_DLOPEN=1` makes `yeslogic-fontconfig-sys` skip its
`pkg_config` probe, and linking the *test* binary is satisfied by three empty
stub `.so`s (`-lxcb`, `-lxkbcommon`, `-lxkbcommon-x11`) that `app-stubs`
generates — the tests are pure functions over view state and never enter the
platform layer. The Makefile picks between the two with one `pkg-config
--exists`, because the fallback must not stay the default once the packages
exist: `-L` beats the system paths, so the empty stubs would shadow the real
libraries, and `RUSTFLAGS` is part of cargo's fingerprint, so a stubbed
`make app-test` and a hand-run `cargo test` would each rebuild the whole gpui
tree over the other.

The boundary is compile and test, yes; **run, no**. A green `make app-test`
says the code compiles and its logic holds, not that a pixel landed anywhere —
the title bar next to the traffic lights, icon-only rows at 240px and whether
a menu item is actually greyed are still `make app` on a Mac. But "the GUI
can't be compiled here" was costing every app-gpui change its feedback loop,
and it was not true.

Data dir: `~/.local/state/tasks-v2/` (override: `TASKS_DATA_DIR`).

**Config is read from `.env`, not just from the environment**
(`crates/tasks/src/env_file.rs`). Three files are tried, in this order, and
the first to define a variable wins — with the real environment outranking all
of them, so `GITHUB_TOKEN=… tasks serve` still overrides:

1. `<data dir>/.env` — launcher-independent, and the only one an installed
   binary outside a checkout can have
2. the nearest `.env` at or above the **cwd** — a developer's `make serve`
3. the nearest `.env` at or above the **executable** — the same repo file,
   found when the cwd is `/` because launchd started the app

The third one is not redundant. Configuration used to come from the process
environment alone, which meant it only ever applied to a server started from a
shell that had exported it: restarting from the app's Server menu — whose
ancestor is launchd — silently reverted `GITHUB_TOKEN`, `ORCHESTRATOR_CMD` and
`ORCHESTRATOR_WORKDIR` to their defaults, and the server came up healthy and
wrong. Loading happens once in `main`, before subcommand dispatch (so `serve`,
`reload` and `status` cannot disagree about `TASKS_DATA_DIR`) and before the
tokio runtime exists (`set_var` is unsafe once threads are running). It is
never done inside `Config::from_env` — tests build configs, and a suite that
read the developer's untracked `.env` would be the worse bug.

**`Command::env_remove` is the opposite of a scrub**, and a test that execs the
`tasks` binary has to know it. The real environment is the only thing a `.env`
entry loses to, so *removing* a variable from a child's environment is exactly
what promotes the file that defines it — and `.env` is gitignored, so a
maintainer with `TASKS_DEFAULT_MODE=play` in one fails a restart suite on their
machine and nowhere else. `TASKS_ENV_FILES=off` is the switch for that:
`crates/tasks/tests/reload.rs` is the only file that execs the binary (so it is
the whole blast radius), and any future one needs the same three settings. The
test that pins it carries a **control** — it first boots with the switch removed
and asserts the `.env` really does decide the mode, then boots with it and
asserts it does not. Without that half the assertion is vacuous. A value that is
neither `on` nor `off` (including one that is not UTF-8) refuses to start rather
than being ignored: `.ok()` there would mean "load the files anyway", the one
direction this switch must not fail in.

The matching rule for the orchestrator: **anything the system prompt claims
about the environment is generated from it**, alongside the charter-generated
authority section. `workdir_is_checkout` and `github_configured` decide whether
the prompt offers a checkout it may edit and whether it warns that GitHub
writes will fail. A hardcoded "your working directory is the project checkout"
is what sent a curl-only agent reaching for `python3` and `Write`.

| var | default | |
| --- | --- | --- |

| var | default | |
| --- | --- | --- |
| `TASKS_SERVER_PORT` | 4800 | HTTP API port (also `--port`) |
| `TASKS_POLL_INTERVAL` | 60 | seconds between GitHub polls |
| `TASKS_DEFAULT_MODE` | `pause` | the mode **every** boot starts in, overwriting whatever the last process left in the store — `play`, `pause` or `stop`, and an unparseable value refuses to boot rather than being ignored. Only `tasks reload` overrides it, by passing the old server's mode to the new one |
| `TASKS_ENV_FILES` | `on` | `off` skips `.env` loading entirely — for tests that exec the `tasks` binary, where `env_remove` promotes a `.env` rather than scrubbing it. Anything that is neither `on` nor `off` refuses to boot |
| `TASKS_INTAKE_LABEL` | — | when set (e.g. `tasks`), only open issues carrying that label are ingested; matched case-insensitively. Applied after the fetch, so closure tracking still sees the complete open set. Un-labelling an issue keeps its existing task, it just stops refreshing it |
| `SCOUT_MAX_CONCURRENT` | 2 | scouts running at once |
| `SCOUT_IMAGE` | `agent:v1` | vm-pool image scouts run in |
| `SCOUT_TIMEOUT_SECS` | 3600 | wall-clock budget per scout; past it the VM is deallocated and the attempt counts as a dispatch failure. Keep below vm-pool's `vm_timeout` (7200) |
| `SCOUT_CHECKPOINT_INTERVAL_SECS` | 30 | how often a Scout's `NOTES.md` is streamed back as a checkpoint. Read *inside* the VM, so it is set in `images/scout/Dockerfile`, not here |
| `SCOUT_MAX_RESUMES` / `BUILDER_MAX_RESUMES` | 2 | times a supervisor re-invokes an agent with `--resume <session_id>` after its API connection dropped mid-response (#845). Only a transport death is retried, and the backoff rises 2s / 15s / 30s. `0` disables it. Read *inside* the VM, so both live in `images/{scout,builder}/Dockerfile` |
| `SCOUT_VM_CPUS` / `SCOUT_VM_MEMORY_MB` | 4 / 6144 | shape of a Scout VM. Multiplied by `SCOUT_MAX_CONCURRENT` on the host — lower one of the three on a small machine |
| `BUILDER_VM_CPUS` / `BUILDER_VM_MEMORY_MB` | 4 / 8192 | shape of a Builder VM. Larger than a Scout's because builds are serial (nothing multiplies it) and a killed Builder costs a whole implementation |
| `SCOUT_BUILD_JOBS` / `BUILDER_BUILD_JOBS` | derived | `CARGO_BUILD_JOBS` injected per-VM. Derived from the VM's memory — `(memory_mb − 2048) / 2048`, clamped to `[1, cpus]` — because cargo defaults `-j` to the CPU count and knows nothing about the memory limit, which is how 4 CPU / 4 GB VMs got a linker OOM-killed. Set either to override the derivation |
| `VM_POOL_SOCKET` | `/tmp/vm-pool.sock` | vm-pool service socket |
| `GITHUB_TOKEN` | — | required for polling; also used for clones |
| `GITHUB_API_URL` | api.github.com | GraphQL endpoint override |
| `GITHUB_CLONE_URL_BASE` | `https://github.com` | clone URL prefix |
| `ORCHESTRATOR_CMD` | `claude --print … --allowedTools Bash(curl:*)` | orchestrator agent command; its permission flags decide what the orchestrator may do |
| `ORCHESTRATOR_WORKDIR` | `<data dir>/orchestrator` | orchestrator cwd; point at the repo checkout (with `--dangerously-skip-permissions` in the cmd) to run it as a full dev agent |
| `ORCHESTRATOR_TIMEOUT_SECS` | 600 | wall-clock budget per orchestrator tick |
| `BRIEFING_CMD` | `claude --print --allowedTools "Bash(gh:*),…"` | one-shot agent command for Home briefings; must stay read-only (gh/curl/git log/git diff — never `--dangerously-skip-permissions`). Shell-style quoting supported |
| `BRIEFING_TTL_SECS` | 900 | Home briefing freshness window (stale-while-revalidate on `GET /briefings`) |
| `BRIEFING_TIMEOUT_SECS` | 300 | wall-clock budget per briefing generation |
