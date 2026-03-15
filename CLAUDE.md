# Tasks

A human-in-the-loop platform that orchestrates coding agents to get project work done.

## Project structure

- `spec/` — Specification documents
  - `spec.md` — Main platform spec
  - `session-runtime.md` — Session runtime architecture (container provider, supervisor, protocol)
  - `github.md` — GitHub integration: normalized model, GraphQL queries, polling
  - `example.md` — Worked example of the system in action
- `crates/` — Rust crates (host-side server)
  - `app/` — Binary entry point: startup, run loops, component wiring
  - `events/` — Event system: append-only log, pub/sub
  - `github/` — GitHub integration: GraphQL client, normalized model, polling
  - `models/` — Shared domain types: project, task, merge entry, task state
  - `runtime/` — Session runtime: container lifecycle, protocol, transport
  - `server/` — Server: domain models, operating modes, merge queue, presence
  - `session/` — Session management: lifecycle, monitoring, event bridging
  - `store/` — Persistent storage: SQLite for projects, tasks, merge queue
  - `supervisor/` — Container supervisor binary (PID 1 inside containers)

## Key decisions

- Host runtime: Rust
- Container supervisor: Rust (PID 1 inside containers, built from same workspace)
- Session isolation: apple/container (one lightweight Linux VM per session)
- Host ↔ container communication: JSON-line protocol over stdio
- Agent provider: Claude Code (initial), pluggable
- Container images: built with `container build` (apple/container CLI), NOT Docker
- Data directory: `~/.tasks/` (SQLite + event logs), configurable via `TASKS_DATA_DIR`
- Config: `.env` file at project root, loaded automatically via dotenvy

## Building the container image

```sh
container build --dns 8.8.8.8 -f src/runtime/Dockerfile -t tasks-agent:latest .
```

The Dockerfile uses a multi-stage build: Rust compilation in `rust:1.85-slim`, runtime on `ubuntu:24.04`. The supervisor binary (`crates/supervisor/`) is compiled in the builder stage and copied into the final image.

## Running

```sh
cargo run -- add-project owner/repo   # add a project (stored in SQLite)
cargo run -- run                      # headless mode
cargo run -- run --tui                # terminal UI
```

Requires `.env` with `GITHUB_TOKEN` and `ANTHROPIC_API_KEY`.
