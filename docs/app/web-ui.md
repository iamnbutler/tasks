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
| **Containers** | Active container session monitor |
| **Automations** | Reusable automation workflows |

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

The task list shows all tasks with columns for:

- Status (state indicator)
- Title
- Project
- Assignee (if any)
- Created date

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

Filter events by:

- **Type** - Specific event type
- **Task** - Events for a specific task
- **Time** - Time range

## Containers

The Containers view shows all active container sessions running agent workloads.

Each row displays:

| Column | Description |
|--------|-------------|
| **Container ID** | Runtime container identifier |
| **Task** | Link to the associated task |
| **State** | working or idle |
| **Uptime** | How long the session has been running |

The list auto-refreshes every 5 seconds.

## Automations

Automations are reusable agent workflows that can run on a schedule, in response to events, or manually.

### Automation List

Each row shows the automation name, trigger type, current state (active/paused), and a dropdown menu with **View Runs**, **Edit**, and **Delete** options. A **New Automation** button opens the creation dialog.

### State Badges

| Badge | Description |
|-------|-------------|
| **Active** | Automation is enabled and will fire on its trigger |
| **Paused** | Automation is temporarily disabled |
| **Disabled** | Automation is fully disabled |

### Creating / Editing an Automation

The automation form dialog collects:

- **Name** — human-readable label
- **Prompt** — instructions given to the agent when the automation runs
- **Trigger** — one of:
  - **Manual** — only runs when triggered via the API or UI
  - **Schedule** — cron expression (presets: hourly, daily, weekdays daily, custom)
  - **Event** — fires on a specific platform event type
- **Active** toggle — enable or pause the automation at creation time

### Runs Panel

Clicking an automation row (or choosing **View Runs**) opens the runs history panel. Each run shows:

- Color-coded status badge: pending (yellow), running (blue, animated), completed (green), failed (red)
- Relative timestamp with absolute time on hover
- Duration for completed/failed runs
- Collapsible output section and error message (if any)

The panel auto-refreshes every 2 seconds while any run is in progress.

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `?` | Show help |
| `g d` | Go to Dashboard |
| `g t` | Go to Tasks |
| `g m` | Go to Merge Queue |
| `g o` | Go to Orchestrator |
| `g e` | Go to Events |
| `g c` | Go to Containers |
| `g a` | Go to Automations |

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
