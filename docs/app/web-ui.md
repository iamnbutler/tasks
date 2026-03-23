# Web UI Guide

The Tasks web interface provides a dashboard for monitoring and managing the platform.

## Accessing the UI

Start Tasks with the `--web` flag:

```bash
tasks run --web
```

Open your browser to `http://localhost:4800`.

## Navigation

The main navigation includes:

| Section | Description |
|---------|-------------|
| **Dashboard** | System overview and statistics |
| **Tasks** | Task list with filtering and details |
| **Merge Queue** | Review and approve pending changes |
| **Orchestrator** | Chat with the AI project foreman |
| **Events** | Real-time event log viewer |

## Dashboard

The dashboard provides an at-a-glance view of:

- **Operating Mode** - Current mode (Play/Pause/Stop)
- **Active Tasks** - Number of tasks in progress
- **Pending Reviews** - Merge queue items awaiting approval
- **Recent Activity** - Latest events across all tasks

### Changing Mode

Click the mode indicator to switch between:

- **Play** - Full autonomy
- **Pause** - Agents work, merges paused
- **Stop** - All activity halted

## Tasks View

### Creating a Task

Use the **New Task** button to create a GitHub issue directly from the UI. Select a project, enter a title and optional markdown description, and optionally add comma-separated labels. The poller picks up the new issue on its next cycle.

### Task List

The task list shows all tasks with columns for:

- Status (state indicator)
- Title
- Project
- Assignee (if any)
- Created date

### Filtering

Use the filter controls to narrow down tasks:

- **State** - Filter by task state
- **Project** - Filter by project
- **Search** - Text search in title/description

### Task Detail

Click a task to view its detail page. The main area has two tabs:

- **Chat Tab** - Agent session messages and human-in-the-loop interaction
- **Details Tab** - Full task description rendered as markdown

The properties sidebar on the right shows metadata (state, source issue/PR, labels, timestamps) and a collapsible event timeline.

## Merge Queue

The merge queue shows PRs awaiting human review.

### Entry States

| State | Description |
|-------|-------------|
| **Pending** | Awaiting review |
| **Approved** | Ready to merge |
| **Rejected** | Declined |
| **Changes Requested** | Needs modification |

### Actions

For each entry:

- **Approve** - Mark ready for merge
- **Reject** - Decline the changes
- **Request Changes** - Ask for modifications with feedback

### Flush

In Pause mode, use the "Flush" button to merge all approved entries.

## Orchestrator

The Orchestrator view shows a conversational, context-rich feed of orchestrator activity plus a chat input to send messages to the AI project foreman.

### Feed

The feed displays orchestrator events with full context:

- **Decisions** - Approval/rejection reasoning with task titles and PR links
- **Feedback** - Actionable feedback sent to agents
- **Escalations** - Issues requiring human attention, with context on what happened and what to do
- **Mode changes** - Operating mode transitions with reasoning

### Chat

Type messages in the input field and press Enter to send. The orchestrator processes the message and responds via the event feed.

## Events

The Events view shows a real-time stream of system events.

### Event Types

| Type | Description |
|------|-------------|
| `task:created` | New task created |
| `task:updated` | Task state changed |
| `session:started` | Agent session began |
| `session:ended` | Agent session completed |
| `merge:approved` | Entry approved in queue |
| `merge:completed` | Changes merged |

### Filtering

Filter events by:

- **Type** - Specific event type
- **Task** - Events for a specific task
- **Time** - Time range

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

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
