# API Reference

Tasks exposes a REST API for programmatic access. All endpoints are prefixed with `/api`.

## Base URL

```
http://localhost:4800/api
```

## Endpoints

### System

#### Get System Snapshot

Returns the complete system state.

```http
GET /api/snapshot
```

**Response:**

```json
{
  "mode": "play",
  "projects": [...],
  "tasks": [...],
  "merge_queue": [...],
  "slot_utilization": {
    "active": 2,
    "max": 5
  },
  "human_present": true
}
```

#### Get Current Mode

```http
GET /api/mode
```

**Response:**

```json
{
  "mode": "play"
}
```

#### Set Operating Mode

```http
POST /api/mode
Content-Type: application/json

{
  "mode": "pause"
}
```

**Valid modes:** `play`, `pause`, `stop`

> **Note:** Transitioning to `stop` mode terminates all running agent sessions (5-second graceful timeout before force-destroy).

#### Rebuild from GitHub

Clears tasks and merge queue (memory + database), then signals the poll loop to re-fetch all data from GitHub. Preserves accounting data, event logs, projects table, and operating mode.

> **Note:** The response contains counts of items *cleared*. The actual re-fetch happens asynchronously as the poll loop re-discovers items from GitHub.

```http
POST /api/rebuild
```

**Response:**

```json
{
  "tasks_cleared": 12,
  "merge_entries_cleared": 3
}
```

### Tasks

#### List All Tasks

```http
GET /api/tasks
```

**Response:**

```json
[
  {
    "id": "task-uuid",
    "title": "Fix login bug",
    "state": "in_progress",
    "project_id": "project-uuid",
    "source_number": 42,
    "source_created_at": "2024-01-10T08:00:00Z",
    "priority": null,
    "created_at": "2024-01-15T10:30:00Z"
  }
]
```

#### Get Task by ID

```http
GET /api/tasks/:id
```

#### Update Task

Update a task's properties. Currently supports updating `priority` for manual queue reordering.

```http
PATCH /api/tasks/:id
Content-Type: application/json

{
  "priority": 1
}
```

**Response:** Updated task object.

#### Reorder Tasks

Bulk reorder tasks by assigning sequential priorities. Used by the drag-and-drop Queue view.

```http
POST /api/tasks/reorder
Content-Type: application/json

{
  "task_ids": ["uuid-1", "uuid-2", "uuid-3"]
}
```

Tasks are assigned priorities 1, 2, 3, … in the given order.

#### Get Task Events

Returns the event history for a task.

```http
GET /api/tasks/:id/events
```

**Response:**

```json
[
  {
    "timestamp": "2024-01-15T10:30:00Z",
    "type": "task:created",
    "data": {...}
  }
]
```

#### Send Chat Message

Send a message to an active agent session.

```http
POST /api/tasks/:id/chat
Content-Type: application/json

{
  "message": "Can you also add tests for this?"
}
```

#### Cancel Task

Stop a running agent session for the given task.

```http
POST /api/tasks/:id/cancel
```

### Issues

#### Create Issue

Create a new GitHub issue in a tracked project. The poller will pick it up on its next cycle and create a task from it.

```http
POST /api/issues
Content-Type: application/json

{
  "project_id": "project-uuid",
  "title": "Fix the login bug",
  "body": "Description in markdown (optional)",
  "labels": ["bug", "priority:high"]
}
```

**Response:**

```json
{
  "number": 42,
  "url": "https://github.com/owner/repo/issues/42"
}
```

### Projects

#### List Projects

```http
GET /api/projects
```

**Response:**

```json
[
  {
    "id": "project-uuid",
    "repo": "owner/repo",
    "created_at": "2024-01-10T08:00:00Z"
  }
]
```

#### Add Project

```http
POST /api/projects
Content-Type: application/json

{
  "repo": "owner/repo"
}
```

#### Remove Project

Removes the project and cascades deletion of all associated tasks and merge queue entries in a single transaction.

```http
DELETE /api/projects/:id
```

### Merge Queue

#### List Merge Queue Entries

```http
GET /api/merge-queue
```

**Response:**

```json
[
  {
    "id": "entry-uuid",
    "task_id": "task-uuid",
    "pr_number": 123,
    "pr_url": "https://github.com/owner/repo/pull/123",
    "head_sha": "abc1234",
    "state": "pending",
    "created_at": "2024-01-15T12:00:00Z"
  }
]
```

The `head_sha` field is populated during GitHub reconciliation and used to detect new commits that require re-evaluation.

#### Approve Entry

In Play mode, approval triggers an immediate GitHub merge. In Pause mode, the entry is approved but merges on flush.

```http
POST /api/merge-queue/:id/approve
```

#### Reject Entry

```http
POST /api/merge-queue/:id/reject
```

#### Request Changes

Keeps the entry in the queue and gives the task priority dispatch so the agent can address feedback.

```http
POST /api/merge-queue/:id/request-changes
Content-Type: application/json

{
  "reasoning": "The implementation is incomplete",
  "feedback": "Please add unit tests for the edge cases in login.rs"
}
```

#### Flush Approved Entries

Merge all approved entries via GitHub API (Pause mode only). Returns the IDs of successfully merged entries.

```http
POST /api/merge-queue/flush
```

### Orchestrator

#### Chat with Orchestrator

Send a message to the AI project foreman.

```http
POST /api/orchestrator/chat
Content-Type: application/json

{
  "message": "What tasks are currently being worked on?"
}
```

### Self-Update

#### Get Update Status

Returns the current update status from the background `UpdateChecker`.

```http
GET /api/self-update
```

**Response when update is available:**

```json
{
  "available": true,
  "applying": false,
  "current_commit": "abc1234",
  "target_commit": "def5678",
  "rebuild_scope": "server",
  "commit_summary": null,
  "last_checked": null
}
```

**Response when no update:**

```json
{
  "available": false,
  "applying": false,
  "current_commit": null,
  "target_commit": null,
  "rebuild_scope": null,
  "commit_summary": null,
  "last_checked": null
}
```

| Field | Description |
|-------|-------------|
| `available` | Whether a newer commit is available upstream |
| `applying` | Whether an update is currently being applied |
| `current_commit` | Short (7-char) hash of the running commit |
| `target_commit` | Short (7-char) hash of the available update |
| `rebuild_scope` | What needs rebuilding: `"server"`, `"container"`, or `"frontend"` |
| `commit_summary` | First line of the target commit message (not yet populated) |
| `last_checked` | Timestamp of last check (not yet populated) |

#### Apply Update

Triggers the self-update shutdown path. The server sets mode to Stop, waits for active sessions to drain (unless `force=true`), then exits with code 100 for the wrapper script to rebuild and restart.

```http
POST /api/self-update/apply
Content-Type: application/json

{
  "force": false
}
```

**Response:**

```json
{
  "status": "applying",
  "message": "Update is being applied. The server will restart shortly."
}
```

| `status` | Meaning |
|----------|---------|
| `"applying"` | Update triggered successfully |
| `"no_update"` | No update available |
| `"already_applying"` | An update is already in progress |

### Accounting

#### Get Accounting Summary

Global summary of API token usage across all tasks.

```http
GET /api/accounting
```

#### List Task Accounting

Per-task accounting summaries.

```http
GET /api/accounting/tasks
```

#### Get Task Accounting

Accounting for a specific task.

```http
GET /api/accounting/tasks/:id
```

### Completions

Fast LLM completions powered by Haiku. All endpoints enforce a 32 KB input limit.

#### General Completion

```http
POST /api/completions
Content-Type: application/json

{
  "prompt": "Summarize this PR in one sentence",
  "system": "You are a helpful assistant",
  "max_tokens": 1024
}
```

**Response:** `{ "text": "..." }`

#### Generate Name

```http
POST /api/completions/name
Content-Type: application/json

{ "context": "Fix login page redirect loop after auth" }
```

**Response:** `{ "name": "..." }`

#### Generate Description

```http
POST /api/completions/describe
Content-Type: application/json

{ "context": "..." }
```

**Response:** `{ "description": "..." }`

#### Brainstorm Ideas

```http
POST /api/completions/brainstorm
Content-Type: application/json

{ "topic": "Ways to improve test coverage", "count": 5 }
```

**Response:** `{ "ideas": ["...", ...] }`

#### Summarize Text

```http
POST /api/completions/summarize
Content-Type: application/json

{ "text": "...", "max_words": 50 }
```

**Response:** `{ "summary": "..." }`

### Events

#### Query Historical Events

Query the event log by type prefix. Returns up to `limit` events (default 200), sorted by timestamp ascending. The `type_prefix` parameter is required.

```http
GET /api/events/query?type_prefix=orchestrator:&limit=100
```

**Query Parameters:**

| Parameter | Description |
|-----------|-------------|
| `type_prefix` | Event type prefix to filter by (required, e.g. `"orchestrator:"`, `"task:"`) |
| `limit` | Maximum events to return (default: 200) |

**Response:** Array of event objects.

#### SSE Live Stream

Server-Sent Events stream for real-time updates.

```http
GET /api/events
```

**Query Parameters:**

| Parameter | Description |
|-----------|-------------|
| `pattern` | Filter by event type pattern |
| `task_id` | Filter by task ID |

**Example:**

```bash
curl -N http://localhost:4800/api/events?task_id=abc123
```

**Event Format:**

```
event: task:updated
data: {"id":"task-uuid","state":"completed"}
```

**Common Event Types:**

| Type | Description |
|------|-------------|
| `task:created` | New task created from a GitHub issue |
| `task:updated` | Task state changed |
| `task:closed` | Task closed externally; data includes `{"source":"reconciliation"}` |
| `task:reordered` | Task priority updated via manual reorder |
| `session:started` | Agent session began |
| `session:ended` | Agent session completed |
| `merge:approved` | Entry approved in queue |
| `merge:completed` | Changes merged |
| `orchestrator:message` | Human message sent to orchestrator |
| `orchestrator:response` | Orchestrator response |
| `system:update:available` | Update detected by background checker |
| `system:update:applying` | Update process has started |

> **Note on `task:closed`:** Both issue closures (detected via `reconcile_task()`) and PR closures (detected in the run loop) emit `task:closed` events with `{ "source": "reconciliation" }` data, providing a unified event format for external closures detected during polling.

## Error Responses

All errors return JSON with the following structure:

```json
{
  "error": "Error message"
}
```

**HTTP Status Codes:**

| Code | Description |
|------|-------------|
| 200 | Success |
| 400 | Bad Request |
| 404 | Not Found |
| 502 | Bad Gateway (GitHub API error) |
| 503 | Service Unavailable (completions not configured) |
| 500 | Internal Server Error |

---

*This documentation is automatically maintained. Last updated: 2026-03-24*
