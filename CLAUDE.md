# Tasks (v2)

A human-in-the-loop platform that orchestrates coding agents (headless Claude
Code) to get project work done, built around the Double Diamond architecture
(issue #744): parallel Scout exploration → spec queue → serial Builder
implementation.

**This file holds the rules that hold everywhere in this repo, and nothing
else. It is not a decision log.** Why a particular module works the way it
does is documented *on that module*, and the reason is mechanical rather than
stylistic: a doc comment travels with the code it describes, is read by whoever
edits it, and dies when the code dies, while a paragraph in here goes stale
silently and is loaded into every agent turn forever. If a rule only bites
while editing one module, it does not belong in this file at any length — not
even as a pointer line.

## Rules

- **The Scout/Builder information barrier is inviolable.** Builders never see
  Scout code — the spec is the deliverable. Specs are text, so a Builder run
  can batch N specs into one branch. Never propose reusing Scout branches.
- **Salvage is never a spec.** A Scout writes two files with two meanings:
  `SPEC.md` means "I concluded", `NOTES.md` means "here is what I have so
  far". Promoting notes into a spec stays a human act, because a half-explored
  spec in the review queue looks finished.
- **Never persist a GitHub-owned fact** (PR mergeable/SHA/CI, issue
  open-closed, labels). Query at decision time. Persist only Tasks-owned
  state plus append-only decisions keyed to immutable SHAs. GitHub writes go
  through the server, never through agents.
- **`done` means shipped, and it is written in exactly one place.** A build
  that opens a PR has made a claim, not a delivery. The only thing that writes
  `done` is closure-derived retirement, so `done` always means "the issue is
  closed upstream".
- **Bulk intake never auto-dispatches, and queue membership is explicit.**
  Ingested issues land in `backlog` and are never dispatched; only explicitly
  queued tasks reach a Scout. Adding a repo with 11,000 issues must not turn
  into 11,000 Scout runs and 11,000 PRs nobody chose.
- **What the orchestrator may do lives in `orchestrator_charter`, never in a
  prompt.** The prompt's authority section is generated from those rows and the
  server enforces the same rows on the endpoints — one statement of authority,
  and not one a long conversation can talk itself out of. What makes autonomy
  safe here is the `decisions` ledger under every write: audit and recourse
  after the fact, never pre-approval. The human is never gated.
- **A write the server cannot attribute is recorded as the human's** — and the
  human is never gated, so a broken agent credential does not fail closed, it
  *escalates*. Anything that widens what an agent can reach has to be checked
  against that, and an `X-Tasks-Actor` that is present but does not verify is a
  403, never a demotion to human.
- **A refusal is a no-op, so everything refusable runs before the effect.** A
  4xx that has already filed the issue, closed the task or spent the credential
  is the failure this prevents; it is what makes a 4xx safe to retry.
- **No VM ever holds a raw `ANTHROPIC_API_KEY` or `GITHUB_TOKEN`.** A run gets
  a scoped, expiring lease redeemed against the in-process broker, which
  injects the real credential host-side. Anything injected into a VM's
  environment eventually reaches a log. Raw values cross module boundaries only
  as `redact::Secret`.
- **A strike is charged for a verdict, and for nothing else.** The attempt caps
  exist so work that genuinely cannot be done stops consuming the pipeline; a
  run that died of something unrelated to the work has learned nothing.
  Classify off a *field*, never off reason text — a reason is prose written for
  a human, and a decision that greps it changes meaning the next time somebody
  improves a sentence.
- **`directions` tell an agent what to do; `rationale` tells a human why. They
  are never copied into each other.** A rationale reaches no VM ever; put an
  instruction there and the agent never sees it.
- **Agent engine is Claude Code / the Agent SDK — never a home-rolled agentic
  loop.** The server consumes Claude Code's typed output (stream-json, hooks,
  MCP tools, structured outputs); it does not reimplement the loop.
- **Dependency direction:** `crates/vm-pool/*` are pure infrastructure and
  must never depend on tasks crates. App vocabulary enters vm-pool only
  through the `AppProtocol` generic (see `crates/tasks-protocol`). vm-pool
  stays independently publishable.
- **Merging is not deploying.** `make images` is the whole deployment step for
  anything inside a VM, it is run by hand on a Mac, and nothing in the pipeline
  can do it. A fix to a supervisor or to `images/` reaches nothing — not a
  test, not the pipeline — until someone runs it. Say so in the pull request.
- **A run in flight is not a reason to refuse host work.** Runs are disposable:
  a restart re-attaches, and what it cannot re-attach costs at most an
  `Orphaned` write-off, which charges no attempt. Never gate host maintenance
  on an in-flight scout or build.

## Where the reasoning lives

Every rule above, and every design decision behind the code, is written up
where it can be maintained:

- **Module doc comments** are the primary home. Modules here carry substantial
  `//!` headers, each opening with the failure that produced the design. The
  ones worth reading before changing anything nearby:
  - `crates/tasks/src/deadline.rs` — run budgets, and a host that sleeps
  - `crates/tasks/src/secrets.rs`, `crates/tasks/src/broker.rs` — credential
    custody, and why no VM holds a real key
  - `crates/tasks/src/loopback.rs` — why the API refuses browser-shaped
    requests
  - `crates/tasks/src/dispatch_gate.rs` — the one place "may I start work?" is
    answered, beside `crates/tasks/src/github_health.rs`,
    `crates/tasks/src/broker_health.rs`, `crates/tasks/src/pool_health.rs` and
    `crates/tasks/src/runtime_health.rs`
  - `crates/tasks/src/verify_dir.rs` — the warm build directory, and what
    bounds it
  - `crates/tasks/src/reload.rs`, `crates/tasks/src/reattach.rs` — upgrading a
    running server without losing the work in flight
  - `crates/tasks/src/orchestrator.rs` — the turn, its prompt sections and its
    generated authority
  - `crates/tasks/src/doctor.rs` — every precondition for a scout, in the order
    they bite
  - `crates/tasks-protocol/src/agent_run.rs` — how an agent process ended, and
    whether to resume it
- `docs/plans/` — implementation plans and the larger designs (credential
  custody, end-user distribution, signing and notarization, the release flow).
- `docs/operating.md` — running a server: environment variables, pool capacity,
  restart and drain semantics, running the tests.
- `crates/vm-pool/CLAUDE.md` — vm-pool's own conventions, which apply within
  it.
- Commit messages and the issues cited throughout, for the archaeology.

**When you make a design decision, record it in the doc comment on the code it
governs — not here.** This file grew 21× in seven days by collecting one
paragraph per decision, until it was 190k characters loaded into every agent
turn and past the point anything read it. `crates/tasks/tests/claude_md.rs`
now fails if it crosses its budget, because that failure was silent.

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
- `crates/builder-supervisor/` — PID 1 inside Builder VMs: the same shape, plus
  running the repository's own suite before anything is packaged
- `crates/vm-pool/` — vendored VM infrastructure (protocol, pool, service,
  client, supervisor). Has its own CLAUDE.md and TODO.md; conventions there
  apply within it (notably: no mocks, real processes in tests)
- `app-gpui/` — the Mac app and menubar app. **Not a workspace member**, so
  `make test` does not touch it; `make app-check` and `make app-test` do, and
  neither needs a display or a Mac
- `images/` — container image definitions. `base`, `agent` and `automation`
  are vm-pool's own (it stays independently publishable); `scout` and
  `builder` are Tasks'. A tool a Tasks crate needs goes in the latter two,
  duplicated, rather than once in `agent` — app vocabulary does not enter
  vm-pool's images any more than it enters its code
- `site/` — the landing page published at <https://nate.rip/tasks/>. No build
  step, and `make site-check` is its publish gate
- `docs/plans/` — implementation plans; `docs/vm-pool.md` — vm-pool spec
- `spec` for the platform: issue #744 + docs/plans/2026-08-09-v2-resume.md

## Conventions

- Tests use real processes and real SQLite (in-memory or tempfile). No mocks.
  HTTP tests bind real servers on `127.0.0.1:0`.
- **Tests exec binaries; they never build them.** A `cargo build` inside a
  test blocks on the build-directory lock, so a background `cargo check`
  (rust-analyzer, another terminal) stalls the whole suite. For a binary in
  the test's own package use `env!("CARGO_BIN_EXE_<name>")`; for one from
  another package use `common::workspace_bin(name)` in `crates/tasks/tests`.
  vm-pool has its own copy of this — deliberately, so vendored infrastructure
  stays independently testable; don't merge the two.
- **A new migration is named for a UTC instant, never for the next free
  number.** `make migration NAME=build_transcripts` writes
  `crates/tasks/migrations/20260815030411_build_transcripts.sql`
  (`YYYYMMDDHHMMSS`, UTC, **digits only**). A sequence number is read off a
  tree that cannot see its sibling branches, so two branches pick `0024`, the
  collision exists only after the merge, and it surfaces as a boot failure in a
  process that has already taken the port. `crates/tasks/src/migrations.rs`
  owns `MIGRATOR`, documents the rule, and holds the guard tests.
- **Write the failure before the code.** Doc comments and commit messages here
  open with the failure that produced the design, not with a description of the
  diff. Match that register — and keep the explanation next to the thing it
  explains.
- Errors: `thiserror` enums per module. Logging: `tracing`.
- Rust edition 2024, `cargo fmt` + `cargo clippy --workspace --all-targets`
  clean before committing.

## Running

`make serve`, `make restart`, `make test`, `tasks doctor`. The README covers
setup and day-to-day operation; `docs/operating.md` covers the environment
variables, pool capacity, and restart/drain semantics.
