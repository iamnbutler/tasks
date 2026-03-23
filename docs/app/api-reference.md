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
    "created_at": "2024-01-15T12:00:00Z",
    "head_sha": "abc123def456"
  }
]
```

> **Note:** `head_sha` is the current head commit SHA of the PR branch. It is populated during GitHub reconciliation and used to detect new commits that require re-evaluation by the orchestrator. It may be `null` until the first reconciliation cycle.

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

*This documentation is automatically maintained. Last updated: 2026-03-23*
