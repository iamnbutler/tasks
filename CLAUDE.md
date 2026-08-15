# Tasks (v2)

A human-in-the-loop platform that orchestrates coding agents (headless Claude
Code) to get project work done, built around the Double Diamond architecture
(issue #744): parallel Scout exploration → spec queue → serial Builder
implementation.

## Load-bearing design rules

- **The Scout/Builder information barrier is inviolable.** Builders never see
  Scout code — the spec is the deliverable. Specs are text, so a Builder run
  can batch N specs into one branch. Never propose reusing Scout branches.
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
--workspace`; anything else that runs the suite must too. And two scout
timeout tests leave a stray child holding the output pipe, so they report as
LEAK; that is expected (`leak-timeout` is `result = "pass"`), and the profile
deliberately keeps the period short rather than waiting the leak out, which
would cost seconds and hide a real leak. Tuning lives in
`.config/nextest.toml`.

Data dir: `~/.local/state/tasks-v2/` (override: `TASKS_DATA_DIR`). Config via
env / `.env`:

| var | default | |
| --- | --- | --- |
| `TASKS_SERVER_PORT` | 4800 | HTTP API port (also `--port`) |
| `TASKS_POLL_INTERVAL` | 60 | seconds between GitHub polls |
| `TASKS_INTAKE_LABEL` | — | when set (e.g. `tasks`), only open issues carrying that label are ingested; matched case-insensitively. Applied after the fetch, so closure tracking still sees the complete open set. Un-labelling an issue keeps its existing task, it just stops refreshing it |
| `SCOUT_MAX_CONCURRENT` | 2 | scouts running at once |
| `SCOUT_IMAGE` | `agent:v1` | vm-pool image scouts run in |
| `SCOUT_TIMEOUT_SECS` | 3600 | wall-clock budget per scout; past it the VM is deallocated and the attempt counts as a dispatch failure. Keep below vm-pool's `vm_timeout` (7200) |
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
