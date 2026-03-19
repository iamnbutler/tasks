# Tasks

A human-in-the-loop platform that orchestrates coding agents to get project work done.

## Project structure

- `spec/` — Specification documents
  - `spec.md` — Main platform spec
  - `session-runtime.md` — Session runtime architecture (container provider, supervisor, protocol)
  - `github.md` — GitHub integration: normalized model, GraphQL queries, polling
  - `symphony-legacy.md` — Historical Symphony spec (predecessor project)
- `crates/` — Rust crates (host-side server)
  - `app/` — Binary entry point: startup, run loops, component wiring
  - `events/` — Event system: append-only log, pub/sub
  - `github/` — GitHub integration: GraphQL client, normalized model, polling
  - `agent/` — LLM abstraction layer: Provider trait, Anthropic client, session, chain builder
  - `models/` — Shared domain types: project, task, merge entry, task state
  - `orchestrator/` — Orchestrator: AI project foreman, quality evaluation, merge authority
  - `runtime/` — Session runtime: container lifecycle, protocol, transport
  - `server/` — Server: domain models, operating modes, merge queue, presence
  - `session/` — Session management: lifecycle, monitoring, event bridging
  - `store/` — Persistent storage: SQLite for projects, tasks, merge queue
  - `supervisor/` — Container supervisor binary (PID 1 inside containers)
  - `desktop/` — GPUI-based native desktop app (macOS/Linux)
  - `gpui-client/` — HTTP API client library for GPUI frontend
- `web/` — React + Vite frontend (shadcn/ui + Tailwind CSS v4 + TanStack Table)

## Key decisions

- Host runtime: Rust
- Container supervisor: Rust (PID 1 inside containers, built from same workspace)
- Session isolation: apple/container (one lightweight Linux VM per session)
- Host ↔ container communication: JSON-line protocol over stdio
- Agent provider: Claude Code (initial), pluggable
- Container images: built with `container build` (apple/container CLI), NOT Docker
- Data directory: `~/.local/state/tasks/` (SQLite + event logs), configurable via `TASKS_DATA_DIR`
- Config: `.env` file at project root, loaded automatically via dotenvy

## Working on issues

- When making design decisions during brainstorming or implementation, leave comments on the relevant GitHub issue with the decision and reasoning. Don't edit the issue body — comments create a timeline and make active issues more glanceable in the list.

## Building the container image

```sh
make container-image   # cross-compile supervisor + build image
```

The supervisor binary is cross-compiled on the host for `aarch64-unknown-linux-gnu` and copied into the container image. This is faster than building inside Docker.

**Prerequisites** (run `make check-linker` to verify):
- Cross-linker: `brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu` (macOS) or `apt install gcc-aarch64-linux-gnu` (Linux)
- Rust target: `rustup target add aarch64-unknown-linux-gnu`

**Individual targets:**
- `make supervisor` — build the supervisor binary only
- `make container-image` — build supervisor + container image
- `make check-linker` — verify cross-compilation toolchain is installed

## Running

```sh
cargo run -- add-project owner/repo   # add a project (stored in SQLite)
cargo run -- run                      # headless mode
cargo run -- run --web                # web UI (serves on port 4800)
```

Requires `.env` with `GITHUB_TOKEN` and `ANTHROPIC_API_KEY`.

## Web frontend

- `web/` — React + Vite SPA with shadcn/ui + Tailwind CSS v4 + TanStack Table
- Built output goes to `web/build/`, served by the Rust server at `/`
- API endpoints at `/api/*`, SSE event stream at `/api/events`
- Uses bun as package manager

```sh
bun install && bun web build   # build frontend
bun web dev                    # dev mode (proxies /api to localhost:4800)
```

### API endpoints

- `GET /api/snapshot` — Full system state (spec Section 16.3)
- `GET /api/tasks` — List all tasks
- `GET /api/tasks/:id` — Get single task
- `GET /api/tasks/:id/events` — Task event history
- `GET /api/projects` — List projects
- `POST /api/projects` — Add project `{ repo: "owner/repo" }`
- `DELETE /api/projects/:id` — Remove a project
- `GET /api/merge-queue` — Merge queue entries
- `GET /api/mode` — Current operating mode
- `POST /api/mode` — Set operating mode `{ mode: "play"|"pause"|"stop" }`
- `POST /api/merge-queue/:id/approve` — Approve merge entry
- `POST /api/merge-queue/:id/reject` — Reject merge entry
- `POST /api/merge-queue/flush` — Flush approved entries (Pause mode only)
- `POST /api/tasks/:id/chat` — Send chat message to agent session `{ message: string }`
- `GET /api/events` — SSE live event stream (optional `?pattern=&task_id=` filters)

## Desktop app (GPUI)

A native desktop application built with [GPUI](https://github.com/zed-industries/zed) (Zed's GPU-accelerated UI framework).

- `crates/desktop/` — Main desktop app crate
- `crates/gpui-client/` — Shared HTTP API client

### Building

The desktop app requires system libraries for X11/Wayland:

**macOS:**
```sh
# No additional dependencies needed
cargo build --package tasks-desktop
```

**Linux (Debian/Ubuntu):**
```sh
sudo apt install libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev
cargo build --package tasks-desktop
```

### Running

```sh
# Start the server first
cargo run -- run --web

# In another terminal, run the desktop app
cargo run --package tasks-desktop
```

### Configuration

- `TASKS_SERVER_URL` — Server URL (default: `http://localhost:4800`)

### Architecture

- **api.rs** — HTTP API client (mirrors web frontend's `api.ts`)
- **sse.rs** — SSE client with auto-reconnection
- **state.rs** — Reactive app state management (GPUI Model-based)
- **theme.rs** — Theming system matching web frontend's Tailwind colors
- **components/** — UI primitives (Badge, Button, Card, Input)
- **views/** — View components (Dashboard, etc.)
