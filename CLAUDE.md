# Tasks

A human-in-the-loop platform that orchestrates coding agents to get project work done.

## Project structure

- `spec/` — Specification documents
  - `spec.md` — Main platform spec
  - `session-runtime.md` — Session runtime architecture (container provider, supervisor, protocol)
  - `github.md` — GitHub integration: normalized model, GraphQL queries, polling
  - `example.md` — Worked example of the system in action
- `crates/` — Rust crates (host-side server)
  - `events/` — Event system: append-only log, pub/sub
  - `github/` — GitHub integration: GraphQL client, normalized model, polling
  - `runtime/` — Session runtime: container lifecycle, protocol, transport
  - `server/` — Server: domain models, operating modes, merge queue, presence
- `src/` — TypeScript (supervisor that runs inside containers)

## Key decisions

- Host runtime: Rust
- Container supervisor: Bun/TypeScript (PID 1 inside containers)
- Session isolation: apple/container (one lightweight Linux VM per session)
- Host ↔ container communication: JSON-line protocol over stdio
- Agent provider: Claude Code (initial), pluggable
