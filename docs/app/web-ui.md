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
| **Tasks** | Task list with filtering and details |
| **Merge Queue** | Review and approve pending changes |
| **Orchestrator** | AI project foreman feed and chat |
| **Events** | Real-time event log viewer |

## Dashboard

The dashboard provides an at-a-glance view of:

- **Operating Mode** - Current mode (Play/Pause/Stop)
- **Active Tasks** - Number of tasks in progress
- **Pending Reviews** - Merge queue items awaiting approval
- **Recent Activity** - Latest events across all tasks
- **Escalations** - Up to 5 recent orchestrator escalation events with task context

### Changing Mode

Click the mode indicator to switch between:

- **Play** - Full autonomy
- **Pause** - Agents work, merges paused
- **Stop** - All activity halted

## Tasks View

### Task List

The task list groups tasks by state and displays them in collapsible sections. Use the tabs at the top to filter:

| Tab | Description |
|-----|-------------|
| **All** | Every task |
| **Active** | Tasks in progress (running, waiting, blocked, etc.) |
| **Done** | Completed, failed, or cancelled tasks |

Each task row shows:

- Priority indicator
- Issue/task ID (e.g. `#42`)
- State icon
- Title
- Labels
- PR link — shown when the task has a submitted pull request (links to GitHub)
- Project
- Last updated time

### Creating a Task

Use the **New Task** button to create a GitHub issue directly from the UI without leaving Tasks. Select a project, enter a title and optional description (Markdown), and add labels. The poller picks up the new issue on its next cycle.

### Filtering

Use the filter controls to narrow down tasks:

- **State** - Filter by task state
- **Project** - Filter by project
- **Search** - Text search in title/description

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

Events appear as context-rich messages including:

- **Decisions** - e.g., "Approving 'Fix login bug' (#42) in owner/repo"
- **Feedback** - Orchestrator comments on work quality
- **Escalations** - Issues requiring human intervention, with actionable context and PR links

### Chat

Type messages in the input field to ask the orchestrator questions or provide direction. The orchestrator responds via the event feed.

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

Use the filter tabs to narrow down the event stream:

| Filter | Description |
|--------|-------------|
| **Important** *(default)* | All events except verbose agent/human messages |
| **All** | Every event, including raw `agent:message` and `human:message` |
| **Task** | Task lifecycle events (`task:*`) |
| **Agent** | Agent session events (`agent:*`) |
| **Merge** | Merge queue events (`merge:*`) |
| **System** | System-level events (`system:*`) |
| **Orchestrator** | Orchestrator events (`orchestrator:*`) |

Use the **Pause** button to freeze the event stream for inspection. Click **Resume** to continue live updates.

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
