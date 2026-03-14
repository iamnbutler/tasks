# Tasks

Status: Draft v1 (TypeScript)

Purpose: Define a human-in-the-loop platform that orchestrates coding agents to get project work
done.

## 1. Problem Statement

Tasks is a platform for collaborating with AI coding agents on real project work. It combines a
long-running server, an AI orchestrator, and a fleet of implementor agents into a system where:

- A human describes work in natural language. The orchestrator decomposes it into well-formed
  issues and dispatches agents to implement them.
- Each task gets an isolated session with its own sandbox, git branch, and agent process. The
  human can drop into any session to steer, answer questions, or add context.
- A merge queue manages the pipeline from completed work to shipped code. The human controls how
  much autonomy the system has — from fully manual review to fully autonomous merging.
- An append-only event system provides a complete audit trail of everything that happens across
  all tasks, agents, and decisions.

The platform solves five operational problems:

- It turns issue execution into a repeatable, observable workflow instead of manual scripts.
- It isolates agent execution in per-task sandboxed workspaces.
- It provides a controllable autonomy model: the human chooses how much to delegate and can
  intervene at any level at any time.
- It gives the human an AI collaborator (the orchestrator) that manages project state, unblocks
  agents, evaluates quality, and makes merge decisions when trusted to do so.
- It keeps a complete, immutable record of all activity through its event system.

Important boundaries:

- Tasks reads from GitHub to discover work. All GitHub writes (comments, labels, state changes,
  PR creation) are performed by agents working inside their sessions.
- The orchestrator is an AI agent, not scheduling logic. The scheduler discovers work; the
  orchestrator manages the project.
- A successful task may end at a workflow-defined handoff state (for example `awaiting_merge`),
  not necessarily a GitHub-closed issue.

## 2. Goals and Non-Goals

### 2.1 Goals

- Provide a human-in-the-loop platform where the human controls the level of autonomy granted
  to the system.
- Present an AI orchestrator that the human can collaborate with via chat or voice to manage
  project work.
- Poll the issue tracker on a fixed cadence and dispatch work with bounded concurrency.
- Create isolated, per-task sessions with sandboxed workspaces and dedicated git branches.
- Manage a merge queue with configurable authority (human, orchestrator, or held).
- Support three operating modes (Stop, Pause, Play) with the orchestrator able to lower the
  mode but only the human able to raise it.
- Maintain a complete audit trail through an append-only event log.
- Expose every session as a persistent chat conversation that any actor can join.
- Recover from transient failures with exponential backoff.
- Support multi-project management across repositories and organizations.
- Keep the agent provider pluggable — the spec defines the session contract, not the agent.

### 2.2 Non-Goals

- Multi-tenant platform or team management. Tasks serves a single human operator.
- Fully autonomous operation without human oversight. The human always controls the autonomy
  level and can intervene at any time.
- General-purpose workflow engine or distributed job scheduler.
- Built-in CI/CD pipeline. Tasks integrates with existing CI but does not replace it.
- Rich issue tracker features (milestones, sprints, boards). GitHub is the system of record
  for project management; Tasks is the execution layer.

## 3. System Overview

### 3.1 Server

The server is the platform. It is the long-running process that everything else runs on.

- Always running. The GUI connects to it on demand.
- Hosts the event log, task state, merge queue, and scheduler.
- Serves a web GUI for human interaction.
- Exposes the orchestrator and task conversations to the GUI over websockets (or equivalent).
- Tracks human presence based on active GUI connections.

### 3.2 Scheduler

The scheduler is responsible for discovering new work and detecting state changes on tracked
issues.

- Polls GitHub on a configurable cadence for issue and PR updates.
- May also accept push notifications from GitHub webhooks when available, with polling as a
  fallback and reconciliation mechanism.
- When new or changed issues are detected, the scheduler emits events into the event bus
  (`system:scheduler:tick`, `task:created`, state changes, etc.).
- The scheduler does not make decisions about what to do with the work — it discovers and queues.
  Dispatch decisions are made by the server's task dispatch logic.

### 3.3 Projects

A project maps to a single repository (initially, for simplicity).

- The server can manage multiple projects across repos and orgs.
- Each project has its own set of tasks, workspace root, and configuration.
- The orchestrator can create tasks in any project the server manages, including directing work
  to a different project when the current one does not cover the needed scope.

### 3.4 External Dependencies

- GitHub API (Issues, PRs, webhooks) for issue tracking.
- Local filesystem for workspaces, event logs, and session state.
- Container runtime for session environments (see session-runtime.md).
- Git CLI for branch and repository operations.
- Coding agent executable (Claude Code initially) that supports chat-style interaction over stdio.
- Host environment authentication for GitHub and the coding agent's AI provider.

## 4. Actors and Roles

Tasks has three actor classes: the human operator, the orchestrator, and implementor agents. They
interact through the server, which is the shared platform all actors operate on.

### 4.1 Human Operator

The human is the project owner. They set direction, make final calls, and can intervene at any level.

Capabilities:

- Talk to the orchestrator directly via chat (or voice) to create work, give direction, or ask
  questions about project state.
- Drop into any individual task conversation to steer an implementor agent, answer its questions,
  or add context.
- Review the merge queue: inspect pending merges, leave notes, approve or reject.
- Control the operating mode (play, pause, stop).

Presence:

- The server tracks whether a GUI client is connected.
- Connected = the human is present. The orchestrator and agents may surface questions and expect
  timely responses.
- Disconnected = autonomous mode. The orchestrator makes judgment calls instead of waiting on the
  human. Questions that would normally surface to the human are either resolved by the orchestrator
  or parked until the human returns.

### 4.2 Orchestrator

The orchestrator is an AI agent that manages the project. It may be one or more agents under the
hood, but presents as a single entity to the human and to implementors.

Responsibilities:

- **Triage and decomposition.** When the human describes work in natural language, the
  orchestrator turns that into well-formed issues — organized, contextualized, and split into
  sub-issues as needed.
- **Unblocking agents.** When an implementor agent is stuck and has a question, it can ask the
  orchestrator. The orchestrator either answers directly (using project context), or escalates to
  the human if present or if the question is important enough to warrant waiting.
- **Quality gate.** The orchestrator evaluates whether an implementation meets the quality bar for
  the task. This is the checkpoint before work enters the merge queue.
- **Merge authority (delegated).** In Play mode, the orchestrator owns merge decisions
  continuously — reviewing pending merges and shipping them as they pass quality evaluation.
  In Pause/Stop, merge authority is held (though the human can Flush approved items in Pause).
- **Surfacing information.** The orchestrator proactively nudges the human about important
  decisions, blockers, or choices that are holding up downstream work — but only when the human
  is around or when the issue is important enough to notify asynchronously.

The orchestrator does not generally execute code or make file changes directly — it operates
through implementor agents and the issue tracker. However, this is not a hard constraint. The
human may ask the orchestrator to perform small tasks directly when that is more expedient than
creating a session.

### 4.3 Implementor Agents

Implementor agents are the workers. Each one is assigned a task and gets an isolated workspace.

Characteristics:

- The agent provider is an implementation detail (Claude Code initially, but pluggable).
- Each agent runs inside a session, which owns a sandboxed copy of the repo on a real git branch.
- Agents do not interact with the Tasks event system directly. The session wrapper monitors the
  agent's output and emits events to the bus on its behalf.
- When stuck, the session detects this and can escalate to the orchestrator for guidance.
- Every session is a chat conversation: the human can drop in at any time to steer, answer
  questions, or add context. The orchestrator can do the same.
- The spec defines the session and task lifecycle, not the agent's internal behavior.

### 4.4 Interaction Patterns

```
Human <--chat/voice--> Orchestrator
                            |
                     delegates / unblocks / evaluates
                            |
                   Sessions (1 per active task)
                    [sandbox + agent + chat + event emitter]
                            |
                     events emitted to bus
                            |
                        Server (platform)
                            |
                     UI, merge queue, event log
```

The human primarily interacts with the orchestrator. Direct interaction with sessions is
available for steering or answering questions, but the default flow is autonomous.

## 5. Domain Model

### 5.1 Task

A task is the internal representation of a unit of work. It may originate from a GitHub issue,
a GitHub PR, or be created by the orchestrator.

Fields:

- `id` (string) — internal task ID
- `source` (object) — origin reference (GitHub issue ID/number, PR ID/number, or internal)
- `title` (string)
- `description` (string or null)
- `state` (string) — current task state (see 5.2)
- `parent_id` (string or null) — parent task ID, if this is a sub-task
- `blocked_by` (list of task IDs) — tasks that must complete before this one can proceed
- `project` (string) — project ID this task belongs to
- `labels` (list of strings)
- `priority` (integer or null) — lower numbers are higher priority
- `session_id` (string or null) — active session ID, if any
- `workspace_id` (string or null) — workspace ID, if provisioned
- `created_at` (timestamp)
- `updated_at` (timestamp)

### 5.2 Task States

- `waiting` — no agent slot available / max concurrency reached
- `blocked` — waiting on another task to finish
- `running` — agent is actively working
- `question` — agent is waiting on human or orchestrator for input
- `testing` — agent done, CI/deterministic testing running
- `awaiting_merge` — implementation complete, in merge queue
- `conflict` — merge conflict needs resolution
- `completed` — task finished successfully
- `failed` — task failed
- `cancelled` — task was cancelled

### 5.3 Task Hierarchy

Tasks can have parent/child relationships. This is primarily an organizational tool — a way to
break a large issue into smaller pieces of work.

- Sub-tasks are dispatched independently. A parent task does not implicitly block on its children.
- A sub-task that gets implemented may produce a PR, a comment on the parent issue, an update to
  the parent task's state, or any combination.
- Explicit blocking relationships can exist between any tasks (not just parent/child). Blocking
  is typically emergent: an agent working on a task recognizes that it depends on work that hasn't
  been done yet and emits a `task:state:blocked` event referencing the blocking task.
- When a blocking task completes, blocked tasks should be re-evaluated for dispatch.
- Parent tasks can subscribe to their children's event streams to track progress.

### 5.4 Session

_See Section 9 for full session specification._

Fields:

- `id` (string) — session ID
- `task_id` (string) — the task this session is executing
- `workspace_path` (string) — path to the sandboxed workspace
- `branch` (string) — git branch name
- `status` (string) — session status (starting, running, completed, failed, terminated)
- `started_at` (timestamp)
- `ended_at` (timestamp or null)

### 5.5 Merge Queue Entry

_See Section 7 for full merge queue specification._

Fields:

- `id` (string) — queue entry ID
- `task_id` (string)
- `pr_url` (string or null)
- `status` (string) — pending, approved, rejected, merged, conflict
- `queued_at` (timestamp)

### 5.6 Project

Fields:

- `id` (string)
- `repo` (string) — repository reference (owner/repo)
- `default_branch` (string) — typically `main`
- `config` (object) — project-level configuration

## 6. Operating Modes

The system operates in one of three modes. The current mode controls merge queue behavior.
Agents are dispatched and work normally in all modes except Stop.

### 6.1 Stop

- No new work is dispatched.
- Running agent processes are terminated. The sandbox and git branch persist, but in-flight
  agent work is lost. When the system resumes, affected sessions restart from scratch.
- The merge queue is held.
- The system is idle until the human resumes.

### 6.2 Pause

- Agents are dispatched and work on tasks normally.
- Completed work enters the merge queue.
- The merge queue is held: nothing merges automatically.
- The orchestrator continues to manage agents, answer questions, and evaluate quality — but does
  not approve merges.
- Pause is the typical review state. The human reviews the queue, triages pending items, leaves
  feedback on individual tasks, and once satisfied, either flushes approved items or switches
  to Play.
- The **Flush** action is available in Pause: it pushes through everything currently approved in
  the queue. The system remains in Pause afterward.

### 6.3 Play

- Agents are dispatched and work on tasks normally.
- The merge queue is continuously active: as work completes, passes quality evaluation, and is
  approved by the orchestrator, it merges automatically.
- The orchestrator owns merge authority. The human can still intervene at any time.
- Play is the fully autonomous mode. The human delegates merge authority to the orchestrator and
  may step away.

### 6.4 Mode Transitions

Mode transitions follow a severity ordering: Stop < Pause < Play.

- The human can change the mode in any direction.
- The orchestrator can lower the mode (for example, Play -> Pause if something goes wrong), but
  only a human can raise it.
- This ensures the system can protect itself, but only the human can grant more autonomy.
- Transitions take effect immediately for new dispatches and merge decisions.

## 7. Merge Queue

The merge queue is the pipeline between "an agent finished its work" and "that work ships."

### 7.1 Queue Entry Lifecycle

1. An implementor agent completes its task and produces a result (typically a PR).
2. The orchestrator evaluates whether the implementation meets the quality bar and appropriately
   resolves the issue.
3. If approved, the work enters the merge queue as a pending merge.
4. The merge authority (human or orchestrator, depending on mode and presence) reviews and either
   merges or rejects.
5. If rejected, the task may be sent back to the implementor with feedback.

### 7.2 Merge Authority

Merge authority determines who approves merges from the queue.

| Mode  | Merge authority                                          |
|-------|---------------------------------------------------------|
| Stop  | Nobody (held)                                           |
| Pause | Nobody (held). Human triages and reviews. Flush available |
| Play  | Orchestrator (continuous). Human can override any time    |

The human always has the ability to intervene — reject a merge, pull something out of the queue,
or drop into a task to give feedback. The mode controls the default flow, not the human's access.

### 7.3 Quality Evaluation

Before a task enters the merge queue, the orchestrator evaluates it:

- Does the implementation address the issue as described?
- Do tests pass (CI/testing state)?
- Are there conflicts that need resolution?
- Does the change meet project conventions and quality standards?

If the orchestrator determines the work isn't ready, it sends the task back to the implementor
with specific feedback rather than queuing a bad merge.

### 7.4 Conflicts

When a pending merge has conflicts:

- The task transitions to the `conflict` state.
- The merge remains in the queue but is not eligible until the conflict is resolved.
- The orchestrator triages the conflict:
  - In Play mode: the orchestrator resolves the conflict autonomously — typically re-engaging the
    implementor agent, or resolving it directly when the resolution is mechanical (rebases,
    trivial merge conflicts).
  - In Pause mode or when the human is present: the orchestrator surfaces non-trivial conflicts
    to the human for guidance. Mechanical conflicts are resolved by the orchestrator directly.

## 8. Event System

Tasks uses an append-only event log as the backbone for all communication between components.
Agents, the orchestrator, the scheduler, and the human all produce events. The UI, orchestrator,
and parent tasks consume them.

### 8.1 Design

- All events are immutable. Once written, an event is never modified or deleted.
- Events are persisted to an append-only log, stored per-task (one log file per task).
- A lightweight in-memory pub/sub layer sits in front of the log for live subscriptions.
- Consumers can subscribe to live events and replay historical events from the log.

### 8.2 Event Shape

Every event has the same base structure:

- `id` (string) — unique event ID
- `type` (string) — colon-delimited event type (see 8.3)
- `task` (string) — task ID this event belongs to
- `actor` (string) — who produced this event: `human`, `orchestrator`, `scheduler`, `agent`,
  or `system`
- `ts` (timestamp) — when the event occurred
- `data` (object) — event-type-specific payload

### 8.3 Event Types

Task events:

- `task:created` — a new task exists
- `task:state:running` — agent is actively working
- `task:state:question` — agent is waiting on human or orchestrator for input
- `task:state:waiting` — no agent slot available / max concurrency reached
- `task:state:blocked` — waiting on another task to finish
- `task:state:testing` — agent done, CI/deterministic testing running
- `task:state:awaiting_merge` — implementation complete, in merge queue
- `task:state:conflict` — merge conflict needs resolution
- `task:state:completed` — task finished successfully
- `task:state:failed` — task failed
- `task:state:cancelled` — task was cancelled

Agent events:

- `agent:message` — agent emitted a status update, progress note, or response
- `agent:question` — agent is asking for help (triggers `task:state:question`)
- `agent:error` — something went wrong inside the agent session

Merge events:

- `merge:queued` — work entered the merge queue
- `merge:approved` — approved for merge
- `merge:rejected` — sent back with feedback
- `merge:completed` — actually merged
- `merge:conflict` — conflict detected

Orchestrator events:

- `orchestrator:feedback` — orchestrator sent feedback to an agent
- `orchestrator:escalation` — orchestrator surfaced something to the human
- `orchestrator:decision` — orchestrator made a judgment call

System events:

- `system:started` — server started
- `system:mode:play` — mode changed to Play
- `system:mode:pause` — mode changed to Pause
- `system:mode:stop` — mode changed to Stop
- `system:flush` — merge queue flush triggered
- `system:config:reloaded` — configuration was reloaded
- `system:scheduler:tick` — scheduler polled for updates

### 8.4 Subscriptions

Consumers subscribe to events using colon-delimited patterns with wildcard support:

- `task:*` — all task events
- `task:state:*` — all state changes
- `agent:*` — all agent communication
- `merge:completed` — just that one event type

A parent task can subscribe to events from its child tasks by task ID. Any consumer can subscribe
to any task's event stream. The bus handles routing — tasks never communicate directly with each
other.

### 8.5 Storage and Cleanup

- Events are stored per-task as append-only files (e.g., `<task-id>/events.jsonl`).
- A global index or secondary log may be maintained for cross-task queries.
- Cleanup policy is configurable:
  - Completed/cancelled task logs can be archived or deleted after a retention period.
  - Active task logs are never pruned.

## 9. Sessions and Agent Runner

A session is the unit of execution. When the server dispatches a task for implementation, it
creates a session. The session owns everything needed to work on that task: an isolated runtime
environment with its own copy of the repo, a git branch, an agent process, a chat history, and
an event emitter.

The session runtime is a multi-process environment — not just an agent process. It hosts the
agent, a supervisor that manages the agent's lifecycle, and any processes the agent spawns
(test runners, build tools, git operations). See session-runtime.md for the runtime
architecture.

### 9.1 Session Lifecycle

1. **Creation.** Dispatch logic determines a task needs work (implementation, comment, sub-issue
   creation, etc.). A new session is created for that task.
2. **Runtime setup.** The session provisions an isolated runtime environment and workspace
   (see Section 10) with its own copy of the repo checked out to a dedicated git branch.
3. **Agent launch.** An agent process is started inside the runtime. The session sends a custom
   prompt to the agent based on the task's details (issue description, context, relevant project
   information).
4. **Agentic flow.** The agent works autonomously. The session wrapper monitors the agent's
   output, interprets what's happening, and emits events to the bus.
5. **Completion.** The agent finishes (task done, error, or killed). The session emits final
   state events. The runtime environment and git branch persist for review or re-use.

### 9.2 Session as Chat

Every session is a chat conversation. This is the universal interface.

- The agent's output appears as messages in the chat.
- The human can join the chat at any time to send messages, steer the agent, answer questions,
  or add context.
- The orchestrator can also join to provide guidance or unblock the agent.
- Most sessions run without external participants. The chat interface is always available
  regardless of whether anyone joins.
- Chat history is persistent. The human can review what happened in any session after the fact.

### 9.3 Event Emission

The agent itself does not know about the Tasks event system. The session wrapper is responsible
for observing the agent's behavior and emitting appropriate events.

- The session reads the agent's output stream and interprets status.
- Some events are direct mappings (agent produced output -> `agent:message`).
- Some events are inferred from context (agent is waiting for user input with choices ->
  `agent:question` + `task:state:question`).
- The session emits all events to the bus on behalf of the agent.

### 9.4 Agent Provider

The agent provider is an implementation detail. Claude Code is the initial provider, but the
session contract is designed to be provider-agnostic.

The session needs the agent provider to support:

- Starting a chat session with an initial prompt in a given working directory.
- Streaming output (so the session can monitor and emit events).
- Accepting human/orchestrator messages as chat input.
- Being terminated gracefully.

### 9.5 Pause, Resume, and Interruption

For the initial implementation, the model is simple:

- **Stop mode:** Running agent processes are terminated. When the system returns to Pause or
  Play, sessions are restarted from scratch with the full task prompt. Work in progress is lost,
  but the git branch and workspace persist so the agent can pick up from the repo state.
- **Human/orchestrator drops in:** The message is delivered to the agent's chat as a normal chat
  message. The agent responds as part of its ongoing flow. If the agent has already finished,
  the session is restarted with context that includes the new message.
- **Future improvement:** Resume agent sessions where they left off rather than restarting. This
  depends on agent provider support and is not required for the initial implementation.

### 9.6 One Session Per Task

A task has at most one active session at a time. If a task needs to be re-run (retry, restart
after Stop, feedback from human), the previous session is ended and a new one is created in the
same sandbox/branch.

Previous session chat history remains accessible for context and audit.

## 10. Workspace Management

### 10.1 Workspace Creation

When a session is created, it provisions an isolated workspace:

- The workspace is created inside an isolated runtime environment provisioned by a configurable
  container provider. The runtime provides process isolation, filesystem isolation, and a
  multi-process environment for the agent and its subprocesses. See session-runtime.md for
  provider details.
- The workspace gets its own copy of the repository, cloned inside the runtime environment. The
  copy method (shallow clone, full clone, etc.) is configurable.
- A new git branch is created off of `main` (or the project's default branch) unless the task
  is explicitly stacking on another in-progress branch.
- The branch is initially named with a generated ID (UUID or similar). It may be renamed once
  work starts or when a PR is created, to something human-readable based on the task.

### 10.2 Workspace Reuse

- A workspace persists across session restarts for the same task. If a session is killed (Stop
  mode) and restarted, the new session reuses the existing workspace and branch — the agent
  starts fresh but the repo state reflects prior work.
- One workspace per task. If a session happens to address multiple tasks, the workspace belongs
  to the primary task.

### 10.3 Cleanup

Workspaces are cleaned up when they are no longer needed:

- **PR merged:** The related workspace is deleted.
- **Task completed/cancelled:** The workspace is eligible for cleanup.
- **Stale/idle:** Workspaces with no active session for a configurable period are eligible for
  cleanup.
- Chat history and event logs are retained independently of workspace cleanup — deleting a
  workspace does not delete the session's history.

## 11. Issue Tracker Integration

### 11.1 GitHub as Source of Truth

GitHub Issues and PRs are the external source of work. Tasks reads from GitHub to discover and
track work, but does not write back — all GitHub mutations (comments, labels, state changes,
PR creation) are performed by agents working inside their sessions.

### 11.2 What Gets Tracked

The scheduler monitors both issues and pull requests:

- **Issues** are the primary source of tasks (implement this, fix that, explore this).
- **PRs** can also be a source of tasks (review this PR, resolve conflicts, fit this into the
  merge queue).

For each issue/PR, the scheduler reads all available fields:

- Title, body, labels, assignees, milestone
- Comments (full history)
- Sub-issues / linked issues
- Linked PRs (for issues) or linked issues (for PRs)
- Open/closed state
- Timestamps (created, updated)

### 11.3 State Mapping

Tasks owns its own internal state (Section 5.1) independently of GitHub's open/closed status.

- A GitHub issue being open is a precondition for creating a task, but after that, task state
  is managed internally based on session activity, merge queue progress, etc.
- When a GitHub issue is closed externally, the corresponding task should be cancelled or
  completed (depending on context).
- Tasks does not push its internal states back to GitHub labels or issue fields. Progress is
  communicated through agent-authored comments and PR activity.

### 11.4 Discovery

The scheduler discovers new and changed work through:

- **Polling:** Periodic check for new/updated issues and PRs on a configurable cadence.
- **Webhooks (optional):** GitHub webhooks can push issue/PR events to the server for faster
  response. Polling remains as a fallback and reconciliation mechanism.

### 11.5 Normalization

The scheduler normalizes GitHub payloads into a stable internal model before emitting events.
This keeps the rest of the system decoupled from GitHub-specific API shapes.

See github.md for the full normalized model, GraphQL query design, client API, polling interface,
and testing strategy.

## 12. Scheduling and Dispatch

The dispatch system determines which tasks get worked on and when. The scheduler discovers work
(§3.2, §11); the dispatcher decides what to run.

### 12.1 Dispatch Loop

The dispatcher is triggered in two ways:

**Event-driven dispatch.** The dispatcher evaluates immediately when any of these events fire:

- `task:created` — new task is available
- `task:state:completed`, `task:state:failed`, `task:state:cancelled` — a slot freed up
- `task:state:waiting` — a blocked task became unblocked
- A `question`-state task receives an answer (human or orchestrator message)
- `system:mode:pause`, `system:mode:play` — mode changed to one that allows dispatch

**Reconciliation tick.** A periodic sweep (configurable, default 30 seconds) runs the same dispatch
logic. This catches missed events, stuck states after restarts, and race conditions in event
processing.

Both triggers invoke the same dispatch evaluation function. The function is idempotent — running it
multiple times in quick succession is harmless.

**Mode gate.** Dispatch is only active in Pause and Play modes. In Stop mode, the dispatch function
returns immediately. When transitioning from Stop to Pause or Play, the reconciliation tick
triggers a full evaluation.

### 12.2 Candidate Selection

The dispatcher divides actionable tasks into two pools:

**Resume candidates.** Tasks with existing sessions that need re-engagement:

- Tasks in `question` state that have received an answer. These already hold a session slot —
  resuming them is free from a concurrency perspective. The dispatcher sends the message to the
  existing session immediately.
- Tasks in `blocked` state that have become unblocked (all `blocked_by` tasks are in terminal
  states). These transition to `waiting` and are dispatched with their existing workspace and
  branch.

Resume candidates are always processed before new work candidates. They represent in-progress work
closer to completion and are cheaper to start.

**New work candidates.** Tasks in `waiting` state with no active session. These require a new
session, container, and workspace.

### 12.3 Prioritization

Within each pool, candidates are sorted by:

1. **Explicit priority.** Lower `priority` number first. Tasks with null priority sort after all
   explicitly prioritized tasks.
2. **Unblocking value.** Tasks that appear in other tasks' `blocked_by` lists sort before tasks
   that don't. This favors completing work that unblocks downstream tasks.
3. **Recency.** Among otherwise equal tasks, newer tasks (`created_at` descending) go first.
   In practice, older tasks in a backlog tend to be lower-priority or complex items; active work
   is recent.

The orchestrator influences dispatch indirectly by setting task priorities and creating or
cancelling tasks, not by participating in the dispatch loop itself. This keeps dispatch fast and
deterministic.

### 12.4 Concurrency Limits

Two limits control how many tasks can run simultaneously:

- **Global limit** (`max_sessions`, required). Total active sessions across all projects. This is
  the primary resource constraint — each session is a container consuming host CPU and memory.
  Default: 5.
- **Per-project limit** (`max_sessions` on project config, optional). Prevents one project from
  consuming all available slots. Defaults to the global limit if unset.

### 12.5 Slot Accounting

A task holds a slot from when its session is created until the session ends:

- `running` — holds a slot (agent actively working)
- `question` — holds a slot (session is live, container running, agent waiting)
- `testing` — holds a slot (CI may be running inside the container)
- `waiting`, `blocked` — does not hold a slot
- `awaiting_merge`, `conflict` — does not hold a slot (session has ended, work is complete)
- Terminal states (`completed`, `failed`, `cancelled`) — does not hold a slot

The dispatcher counts slots by counting tasks in slot-holding states, not by tracking session
objects. This keeps the accounting simple and derivable from task state.

### 12.6 Dispatch Evaluation

On each evaluation, the dispatcher:

1. Checks mode. If Stop, return immediately.
2. Processes all resume candidates (free — no slot cost for `question` answers).
3. Counts active slots globally and per-project.
4. Collects new work candidates, sorted by priority rules (§12.3).
5. For each candidate in order: if both global and project slot limits have room, create a session
   and start the task. Otherwise, the task remains in `waiting`.

## 13. Retry and Recovery

### 13.1 Failure Classes

The system categorizes failures to determine the appropriate response:

**Transient failures.** Temporary problems likely to resolve on their own:
- Network errors (GitHub API timeouts, DNS failures)
- Container startup failures (resource pressure, daemon hiccups)
- Agent process crashes (non-deterministic, may succeed on retry)

Response: Retry with exponential backoff.

**Deterministic failures.** Problems that will recur if retried with the same inputs:
- Agent repeatedly fails on the same task (same error multiple times)
- Invalid task configuration (missing repo, bad branch)
- Authentication failures (expired token, revoked access)

Response: Mark the task as `failed`, emit an event, surface to the orchestrator or human. Do not
retry automatically.

**Infrastructure failures.** The host or server itself has a problem:
- Server crash and restart
- Container runtime unavailable
- Disk full

Response: Recover on restart via reconciliation (§13.3).

The system distinguishes transient from deterministic failures using a **retry counter per task**.
If a task fails and is retried N times (configurable, default: 3) without making progress, it is
reclassified as deterministic and marked `failed`.

"Making progress" means the agent produced commits, changed task state, or ran for longer than a
minimum duration (configurable, default: 60 seconds). A task that crashes immediately on start 3
times in a row is deterministic. A task that runs for 10 minutes and then hits an edge case is
still worth retrying.

### 13.2 Retry Behavior

**Exponential backoff.** When a transient failure occurs, the system retries with increasing
delays:

- Base delay: 5 seconds
- Multiplier: 2x per attempt
- Maximum delay: 5 minutes
- Jitter: ±25% (prevents thundering herd when multiple tasks fail simultaneously)

Sequence: ~5s, ~10s, ~20s, ~40s, ~80s, capped at ~300s.

**Retry scope.** Retries apply at two levels:

- **API retries.** GitHub API calls, container runtime commands, and other infrastructure
  operations retry transparently within the client code. The caller does not see transient failures
  unless retries are exhausted. Max attempts: 3.
- **Task retries.** When an agent session fails (agent crashes, container dies), the dispatcher
  can restart the session for the same task. The workspace and branch persist, so the new session
  picks up from the repo state. Max attempts: 3 (configurable per project).

**Retry state.** Each task tracks:

- `retry_count` (integer) — number of times this task has been retried
- `last_failure_at` (timestamp or null) — when the most recent failure occurred

These fields are used by the dispatcher to calculate backoff delay. A task whose
`last_failure_at` plus its current backoff interval is still in the future is not eligible for
dispatch.

**Retry vs. new session.** A retry creates a new session in the existing workspace. The agent
starts fresh but the repo state (commits, branch) reflects prior work. This is the same behavior
as restarting after Stop mode (§9.5).

### 13.3 Restart Recovery

When the server restarts, it reconciles its in-memory state with persistent state:

1. **Reload task state.** Tasks are persisted (event log is the source of truth). Replay each
   task's event stream to reconstruct current state.
2. **Detect orphaned sessions.** Tasks in `running` or `question` state may have had their
   agent process killed by the restart. The server checks whether each session's container is
   still alive.
3. **Recover or fail.** For tasks with dead sessions:
   - If `retry_count` < max retries, transition to `waiting` with an incremented retry count.
     The dispatcher picks them up on the next evaluation.
   - If retries exhausted, transition to `failed`.
4. **Resume the dispatch loop.** The reconciliation tick fires, evaluating all `waiting` tasks.

Container state may persist across server restarts (the containers are independent processes).
If a container is still running, the server re-attaches to its stdio and resumes the session
without restarting the agent.

### 13.4 Failure Surfacing

When a task fails (retries exhausted or deterministic failure):

- A `task:state:failed` event is emitted with failure details in the event data.
- If the human is present, the orchestrator surfaces the failure in conversation.
- If the human is absent, the failure is logged and visible in the UI on return.
- The orchestrator may attempt to diagnose the failure and suggest a course of action (retry with
  different parameters, break the task into smaller pieces, escalate to the human).

## 14. Workflow Configuration

Each project can customize how tasks are handled through a workflow configuration file in the
repository.

### 14.1 Configuration File

The workflow configuration lives at `workflow.toml` in the project's repository root. This
file is read when the project is added to the server and can be reloaded dynamically.

```toml
[project]
max_sessions = 3                # Per-project concurrency limit (§12.4)
default_branch = "main"         # Override project default branch

[dispatch]
max_retries = 3                 # Task retry limit (§13.2)
retry_base_delay = 5            # Base backoff delay in seconds
progress_threshold = 60         # Minimum runtime (seconds) to count as "progress" (§13.1)

[labels]
# Map GitHub labels to task behavior.
# Tasks with "blocked" label start in blocked state.
# Tasks with "ignore" label are not imported.
ignore = ["wontfix", "duplicate", "ignore"]
blocked = ["blocked", "waiting-on-external"]

[prompt]
# Path to a system prompt file included in every agent session for this project.
# Relative to repo root.
system_prompt = "system-prompt.md"
```

### 14.2 Label Mapping

The `[labels]` section controls how GitHub labels affect task behavior:

- **ignore:** Issues with any of these labels are skipped during import. The scheduler does not
  create tasks for them.
- **blocked:** Issues with any of these labels start in `blocked` state instead of `waiting`.

Labels not listed in the configuration have no special meaning to the dispatch system. The
orchestrator and human can still use them for their own organizational purposes.

### 14.3 Dynamic Reload

The server watches for configuration changes:

- When the configuration file changes (detected via polling the repo or webhook), the server
  reloads it and emits a `system:config:reloaded` event.
- Active sessions are not affected — configuration changes apply to newly created sessions and
  future dispatch decisions.
- Invalid configuration is rejected with a warning. The previous valid configuration remains in
  effect.

## 15. Prompt Construction

When a session starts, the server constructs a prompt for the agent based on the task's details
and project context. The prompt is the agent's entire understanding of what it needs to do.

### 15.1 Prompt Structure

The prompt is assembled from several layers, concatenated in order:

1. **System prompt (project-level).** The contents of the file referenced by
   `[prompt].system_prompt` in the workflow configuration (§14.1). This typically contains
   project conventions, coding standards, and repository-specific context. If not configured,
   this layer is omitted.

2. **Task description.** The core of the prompt — what the agent needs to do:
   - Issue/PR title and body
   - Comments: the first 10 and last 10 comments, chronologically ordered. If there are more
     than 20 comments, a note is inserted between the two groups indicating how many were omitted
     and that the agent can use `gh` CLI to fetch the full history.
   - Labels and assignees
   - Sub-issues (titles and states, for context)
   - Linked PRs or issues (titles and states)

3. **Task context.** Additional context the server provides:
   - Parent task details (if this is a sub-task)
   - Related task summaries (tasks in the same project that are in progress or recently completed,
     to help the agent avoid conflicts)
   - The git branch name and whether prior work exists on it

4. **Behavioral instructions.** Instructions that control how the agent operates:
   - Commit and push work to the branch when done
   - Do not merge — the merge queue handles that
   - If stuck, describe the problem clearly so the orchestrator or human can help
   - If the task is ambiguous, ask for clarification rather than guessing

### 15.2 Retry and Continuation Context

When a task is being retried (§13.2), additional context is prepended:

- A note that this is a retry, not a first attempt
- The previous session's failure mode (crash, error message, timeout)
- What progress was made (commits on the branch, if any)
- Guidance to try a different approach if the previous one failed

When a task receives a human or orchestrator message while in `question` state, the message is
delivered via the session's chat interface (§9.2), not by reconstructing the prompt.

### 15.3 Prompt Rendering

The prompt is rendered as plain Markdown. No template engine — the server concatenates the
sections with clear headings. This keeps the system simple and the prompts inspectable.

```markdown
# Project Context

{contents of system-prompt.md}

# Task

**{title}** (#{number})

{body}

## Comments

**{author}** ({timestamp}):
{comment body}

... (showing first 10 and last 10 of {total} comments — use `gh issue view {number} --comments` for full history)

**{author}** ({timestamp}):
{comment body}

## Context

- Branch: `tasks/{task-id}`
- Parent task: #{parent_number} — {parent_title}
- Related in-progress tasks: #{n1} — {title1}, #{n2} — {title2}

## Instructions

- Work on the branch `tasks/{task-id}`. Commit and push your changes when done.
- Do not merge into main. The merge queue handles merging.
- If you are stuck or the task is ambiguous, describe the problem clearly.
```

## 16. Observability

### 16.1 Structured Logging

All server components emit structured log entries (JSON) with consistent fields:

- `ts` — timestamp
- `level` — trace, debug, info, warn, error
- `component` — which subsystem (scheduler, dispatcher, session, merge_queue, orchestrator)
- `task_id` — if the log relates to a specific task
- `session_id` — if the log relates to a specific session
- `message` — human-readable description
- `data` — additional structured data

Logs are written to stdout and optionally to a file. The log level is configurable at startup
and can be changed at runtime.

### 16.2 GUI Dashboard

The web GUI (§3.1) provides a real-time view of system state:

- **System status.** Current operating mode, active session count, slot utilization.
- **Task list.** All tasks with current state, priority, and session status. Filterable by
  project, state, and label.
- **Session view.** For each active session: agent output stream (live), chat history, task
  details, git branch status.
- **Merge queue.** Pending, approved, and recently merged items. Review and approve/reject
  from the UI.
- **Event stream.** Live feed of events across all tasks, filterable by type and task.
- **Orchestrator chat.** Persistent conversation with the orchestrator.

### 16.3 Runtime Snapshots

The server exposes a snapshot endpoint (HTTP GET) that returns the full system state as JSON:

- All tasks and their current states
- All active sessions and their statuses
- Merge queue contents
- Current operating mode
- Rate limit state for each project's GitHub connection
- Slot utilization (active / max, global and per-project)

This is useful for debugging, monitoring integrations, and the GUI's initial page load.

### 16.4 Token and Cost Accounting

The server tracks resource consumption per task and per project:

- **Agent tokens.** Input and output token counts per session, sourced from agent output parsing
  (agent-provider-specific). Accumulated per task and per project.
- **API calls.** GitHub API calls and rate limit point consumption per project per polling cycle.
- **Session duration.** Wall-clock time per session, from creation to termination.
- **Container resources.** CPU and memory utilization per session (if available from the container
  runtime).

Accounting data is stored as events (`system:accounting:*`) and surfaced in the GUI dashboard.
Cost estimation (mapping tokens to dollars) is not built in — the accounting provides the raw
numbers, and the human can interpret them with their provider's pricing.

## 17. Security and Safety

### 17.1 Workspace Isolation

Session isolation is provided by the container runtime (session-runtime.md §2):

- Each session runs in its own lightweight VM (apple/container).
- Processes in one session cannot see or affect processes in another.
- Each session has its own filesystem. No shared mounts between sessions.
- The host filesystem is not accessible from inside containers.

### 17.2 Secret Handling

Secrets are injected into containers as environment variables at creation time
(session-runtime.md §3.1):

- `GITHUB_TOKEN` — for git operations and `gh` CLI.
- Agent-provider API keys (e.g., `ANTHROPIC_API_KEY`).

Security properties:

- Secrets are never written to disk inside the container (environment variables only).
- Secrets are not included in event logs, task state, or any persisted data.
- Secrets are not passed through the supervisor protocol — they are set at container creation
  and available to all processes inside the container.
- Each project can use a different GitHub token with scoped permissions (e.g., repo-level
  fine-grained PAT).

Secrets are configured on the server side (environment variables, config file, or secret
manager). The server reads them and passes them to the container runtime at session creation.
The mechanism for configuring secrets on the server is an operational concern, not specified here.

### 17.3 Trust Boundaries

The system has three trust boundaries:

1. **Host ↔ Container.** The container is untrusted from the host's perspective. The agent can
   execute arbitrary code inside the container, but cannot affect the host. Communication is
   limited to the supervisor protocol over stdio.

2. **Server ↔ GitHub.** The server reads from GitHub using authenticated API calls. GitHub is
   trusted as the source of truth for issues and PRs. The server does not write to GitHub
   directly — all mutations happen through agents inside containers.

3. **Server ↔ Agent provider.** API keys for the agent's AI provider are passed into containers.
   The server trusts the agent provider's API but limits exposure by scoping keys to the minimum
   required permissions where possible.

### 17.4 Agent Sandboxing

Agents run inside containers with the following constraints:

- **Network access.** Agents have unrestricted network access. They need it for git operations,
  package installation (npm, cargo, pip), AI provider APIs, and potentially browsing
  documentation. Network restriction is not enforced at the container level.
- **Filesystem.** Agents can read and write anywhere inside their container. The container's
  filesystem is ephemeral and isolated — nothing persists beyond the container's lifetime except
  git pushes.
- **Process execution.** Agents can spawn arbitrary processes inside their container (build tools,
  test runners, language servers). This is required for them to do their job.
- **Resource limits.** CPU and memory limits are set at container creation (session-runtime.md
  §2.1). Default limits are configurable per project.
- **Time limits.** Sessions have a soft limit and a hard limit on wall-clock duration:
  - **Soft limit** (configurable, default: 1 hour). When reached, the server nudges the
    orchestrator or human that the session is running long. The orchestrator may intervene
    (provide guidance, break the task into smaller pieces) or the human may extend or steer.
  - **Hard limit** (soft limit + 15 minutes). If no intervention occurs after the nudge, the
    session is terminated and the task is retried or failed per §13.

The sandboxing model is: give agents everything they need to do their work, but contain the blast
radius to a single disposable VM.

## 18. Reference Algorithms

### 18.1 Dispatch Tick

```
function dispatch_tick(server):
    if server.mode == Stop:
        return

    # Phase 1: Resume candidates (free — no slot cost).
    for task in server.tasks where task.state == Question:
        if task has pending message:
            send message to task.session
            set task.state = Running

    # Phase 2: Unblock tasks whose dependencies completed.
    for task in server.tasks where task.state == Blocked:
        if all tasks in task.blocked_by are terminal:
            set task.state = Waiting

    # Phase 3: Dispatch new work.
    candidates = server.tasks
        where state == Waiting
        and retry_backoff_elapsed(task)
        sorted by priority_sort(task)

    for task in candidates:
        global_slots = count(server.tasks where state in {Running, Question, Testing})
        project_slots = count(server.tasks
            where project == task.project
            and state in {Running, Question, Testing})

        if global_slots >= server.max_sessions:
            break
        if project_slots >= task.project.max_sessions:
            continue

        session = create_session(task)
        prompt = build_prompt(task)
        session.start(prompt)
        set task.state = Running
        emit task:state:running
```

### 18.2 Session Lifecycle

```
function create_session(task):
    container = runtime.create(task.project.image, task.project.env)
    runtime.start(container)
    transport = runtime.attach(container)

    wait for system:ready event on transport

    session = Session {
        id: new_uuid(),
        task_id: task.id,
        container: container,
        transport: transport,
        status: Ready,
    }

    task.session_id = session.id
    return session

function start_session(session, prompt):
    send start command {
        repo: session.task.project.repo_url,
        branch: session.task.branch,
        prompt: prompt,
    } over session.transport

    # Monitor agent output.
    loop:
        event = session.transport.recv()
        match event:
            agent:started -> emit task:state:running
            agent:stdout  -> emit agent:message, check for question patterns
            agent:stderr  -> log warning
            agent:exit(0) -> emit task:state:testing or task:state:awaiting_merge
            agent:exit(n) -> handle_failure(session, exit_code=n)

function handle_failure(session, exit_code):
    task = session.task
    task.retry_count += 1
    task.last_failure_at = now()

    if task.retry_count >= max_retries:
        set task.state = Failed
        emit task:state:failed
    else:
        set task.state = Waiting
        emit task:state:waiting
        # Dispatcher will pick it up after backoff.
```

### 18.3 Merge Queue Processing

```
function process_merge_queue(server):
    if server.mode == Stop:
        return
    if server.mode == Pause:
        return  # Queue is held. Only Flush triggers processing.

    # Play mode: orchestrator has merge authority.
    for entry in server.merge_queue where status == Pending:
        evaluation = orchestrator.evaluate(entry.task)

        if evaluation.approved:
            entry.status = Approved
            emit merge:approved

            conflict = check_merge_conflicts(entry)
            if conflict:
                entry.status = Conflict
                entry.task.state = Conflict
                emit merge:conflict
                continue

            perform_merge(entry)
            entry.status = Merged
            entry.task.state = Completed
            emit merge:completed
            emit task:state:completed
        else:
            entry.status = Rejected
            emit merge:rejected
            # Send task back to implementor with feedback.
            restart_with_feedback(entry.task, evaluation.feedback)

function flush_merge_queue(server):
    # Only callable in Pause mode.
    for entry in server.merge_queue where status == Approved:
        conflict = check_merge_conflicts(entry)
        if conflict:
            entry.status = Conflict
            entry.task.state = Conflict
            emit merge:conflict
            continue

        perform_merge(entry)
        entry.status = Merged
        entry.task.state = Completed
        emit merge:completed
        emit task:state:completed

    emit system:flush
```

### 18.4 Event Routing

```
function publish(bus, event):
    # Persist to task-specific log.
    bus.store.append(event.task, event)

    # Broadcast to live subscribers.
    for subscriber in bus.subscribers:
        if matches(subscriber.pattern, event.type)
           and matches(subscriber.task_filter, event.task):
            subscriber.send(event)

function matches(pattern, event_type):
    # Colon-delimited pattern matching with wildcard support.
    pattern_parts = pattern.split(":")
    type_parts = event_type.split(":")

    for i in 0..pattern_parts.len():
        if pattern_parts[i] == "*":
            return true  # Wildcard matches all remaining segments.
        if i >= type_parts.len():
            return false
        if pattern_parts[i] != type_parts[i]:
            return false

    return pattern_parts.len() == type_parts.len()
```

## 19. Test and Validation Matrix

### 19.1 Unit Tests

Each crate has unit tests covering its core logic in isolation:

| Crate | Coverage |
|-------|----------|
| events | Event serialization, pattern matching, wildcards, store append/read, bus publish/subscribe/replay |
| github | Response normalization, GraphQL response deserialization, rate limit parsing, pagination cursor handling, filter construction |
| runtime | Protocol codec (encode/decode/partial lines), command/event serialization |
| server | Mode transitions (all actor/direction combinations), task state transitions, merge queue operations (enqueue/approve/reject/flush/conflict/cleanup), presence tracking, slot accounting |

### 19.2 Mock Integration Tests

Tests that use mock servers or in-process fakes to test cross-component behavior:

| Component | Coverage |
|-----------|----------|
| GitHub client | List/get issues and PRs, pagination, nested comment/review fetching, error handling (auth, not found, GraphQL errors, rate limiting), since-based filtering |
| Poller | High-water mark advancement, failure recovery (mark not advanced), empty poll stability |
| Dispatcher | Candidate selection (resume vs new), priority sorting, concurrency enforcement (global and per-project), mode gating, backoff eligibility |
| Merge queue | Mode-dependent behavior (Stop/Pause/Play), flush in Pause, conflict detection, rejection with feedback |

### 19.3 Container Integration Tests

Tests that exercise the full session lifecycle with real containers. These are slower and require
the container runtime to be available.

| Test | What it validates |
|------|-------------------|
| Session start | Container creation, supervisor ready, agent launch |
| Agent execution | Send prompt, receive output, agent exits cleanly |
| Chat injection | Send message to running agent, receive response |
| Exec command | Run command inside container, receive result |
| Session restart | Stop agent, restart in same workspace, verify repo state persists |
| Session cleanup | Destroy container, verify resources released |

These tests use a mock agent (simple echo process) to avoid depending on a real AI provider.
The existing `verify.ts` script (§session-runtime.md) is the foundation for these tests.

### 19.4 End-to-End Tests

Full system tests that exercise the platform from issue discovery to merge:

| Test | What it validates |
|------|-------------------|
| Happy path | Issue created → task dispatched → agent completes → merge queue → merged |
| Question flow | Agent asks question → human answers → agent resumes → completes |
| Retry on failure | Agent crashes → task retried → succeeds on second attempt |
| Concurrency | Multiple tasks dispatched up to limit, excess tasks wait |
| Mode transitions | Stop halts agents, Pause holds merges, Play resumes everything |
| Conflict resolution | Two tasks complete, second has conflict, gets re-engaged |
| Priority ordering | Higher-priority task dispatched before lower-priority |

End-to-end tests use a fixture GitHub repository (or mock server) and a mock agent. They
exercise the full dispatch → session → merge pipeline.

### 19.5 Test Environment

- **Unit and mock tests:** Run with `cargo test` and require no external dependencies.
- **Container tests:** Require the container runtime (`container` CLI) and a pre-built base image.
  Gated behind a `--features container` flag.
- **End-to-end tests:** Require container runtime and optionally a GitHub token for live API
  tests. Gated behind `--features e2e`.
- **GitHub integration tests:** Require a `GITHUB_TOKEN` and a fixture repository. Gated behind
  `--features integration`.

## 20. Implementation Checklist

A conforming implementation must satisfy all of the following:

### 20.1 Core Platform

- [ ] Server starts, tracks mode (Stop/Pause/Play), and enforces transition rules
- [ ] Event system: append-only log with per-task storage, pub/sub with pattern matching
- [ ] Human presence tracking based on active GUI connections
- [ ] Multi-project support with per-project configuration

### 20.2 GitHub Integration

- [ ] GraphQL client fetches issues and PRs with full metadata
- [ ] Normalized model decoupled from GitHub API shapes
- [ ] Polling with high-water mark for incremental discovery
- [ ] Rate limit tracking and backoff

### 20.3 Scheduling and Dispatch

- [ ] Event-driven dispatch with reconciliation tick
- [ ] Candidate selection: resume candidates before new work
- [ ] Priority sorting: explicit priority → unblocking value → recency
- [ ] Global and per-project concurrency limits with slot accounting
- [ ] Mode-gated dispatch (no dispatch in Stop)

### 20.4 Sessions and Agent Runner

- [ ] Container lifecycle: create, start, attach, stop, destroy
- [ ] Supervisor protocol: start, chat, stop, exec commands; all event types
- [ ] Session lifecycle: creation → ready → running → ended
- [ ] Chat injection from human and orchestrator
- [ ] Workspace persistence across session restarts
- [ ] Session soft/hard time limits with escalation nudge

### 20.5 Merge Queue

- [ ] Queue entry lifecycle: pending → approved/rejected → merged/conflict
- [ ] Mode-dependent merge authority (Stop: held, Pause: held with flush, Play: orchestrator)
- [ ] Conflict detection and re-engagement
- [ ] Quality evaluation by orchestrator before queuing

### 20.6 Retry and Recovery

- [ ] Failure classification (transient vs deterministic)
- [ ] Exponential backoff with jitter
- [ ] Progress detection to distinguish transient from deterministic failures
- [ ] Server restart recovery: state reconstruction from event log, orphaned session detection
- [ ] Failure surfacing to orchestrator and human

### 20.7 Prompt Construction

- [ ] Layered prompt assembly: system prompt, task description, context, instructions
- [ ] Retry context for failed tasks
- [ ] Project-level system prompt from workflow configuration

### 20.8 Observability

- [ ] Structured JSON logging with consistent fields
- [ ] Runtime snapshot endpoint (full system state as JSON)
- [ ] Token and cost accounting per task and per project
- [ ] GUI dashboard with live task list, session view, merge queue, event stream

### 20.9 Security

- [ ] Session isolation via container runtime
- [ ] Secret injection via environment variables (not persisted in logs or state)
- [ ] Session time limits
