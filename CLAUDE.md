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
- **Manual queue is human-authoritative, and queue membership is explicit.**
  `tasks.manual_rank` is set only via the API; the GitHub poller must never
  write it. Ingested issues land in `backlog` and are never dispatched — only
  explicitly queued tasks (`POST /tasks/{id}/queue` or `/scout`) reach a
  Scout, and picked-up work stays picked up (failures and `needs_revision`
  return to `queued`, not `backlog`).
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
- Errors: `thiserror` enums per module. Logging: `tracing`.
- Rust edition 2024, `cargo fmt` + `cargo clippy --workspace --all-targets`
  clean before committing.

## Running

```sh
cargo run -p tasks -- serve            # poller + scout dispatcher + HTTP API
cargo run -p tasks -- add-project owner/repo
cargo test --workspace
```

`serve` runs the Diamond 1 loop (`crates/tasks/src/run.rs`): GitHub intake,
scout dispatch bounded by `SCOUT_MAX_CONCURRENT`, and the HTTP API. Mode gates
*new* work only — `Pause`/`Stop` never interrupt a scout already in flight.
Both dependencies degrade rather than crash: no `GITHUB_TOKEN` disables
polling, an unreachable vm-pool disables dispatch and reconnects periodically,
and the API stays up either way.

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
