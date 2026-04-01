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
  "automations": [...],
  "slot_utilization": {
    "active": 2,
    "max": 5
  },
  "human_present": true,
  "server_version": "abc1234",
  "data_version": 7
}
```

`server_version` is the current binary's git commit SHA. `data_version` is the SQLite schema version. The web UI uses `data_version` to detect schema changes across sessions and prompt for a reload.

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

#### Rebuild State from GitHub

Clear all tasks and merge queue entries, then let the poller re-fetch everything from GitHub. Useful for recovering from a desync.

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

> Re-population happens asynchronously as the GitHub poller discovers items on its next cycle.

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
    "created_at": "2024-01-15T10:30:00Z",
    "rejection_feedback": null
  }
]
```

`rejection_feedback` is populated when the orchestrator rejects a PR and provides feedback for the agent. It is `null` when no feedback is present.

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

#### Update Task

Update task metadata. All fields are optional.

```http
PATCH /api/tasks/:id
Content-Type: application/json

{
  "priority": 1
}
```

Lower priority values are dispatched first.

**Response:** The updated `Task` object.

#### Reorder Tasks

Assign sequential priorities to tasks in the given order. Used for drag-and-drop reordering in the UI.

```http
POST /api/tasks/reorder
Content-Type: application/json

{
  "task_ids": ["task-uuid-1", "task-uuid-2", "task-uuid-3"]
}
```

Tasks are assigned priorities 1, 2, 3… in the provided order.

**Response:** `200 OK`

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

#### Bootstrap Project

Create a new private GitHub repository, add it as a project, and open an initial issue. The poller picks up the issue and dispatches an agent automatically.

```http
POST /api/projects/bootstrap
Content-Type: application/json

{
  "prompt": "Build a CLI tool that converts CSV to JSON",
  "repo_name": "csv-to-json"
}
```

`repo_name` is optional — if omitted, a name is derived from the prompt.

**Response:**

```json
{
  "project": { "id": "...", "repo": "owner/csv-to-json", "created_at": "..." },
  "issue": { "number": 1, "url": "https://github.com/owner/csv-to-json/issues/1" },
  "repo_url": "https://github.com/owner/csv-to-json"
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

#### Approve Entry <!-- LAST_UPDATED: 2026-04-01 -->

In Play mode, approval triggers an immediate GitHub merge. In Pause mode, the entry is approved but merges on flush. If the GitHub merge fails transiently, the entry reverts to Approved and will be retried automatically on the next poll cycle.

> **Note:** If the agent pushes new commits to the PR after it has been approved, the entry automatically resets from Approved back to Pending so the orchestrator can re-evaluate the updated code.

```http
POST /api/merge-queue/:id/approve
```

#### Reject Entry

Permanently rejects the entry and persists the rejection to the database.

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

Merge all approved entries via GitHub API (Pause mode only). Returns the IDs of successfully merged entries. Entries that fail to merge transiently are reverted to Approved and will be retried on the next flush or poll cycle.

```http
POST /api/merge-queue/flush
```

### Containers

#### List Active Containers

Returns all currently running container sessions.

```http
GET /api/containers
```

**Response:**

```json
[
  {
    "container_id": "container-abc123",
    "task_id": "task-uuid",
    "started_at": "2024-01-15T10:30:00Z",
    "uptime_secs": 342
  }
]
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

### Automations

Automations are reusable agent workflows that can be triggered manually, on a schedule, or by events.

#### List Automations

```http
GET /api/automations
GET /api/automations?project_id=<project-uuid>
```

**Response:** Array of `Automation` objects.

#### Create Automation

```http
POST /api/automations
Content-Type: application/json

{
  "project_id": "project-uuid",
  "name": "Daily dependency audit",
  "prompt": "Check for outdated dependencies and open an issue if any are found.",
  "trigger": { "type": "schedule", "cron": "0 9 * * 1-5" }
}
```

`trigger` variants:
- `{ "type": "manual" }` — triggered via API only
- `{ "type": "schedule", "cron": "0 9 * * *" }` — cron schedule
- `{ "type": "event", "event_type": "push" }` — fires on a platform event

**Response:** The created `Automation` object.

#### Get Automation

```http
GET /api/automations/:id
```

#### Update Automation

```http
PATCH /api/automations/:id
Content-Type: application/json

{
  "name": "New name",
  "prompt": "Updated prompt",
  "state": "paused",
  "trigger": { "type": "manual" }
}
```

All fields are optional.

#### Delete Automation

```http
DELETE /api/automations/:id
```

#### List Automation Runs

```http
GET /api/automations/:id/runs
```

**Response:**

```json
[
  {
    "id": "run-uuid",
    "automation_id": "automation-uuid",
    "status": "completed",
    "started_at": "2024-01-15T09:00:00Z",
    "completed_at": "2024-01-15T09:04:12Z",
    "output": "No outdated dependencies found.",
    "error": null
  }
]
```

**Run statuses:** `pending`, `running`, `completed`, `failed`, `cancelled`

#### Get Automation Run Events

Returns the event stream for a specific automation run (agent output, tool calls, etc.).

```http
GET /api/automations/:id/runs/:run_id/events
```

**Response:** Array of `Event` objects for the run.

#### Trigger Automation Run

Manually start a run for an automation regardless of its trigger type.

```http
POST /api/automations/:id/run
```

**Response:** The created `AutomationRun` object.

#### Cancel Automation Run

Cancel a running or pending automation run.

```http
POST /api/automations/:id/runs/:run_id/cancel
```

**Response:** The updated `AutomationRun` object with status `cancelled`.

### Self-Update

#### Get Update Status

```http
GET /api/self-update
```

**Response:**

```json
{
  "available": true,
  "applying": false,
  "current_commit": "abc1234",
  "target_commit": "def5678",
  "rebuild_scope": "server",
  "commit_summary": "Fix merge queue flush race condition",
  "last_checked": "2024-01-15T12:00:00Z"
}
```

`rebuild_scope` is one of `"server"`, `"container"`, or `"frontend"`.

#### Apply Update

```http
POST /api/self-update/apply
Content-Type: application/json

{
  "force": false
}
```

`force: true` skips waiting for active sessions to complete.

**Response:**

```json
{
  "status": "applying",
  "message": "Update is being applied."
}
```

`status` values: `"applying"`, `"no_update"`, `"already_applying"`.

### Events (SSE)

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

**Common event type prefixes:**

| Prefix | Description |
|--------|-------------|
| `task:` | Task state changes |
| `orchestrator:` | Orchestrator thoughts and actions |
| `orchestrator:agent:` | Orchestrator-dispatched agent sessions (`dispatched`, `completed`, `failed`) |
| `session:` | Container session lifecycle |
| `merge:` | Merge queue events |

#### Query Historical Events

Query past events from the event log by type prefix.

```http
GET /api/events/query?type_prefix=orchestrator:&limit=50
```

**Query Parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `type_prefix` | Yes | Event type prefix to filter by (e.g. `"task:"`, `"orchestrator:"`) |
| `limit` | No | Maximum results to return (default: 200) |

**Response:** Array of `Event` objects matching the prefix.

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

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED: 2026-04-01 -->*
