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
| `app` | Binary entry point, startup, CLI, component wiring |
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
| `orchestrator` | AI project foreman, quality evaluation, agent dispatch |

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
8. **Merger** lands approved changes; transient GitHub API failures revert the entry to Approved for automatic retry on the next poll cycle

> If a GitHub issue is closed externally while a task session is active, the session is stopped automatically (5-second graceful shutdown). The task state is updated to terminal in the same reconciliation pass.

> If a PR receives new commits after being approved, its merge queue entry resets from Approved back to Pending so the orchestrator can re-evaluate the updated code.

### Event System

All state changes are recorded as immutable events:

```
Event → EventBus → Subscribers
          ↓
      EventStore (JSONL)
```

Events are stored per-task in `~/.local/state/tasks/events/{task-id}/events.jsonl`.

## Orchestrator <!-- LAST_UPDATED: 2026-04-01 -->

The orchestrator is an AI project foreman that runs a `think()` pass each poll cycle. It evaluates open tasks, reviews completed work, and emits `OrchestratorAction` values (intentions) for the run loop to execute. This keeps orchestrator logic pure — it returns decisions, not side effects.

### Actions

| Action | Description |
|--------|-------------|
| `EmitThought` | Append a thought to the narration feed |
| `UpdateTaskState` | Transition a task to a new state |
| `PrioritizeTask` | Request priority dispatch for a task |
| `DispatchAgent` | Spawn a one-off bounded agent session |

### Dispatch Agent

The orchestrator can spawn one-off agent sessions for targeted work — codebase investigation, issue triage, or small fixes identified during review. These sessions are bounded by time (default: 5 minutes) and turn limits.

Dispatched agents report back via events:

| Event | Meaning |
|-------|---------|
| `orchestrator:agent:dispatched` | Session spawned |
| `orchestrator:agent:completed` | Session finished successfully |
| `orchestrator:agent:failed` | Session failed or timed out |

Results flow back into the orchestrator via `recent_events` on the next `think()` pass.

### Specialized Agent Types <!-- LAST_UPDATED: 2026-04-01 -->

Agents are dispatched with a type that controls their tool access, model, and turn limits. The supervisor enforces these restrictions when starting the agent process.

| Type | Purpose | Tool Access |
|------|---------|-------------|
| `implementer` | Full coding sessions | All tools |
| `reviewer` | Read-only quality checks | No write/exec tools |
| `explorer` | Codebase research | Read-only tools |

The orchestrator selects the agent type based on the task. For example, code review dispatches a `reviewer` agent; investigating a pattern dispatches an `explorer`.

### Chat History

Orchestrator chat history is restored from the event log on startup, so conversation context persists across server restarts.

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

## Storage

### SQLite Database <!-- LAST_UPDATED: 2026-03-27 -->

Located at `~/.local/state/tasks/db.sqlite`. Uses an r2d2 connection pool with WAL mode so reads don't block writes:

- Projects - Tracked repositories
- Tasks - Work items with state
- Merge Queue - Pending approvals
- Configuration - System settings

### Event Logs <!-- LAST_UPDATED: 2026-03-27 -->

Per-task event logs at `~/.local/state/tasks/events/{task-id}/events.jsonl`.

Event logs are compacted hourly according to a configurable retention policy. Compaction trims old events per-task and removes orphaned task directories that no longer have corresponding database entries.

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_DATA_DIR` | Data directory | `~/.local/state/tasks/` |
| `GITHUB_TOKEN` | GitHub API token | Required |
| `ANTHROPIC_API_KEY` | Claude API key | Required |

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED: 2026-04-01 -->*
