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
- **Never persist a GitHub-owned fact** (PR mergeable/SHA/CI, issue
  open-closed, labels). Query at decision time. Persist only Tasks-owned
  state plus append-only decisions keyed to immutable SHAs. GitHub writes go
  through the server, never through agents.
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
  and builds are serial.
- **What the orchestrator may do lives in `orchestrator_charter`, never in a
  prompt.** Eight independently switchable capabilities (`capture_work`,
  `curate_work`, `comment_on_work`, `retire_work`, `queue_tasks`,
  `dispatch_builds`, `auto_review_specs`, `land_builds`), each
  `off` | `shadow` | `live`, human-writable only. The system prompt's
  authority section is *generated* from those rows every turn and the server
  enforces the same rows on the endpoints — one statement of authority, and
  not one a long conversation can talk itself out of. **All eight ship `live`
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
- `images/` — container image definitions (base, agent, automation)
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
it *can* be checked from a Linux agent VM**, which was long assumed otherwise:

```sh
make app-check   # cargo check --all-targets, ~1 minute cold
make app-test    # the app's own unit tests
```

Neither needs a display, X11 dev packages or a Mac. `RUST_FONTCONFIG_DLOPEN=1`
makes `yeslogic-fontconfig-sys` skip the `pkg_config` probe that is the only
thing blocking the check, and linking the *test* binary is satisfied by three
empty stub `.so`s (`-lxcb`, `-lxkbcommon`, `-lxkbcommon-x11`) that `app-stubs`
generates — the tests are pure functions over view state and never enter the
platform layer. What this cannot tell you is whether a pixel landed correctly;
that still needs `make app` on a Mac. But "the GUI can't be compiled here" was
costing every app-gpui change its feedback loop, and it was not true.

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
