# Web UI Guide

The Tasks web interface provides a dashboard for monitoring and managing the platform.

## Accessing the UI

Start Tasks with the `--web` flag:

```bash
cargo run -- run --web
```

Open your browser to `http://localhost:4800`.

## Navigation

The main navigation includes:

| Section | Description |
|---------|-------------|
| **Dashboard** | System overview and statistics |
| **Tasks** | Task list with tabs for filtering and detail views |
| **Merge Queue** | Review and approve pending changes |
| **Orchestrator** | AI project foreman feed and chat |
| **Events** | Real-time and historical event log viewer |

## Dashboard

The dashboard provides an at-a-glance view of:

- **Operating Mode** - Current mode (Play/Pause/Stop)
- **Active Tasks** - Number of tasks in progress
- **Pending Reviews** - Merge queue items awaiting approval
- **Recent Activity** - Latest events across all tasks
- **Escalations** - Up to 5 recent orchestrator escalation events with task context

### Changing Mode

Click the mode indicator to switch between:

- **Play** - Full autonomy; merges happen automatically on approval
- **Pause** - Agents work normally; only approved merges are held until flush. Rejections, conflict handling, and changes-requested all execute normally.
- **Stop** - All activity halted; running sessions are terminated

### Update Banner

When the background update checker detects a newer version, an **Update Available** banner appears at the top of the dashboard. The banner shows the target commit hash and the rebuild scope (`server`, `container`, or `frontend`). Clicking **Update Now** triggers `POST /api/self-update/apply` and the server restarts via the wrapper script.

## Tasks View

### Task List Tabs

The task list is organized into tabs:

| Tab | States shown | Description |
|-----|-------------|-------------|
| **Active** | `running`, `question`, `testing` | Tasks currently being worked on |
| **Backlog** | `waiting`, `blocked`, `changes_requested` | Tasks queued or paused |
| **Completed** | `completed`, `failed`, `cancelled` | Finished tasks |
| **All** | All states | Unfiltered full task list |
| **Queue** | `waiting`, `changes_requested` | Dispatch order with drag-to-reorder |

### Queue Tab

The Queue tab shows tasks that are next to be dispatched to agents, ordered by priority. Drag rows to reorder them; changes call `PATCH /api/tasks/:id` and `POST /api/tasks/reorder` to persist the new order. Tasks with lower priority numbers are dispatched first.

### Task List Columns

The task list displays:

- **Status** - State indicator badge
- **ID** - Task identifier with a link to the GitHub issue
- **Title** - Task title
- **Project** - Source repository
- **PR** - Pull request link (shown when a task has a merge queue entry)
- **Labels** - GitHub labels
- **Updated** - Last updated timestamp

### Creating a Task

Use the **New Task** button to create a GitHub issue directly from the UI without leaving Tasks. Select a project, enter a title and optional description (Markdown), and add labels. The poller picks up the new issue on its next cycle.

### Task Detail

Click a task to view its detail page. The detail view is split into tabs:

| Tab | Description |
|-----|-------------|
| **Chat** | Send messages to the active agent session |
| **Details** | Full task description (Markdown rendered) |

A **Properties** sidebar on the right shows metadata (state, project, created date) and the task event timeline.

## Merge Queue

The merge queue shows PRs awaiting human review.

### Entry States

| State | Description |
|-------|-------------|
| **Pending** | Awaiting review |
| **Approved** | Ready to merge |
| **Rejected** | Declined |
| **Conflict** | PR is not mergeable |
| **Changes Requested** | Needs modification before merging |

### Actions

For each entry:

- **Approve** - Mark ready for merge. In Play mode, triggers an immediate GitHub merge. In Pause mode, entry waits for flush.
- **Reject** - Decline the changes
- **Request Changes** - Ask for modifications with specific feedback; the task gets priority re-dispatch

### Flush

In Pause mode, use the **Flush** button to merge all approved entries via the GitHub API.

## Orchestrator

The Orchestrator view shows a conversational feed of the AI project foreman's activity.

### Feed

Historical orchestrator events are loaded on page mount and merged with the live SSE stream (deduplicated by event ID), so you see the full conversation even after navigating away and back. Events appear as context-rich messages including:

- **Decisions** - e.g., "Approving 'Fix login bug' (#42) in owner/repo"
- **Feedback** - Orchestrator comments on work quality
- **Escalations** - Issues requiring human intervention, with actionable context and PR links

Conversation history is bounded at 40 messages for chat context.

### Chat

Type messages in the input field to ask the orchestrator questions or provide direction. The orchestrator responds via the event feed. Chat messages bypass the merge queue and are handled directly.

## Events

The Events view shows a real-time stream of system events with historical replay.

### Filter Tabs

| Tab | Description |
|-----|-------------|
| **Important** | Default view — hides verbose `agent:message` / `human:message` events |
| **All** | Every event |
| **Task** | `task:*` events |
| **Agent** | `agent:*` events (session output) |
| **Merge** | `merge:*` events |
| **System** | `system:*` events (update, mode changes) |
| **Orchestrator** | `orchestrator:*` events |

The **Important** filter is the default to avoid noise from high-frequency message events.

### Pause / Resume

The Events page has a **Pause** button to freeze the live stream for inspection without disconnecting. Click **Resume** to catch up with buffered events.

### Event Types

| Type | Description |
|------|-------------|
| `task:created` | New task created from a GitHub issue |
| `task:updated` | Task state changed |
| `task:closed` | Task closed externally (source: `reconciliation`) |
| `task:reordered` | Task priority updated via manual reorder |
| `session:started` | Agent session began |
| `session:ended` | Agent session completed |
| `merge:approved` | Entry approved in queue |
| `merge:completed` | Changes merged |
| `orchestrator:message` | Human message sent to orchestrator |
| `orchestrator:response` | Orchestrator response |
| `system:update:available` | Update detected by background checker |
| `system:update:applying` | Update process has started |

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `?` | Show help |
| `g d` | Go to Dashboard |
| `g t` | Go to Tasks |
| `g m` | Go to Merge Queue |
| `g o` | Go to Orchestrator |
| `g e` | Go to Events |

---

*This documentation is automatically maintained. Last updated: 2026-03-24*
