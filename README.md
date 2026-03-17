# Tasks

> **WIP** — This project is under active development. Expect breaking changes.

A human-in-the-loop platform that orchestrates coding agents to get project work done. It polls GitHub for issues, dispatches isolated agent sessions in lightweight Linux VMs, and routes results through a quality-gated merge queue.

## Prerequisites

- **Rust 1.85+** (`rustup update`)
- **Bun** ([bun.sh](https://bun.sh)) — for the web frontend
- **apple/container** — for session isolation (macOS only, `brew install container`)
- A **GitHub token** with repo access
- An **Anthropic API key**

## Setup

```sh
# Clone
git clone https://github.com/iamnbutler/tasks.git
cd tasks

# Configure environment
cp .env.example .env
# Edit .env — set GITHUB_TOKEN and ANTHROPIC_API_KEY

# Build the container image (required for agent sessions)
container build --dns 8.8.8.8 -f src/runtime/Dockerfile -t tasks-agent:latest .

# Build the web frontend
bun install && bun web build
```

## Running

```sh
# Add a project to track
cargo run -- add-project owner/repo

# Run with web UI (serves on :4800)
cargo run -- run --web

# Other modes
cargo run -- run          # headless
cargo run -- run --tui    # terminal UI
```

## Development

```sh
# Frontend dev server (proxies /api to localhost:4800)
bun web dev

# Run the backend separately
cargo run -- run --web
```

## Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `GITHUB_TOKEN` | Yes | — | GitHub personal access token |
| `ANTHROPIC_API_KEY` | Yes | — | Anthropic API key (for agent sessions) |
| `TASKS_MAX_SESSIONS` | No | `5` | Max concurrent agent sessions |
| `TASKS_POLL_INTERVAL` | No | `60` | GitHub polling interval (seconds) |
| `TASKS_DISPATCH_INTERVAL` | No | `30` | Dispatch loop interval (seconds) |
| `TASKS_CONTAINER_IMAGE` | No | `tasks-agent:latest` | Container image for sessions |
| `TASKS_DATA_DIR` | No | `~/.tasks` | Data directory (SQLite + event logs) |

## License

MIT
