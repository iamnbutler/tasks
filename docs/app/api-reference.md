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

#### Rebuild from GitHub

Clears tasks and merge queue (memory + database) and signals the poll loop to re-fetch all data from GitHub. Preserves accounting data, event logs, projects, and operating mode. The actual re-fetch happens asynchronously.

```http
POST /api/rebuild
```

**Response:**

```json
{
  "tasks_cleared": 3,
  "merge_queue_cleared": 1
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
    "github_issue_number": 42,
    "created_at": "2024-01-15T10:30:00Z"
  }
]
```

#### Get Task by ID

```http
GET /api/tasks/:id
```

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

#### Update Task

Update a task's properties (currently supports `priority`).

```http
PATCH /api/tasks/:id
Content-Type: application/json

{
  "priority": 1
}
```

**Response:** Updated task object.

#### Reorder Tasks

Assign sequential priorities to tasks in the given order. Used for drag-and-drop reordering in the UI.

```http
POST /api/tasks/reorder
Content-Type: application/json

{
  "task_ids": ["uuid-1", "uuid-2", "uuid-3"]
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
    "state": "pending",
    "created_at": "2024-01-15T12:00:00Z"
  }
]
```

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

### Self-Update

Endpoints for checking and triggering server self-updates. Full update automation requires Phase 1 infrastructure (issue #319); these endpoints currently return stub responses.

#### Get Update Status

```http
GET /api/self-update
```

**Response:**

```json
{
  "available": false,
  "current_commit": "abc1234",
  "target_commit": "def5678",
  "rebuild_scope": "server",
  "commit_summary": "Fix SSE presence guard",
  "last_checked": "2024-01-15T10:30:00Z"
}
```

| Field | Description |
|-------|-------------|
| `available` | Whether an update is available |
| `current_commit` | Current running commit (short SHA) |
| `target_commit` | Target commit to update to (short SHA) |
| `rebuild_scope` | What needs rebuilding: `"server"`, `"container"`, or `"frontend"` |
| `commit_summary` | First line of the target commit message |
| `last_checked` | When update was last checked |

#### Apply Update

Triggers the update process: sets mode to Stop, waits for sessions to complete (unless `force=true`), then exits with code 100 for the wrapper script to restart.

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
  "message": "Waiting for 2 active sessions to complete"
}
```

**Status values:** `"applying"`, `"no_update"`, `"already_applying"`

### Events

#### Query Historical Events

Query historical events by type prefix across all task logs.

```http
GET /api/events/query?type_prefix=orchestrator:&limit=50
```

**Query Parameters:**

| Parameter | Description | Default |
|-----------|-------------|---------|
| `type_prefix` | Event type prefix to filter by (required) | — |
| `limit` | Maximum events to return | 200 |

**Response:** Array of event objects (sorted by timestamp ascending).

#### SSE Live Stream

Server-Sent Events stream for real-time updates.

```http
GET /api/events
```

**Query Parameters:**

| Parameter | Description |
|-----------|-------------|
| `pattern` | Filter by event type pattern (e.g. `task:*`, `system:update:*`) |
| `task_id` | Filter by task ID |

**Example:**

```bash
curl -N http://localhost:4800/api/events?task_id=abc123
curl -N "http://localhost:4800/api/events?pattern=system:update:*"
```

**Event Format:**

```
event: task:updated
data: {"id":"task-uuid","state":"completed"}
```

**Event Types:**

| Type | Description |
|------|-------------|
| `task:created` | New task created |
| `task:updated` | Task properties updated |
| `task:reordered` | Task queue reordered |
| `task:state:running` | Task agent started |
| `task:state:question` | Agent waiting for human input |
| `task:state:waiting` | Task waiting to be dispatched |
| `task:state:blocked` | Task blocked |
| `task:state:testing` | Agent running tests |
| `task:state:awaiting_merge` | PR submitted, awaiting merge review |
| `task:state:conflict` | Merge conflict detected |
| `task:state:changes_requested` | Changes requested on PR |
| `task:state:completed` | Task completed |
| `task:state:failed` | Task failed |
| `task:state:cancelled` | Task cancelled |
| `agent:message` | Agent output message |
| `agent:question` | Agent question to human |
| `agent:error` | Agent error |
| `human:message` | Human chat message sent |
| `merge:queued` | PR added to merge queue |
| `merge:approved` | Merge queue entry approved |
| `merge:rejected` | Merge queue entry rejected |
| `merge:changes_requested` | Changes requested |
| `merge:completed` | PR merged |
| `merge:conflict` | Merge conflict |
| `orchestrator:feedback` | Orchestrator quality feedback |
| `orchestrator:escalation` | Orchestrator escalation |
| `orchestrator:decision` | Orchestrator decision |
| `orchestrator:message` | Human message to orchestrator |
| `orchestrator:response` | Orchestrator LLM reply |
| `system:started` | Server started |
| `system:mode:play` / `pause` / `stop` | Mode changed |
| `system:flush` | Merge queue flushed |
| `system:rebuild` | State rebuilt from GitHub |
| `system:update:available` | New server update detected |
| `system:update:applying` | Update in progress |
| `system:accounting:tokens` | Token usage recorded |
| `system:accounting:api_call` | API call recorded |
| `system:accounting:session` | Session accounting summary |
| `system:memory:warning` / `pressure` / `emergency` | Memory threshold events |
| `system:time_limit:soft` / `hard` | Session time limit events |
| `workspace:cleaned` | Session workspace cleaned up |

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
