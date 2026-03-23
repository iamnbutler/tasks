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
  "slot_utilization": { "active": 2, "max": 5 },
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

Stop a running agent session.

```http
POST /api/tasks/:id/cancel
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

### Issues

#### Create Issue

Create a new GitHub issue in a tracked project. The poller will pick it up on its next cycle and create a task.

```http
POST /api/issues
Content-Type: application/json

{
  "project_id": "project-uuid",
  "title": "Fix login bug",
  "body": "Optional markdown description",
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

```http
POST /api/merge-queue/:id/approve
```

#### Reject Entry

```http
POST /api/merge-queue/:id/reject
```

#### Request Changes

Unlike rejection, the entry stays in the queue and the task receives priority dispatch.

```http
POST /api/merge-queue/:id/request-changes
Content-Type: application/json

{
  "reasoning": "Why changes are needed",
  "feedback": "Specific, actionable instructions for the agent"
}
```

#### Flush Approved Entries

Merge all approved entries (Pause mode only). Calls the GitHub API for each entry and returns the IDs of successfully merged entries.

```http
POST /api/merge-queue/flush
```

**Response:** Array of merged entry IDs.

```json
["entry-uuid-1", "entry-uuid-2"]
```

### Orchestrator

#### Send Orchestrator Message

Send a message to the orchestrator. Emits an `orchestrator:message` event that the orchestrator processes.

```http
POST /api/orchestrator/chat
Content-Type: application/json

{
  "message": "What's the status of the authentication work?"
}
```

### Accounting

#### Get Accounting Summary

Get global token usage and cost summary.

```http
GET /api/accounting
```

#### List Task Accounting

Get per-task token usage summaries.

```http
GET /api/accounting/tasks
```

#### Get Task Accounting

Get token usage for a specific task.

```http
GET /api/accounting/tasks/:id
```

### Completions

Fast LLM completion endpoints powered by Claude Haiku. Input is limited to 32 KB.

#### General Completion

```http
POST /api/completions
Content-Type: application/json

{
  "prompt": "Your prompt here",
  "system": "Optional system prompt",
  "max_tokens": 1024
}
```

**Response:** `{ "text": "..." }`

#### Generate Name

Generate a concise name from context.

```http
POST /api/completions/name
Content-Type: application/json

{ "context": "task title and summary" }
```

**Response:** `{ "name": "..." }`

#### Generate Description

Generate a brief description (1–2 sentences) from context.

```http
POST /api/completions/describe
Content-Type: application/json

{ "context": "..." }
```

**Response:** `{ "description": "..." }`

#### Brainstorm Ideas

Generate a list of ideas for a topic.

```http
POST /api/completions/brainstorm
Content-Type: application/json

{ "topic": "...", "count": 5 }
```

**Response:** `{ "ideas": ["...", "..."] }`

#### Summarize Text

Summarize text with an optional word limit.

```http
POST /api/completions/summarize
Content-Type: application/json

{ "text": "...", "max_words": 100 }
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
  "error": "Error message",
  "code": "ERROR_CODE"
}
```

**HTTP Status Codes:**

| Code | Description |
|------|-------------|
| 200 | Success |
| 400 | Bad Request |
| 404 | Not Found |
| 500 | Internal Server Error |
| 502 | Bad Gateway (GitHub API error) |
| 503 | Service Unavailable (completions not configured) |

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
