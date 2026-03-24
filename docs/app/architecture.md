# Architecture

Tasks is built as a modular Rust monorepo with a React web frontend. This document describes the system architecture and how components interact.

## System Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        Tasks Server                          │
├──────────────┬──────────────┬──────────────┬────────────────┤
│   Scheduler  │  Dispatcher  │ Merge Queue  │  Event System  │
│  (GitHub     │  (Session    │  (Quality    │  (Audit Log)   │
│   Polling)   │   Spawning)  │   Gate)      │                │
└──────┬───────┴──────┬───────┴──────┬───────┴────────────────┘
       │              │              │
       ▼              ▼              ▼
┌──────────────┐ ┌──────────────┐ ┌──────────────┐
│   GitHub     │ │  Containers  │ │   SQLite     │
│   API        │ │  (Agents)    │ │   Store      │
└──────────────┘ └──────────────┘ └──────────────┘
```

## Crate Architecture

### Core Crates

| Crate | Purpose |
|-------|---------|
| `app` | Binary entry point, startup, CLI, component wiring, update checker |
| `server` | HTTP server, run loops, domain logic |
| `models` | Shared domain types (Task, Project, Mode, etc.) |
| `store` | SQLite persistence layer |
| `events` | Append-only event log with pub/sub |

### Agent & Session Crates

| Crate | Purpose |
|-------|---------|
| `agent` | LLM abstraction layer, provider trait, session handling |
| `session` | Session lifecycle management, event bridging |
| `runtime` | Container lifecycle, JSON-line protocol |
| `supervisor` | PID 1 inside containers, workspace provisioning |

### Integration Crates

| Crate | Purpose |
|-------|---------|
| `github` | GraphQL client, normalized model, polling |
| `orchestrator` | AI project foreman, quality evaluation |

### UI Crates

| Crate | Purpose |
|-------|---------|
| `desktop` | GPUI-based native desktop app |
| `gpui-client` | HTTP client library for desktop |

## Data Flow

### Issue to PR Flow

1. **Scheduler** polls GitHub for issues/PRs
2. **Orchestrator** evaluates work and assigns tasks
3. **Dispatcher** spawns container session
4. **Agent** (Claude Code) works on the task
5. **Session** bridges events between agent and server
6. **Merge Queue** receives completed work for review
7. **Human** approves/rejects in merge queue
8. **Merger** lands approved changes

### Event System

All state changes are recorded as immutable events:

```
Event → EventBus → Subscribers
          ↓
      EventStore (JSONL)
```

Events are stored per-task in `~/.local/state/tasks/events/{task-id}/events.jsonl`.

## Session Runtime

### Container Isolation

Each agent session runs in an isolated container:

```
┌──────────────────────────────────┐
│         Host (Tasks Server)       │
│  ┌────────────────────────────┐  │
│  │    Container (Linux VM)     │  │
│  │  ┌──────────────────────┐  │  │
│  │  │     Supervisor       │  │  │
│  │  │  (PID 1, Rust)       │  │  │
│  │  │  ┌──────────────┐    │  │  │
│  │  │  │ Claude Code  │    │  │  │
│  │  │  │   (Agent)    │    │  │  │
│  │  │  └──────────────┘    │  │  │
│  │  └──────────────────────┘  │  │
│  └────────────────────────────┘  │
└──────────────────────────────────┘
```

### Host ↔ Container Protocol

Communication uses JSON-line protocol over stdio:

**Commands (Host → Container):**
- `start` - Initialize workspace and launch agent
- `chat` - Send message to running agent
- `stop` - Gracefully terminate agent
- `exec` - Execute arbitrary command

**Events (Container → Host):**
- `system:ready` - Supervisor initialized
- `agent:started` - Agent process launched
- `agent:stdout/stderr` - Agent output
- `agent:exit` - Agent terminated

## Self-Update

The server monitors the local git repository for new commits and can restart itself to apply updates.

### How It Works

1. **UpdateChecker** runs as a background task in the `app` crate (`update.rs`)
2. On each check interval, it runs `git diff` against the remote to detect new commits
3. When an update is found, it sets update state and optionally triggers an auto-apply
4. On apply, the server drains active sessions (up to `TASKS_UPDATE_SESSION_TIMEOUT`), writes a scope file, and exits with **code 100**
5. The wrapper script (`scripts/tasks-runner.sh`) detects exit code 100, pulls the latest code, rebuilds the appropriate scope, and restarts the server

### Rebuild Scopes

The update checker classifies changed files into one of three scopes:

| Scope | Triggered By | What Gets Rebuilt |
|-------|-------------|-------------------|
| `Full` | Changes to `Cargo.*`, `crates/supervisor/` | Supervisor + container image + server |
| `ServerOnly` | Changes to any other Rust source | Server binary only |
| `WebOnly` | Changes to `web/` only | Frontend assets only |
| `None` | No source changes | No rebuild (e.g. docs-only push) |

### Events

| Event | Description |
|-------|-------------|
| `system:update:available` | New commit detected; payload includes commit summary and rebuild scope |
| `system:update:applying` | Update in progress; sessions draining |

### Configuration

See [Configuration → Self-Update](configuration.md#self-update) for relevant environment variables.

## Storage

### SQLite Database

Located at `~/.local/state/tasks/db.sqlite`:

- Projects - Tracked repositories
- Tasks - Work items with state
- Merge Queue - Pending approvals
- Configuration - System settings

### Event Logs

Per-task event logs at `~/.local/state/tasks/events/{task-id}/events.jsonl`.

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_DATA_DIR` | Data directory | `~/.local/state/tasks/` |
| `GITHUB_TOKEN` | GitHub API token | Required |
| `ANTHROPIC_API_KEY` | Claude API key | Required |

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
