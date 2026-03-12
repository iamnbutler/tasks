# Tasks

A human-in-the-loop platform that orchestrates coding agents to get project work done.

## Project structure

- `spec/` — Specification documents
  - `spec.md` — Main platform spec
  - `session-runtime.md` — Session runtime architecture (container provider, supervisor, protocol)
  - `example.md` — Worked example of the system in action
- `src/` — Source code (TypeScript, Bun runtime)

## Key decisions

- Runtime: Bun
- Session isolation: apple/container (one lightweight Linux VM per session)
- Host ↔ container communication: Supervisor process (Bun/TS) as PID 1, JSON-line protocol over stdio
- Agent provider: Claude Code (initial), pluggable
