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
| `agent` | LLM abstraction layer, provider trait, session handling with context compaction |
| `session` | Session lifecycle management, event bridging |
| `runtime` | Container lifecycle, JSON-line protocol |
| `supervisor` | PID 1 inside containers, workspace provisioning |

### Integration Crates

| Crate | Purpose |
|-------|---------|
| `github` | GraphQL client, normalized model, polling |
| `orchestrator` | AI project foreman, quality evaluation; records `missing_context` on evaluations when PR diffs or linked issues can't be fetched |

### UI Crates

| Crate | Purpose |
|-------|---------|
| `desktop` | GPUI-based native desktop app |
| `gpui-client` | HTTP client library for desktop |

## Data Flow

### Issue to PR Flow

1. **Scheduler** polls GitHub for issues/PRs; closed issues are imported as terminal tasks (Completed or Cancelled based on closure reason) rather than skipped, so they are tracked as "seen"
2. **Orchestrator** evaluates work and assigns tasks
3. **Dispatcher** spawns container session
4. **Agent** (Claude Code) works on the task
5. **Session** bridges events between agent and server
6. **Merge Queue** receives completed work for review; entries with `mergeable_unknown: true` (GitHub's mergeability computation still pending) are skipped by the orchestrator until GitHub reports a definitive status
7. **Human** approves/rejects in merge queue
8. **Merger** checks CI status before executing merge:
   - CI passing → merge proceeds
   - CI pending → deferred to next poll cycle
   - CI failing → entry reverts to "request changes" with feedback about failed checks; agent is re-dispatched to fix without losing the branch/PR
   - No CI configured → merge proceeds
   - Transient GitHub API failures → entry reverts to Approved and retried on next poll cycle

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

## Quality Evaluation <!-- LAST_UPDATED: 2026-04-07 -->

When the orchestrator evaluates a PR for merge readiness, it produces a `QualityEvaluation` with:

- `approved: bool` — whether the PR meets quality standards
- `reasoning: String` — the orchestrator's rationale
- `feedback: Option<String>` — specific guidance for the implementor if changes are needed
- `missing_context: Vec<String>` — what data was unavailable during evaluation (e.g. `"pr_diff"`, `"associated_issue"`)

When `missing_context` is non-empty the evaluation was made with incomplete information. The orchestrator surfaces this in its PR comment so humans can gauge the confidence level of the decision.

## Context Management <!-- LAST_UPDATED: 2026-04-06 -->

Agent sessions handling long-running tasks can accumulate conversation history that exceeds the model's context window. The `agent` crate uses a two-stage strategy to keep sessions within budget:

### 1. LLM Summarization (Compaction)

When estimated tokens exceed 85% of the context window, older messages are summarized via an LLM call before the next turn:

- The first message (task context) and the most recent ~10 messages are preserved verbatim.
- Everything in between is replaced with a single summary message that retains key decisions, file paths, errors, and outstanding action items.
- The summary is produced by calling the same provider with a dedicated summarization system prompt.
- `Session::compaction_count` tracks how many times compaction has run.

The threshold is controlled by `CompletionConfig::compact_threshold` (default `0.85`).

### 2. Hard Truncation (Fallback)

If compaction cannot run (fewer than 4 messages) or the session is still over budget after compaction, `truncate_to_budget()` drops messages from the middle, keeping the first message and as many recent messages as fit. Orphaned `ToolResult` messages at the truncation boundary are skipped to maintain API invariants.

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

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
