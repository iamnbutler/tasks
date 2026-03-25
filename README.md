# Tasks

> **WIP** — This project is under active development. Expect breaking changes.

🚨 Frequent breaking changes happening multiple times a day. This app has zero security or saftey guardrails, and shouldn't be used by anyone! You have been warned 🙂 🚨

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

# Install cross-compilation toolchain (one-time setup)
brew tap messense/macos-cross-toolchains
brew install aarch64-unknown-linux-gnu
rustup target add aarch64-unknown-linux-gnu

# Build the container image (cross-compiles supervisor + builds image)
make container-image

# Build the web frontend
make web
```

## Running

```sh
# Add a project to track
cargo run -- add-project owner/repo

# Build web frontend + start server on :4800
# Auto-restarts on exit code 100 (self-update: pulls, rebuilds, restarts)
make run

# Headless mode (no web UI)
cargo run -- run
```

## Development

```sh
# Run the backend separately
cargo run -- run --web

# Frontend dev server (proxies /api to localhost:4800)
bun web dev

# Build the web frontend only (production)
make web
```

## Make Targets

| Target | Description |
|---|---|
| `make run` | Build web frontend + start server (auto-restarts on update) |
| `make web` | Build the web frontend |
| `make container-image` | Cross-compile supervisor + build container image |
| `make supervisor` | Build the supervisor binary only |
| `make check-linker` | Verify cross-compilation toolchain is installed |
| `make clean` | Clean build artifacts |

## Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `GITHUB_TOKEN` | Yes | — | GitHub personal access token |
| `ANTHROPIC_API_KEY` | Yes | — | Anthropic API key (for agent sessions) |
| `TASKS_MAX_SESSIONS` | No | `5` | Max concurrent agent sessions |
| `TASKS_POLL_INTERVAL` | No | `60` | GitHub polling interval (seconds) |
| `TASKS_DISPATCH_INTERVAL` | No | `30` | Dispatch loop interval (seconds) |
| `TASKS_CONTAINER_IMAGE` | No | `tasks-agent:latest` | Container image for sessions |
| `TASKS_DATA_DIR` | No | `~/.local/state/tasks` | Data directory (SQLite + event logs) |

## License

MIT
