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

_TODO: Define the normalized issue/PR model._

## 12. Scheduling and Dispatch

_TODO: Candidate selection, priority sorting, concurrency limits, dispatch logic._

## 13. Retry and Recovery

_TODO: Exponential backoff, continuation retries, restart recovery, failure classes._

## 14. Workflow Configuration

_TODO: WORKFLOW.md format, front matter schema, prompt templates, dynamic reload._

## 15. Prompt Construction

_TODO: How task details are assembled into agent prompts, template rendering, retry/continuation
context._

## 16. Observability

_TODO: Structured logging, GUI dashboard, runtime snapshots, token/cost accounting._

## 17. Security and Safety

_TODO: Workspace isolation, secret handling, trust boundaries, agent sandboxing._

## 18. Reference Algorithms

_TODO: Pseudocode for key flows (dispatch tick, session lifecycle, merge queue processing,
event routing)._

## 19. Test and Validation Matrix

_TODO: Core conformance tests, extension tests, integration test profile._

## 20. Implementation Checklist

_TODO: Definition of done for a conforming implementation._
