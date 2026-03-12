# Session Runtime

Status: Provisional

This document specifies the session runtime architecture — how sessions are hosted, how the
host communicates with session processes, and how agent lifecycle is managed. It is a companion
to the main spec (spec.md §9 Sessions, §10 Workspace Management).

## 1. Overview

A session is a multi-process runtime environment. It hosts the agent, any processes the agent
spawns (test runners, build tools, git operations), and a supervisor that manages the agent
lifecycle and bridges communication back to the host.

Each session runs inside its own container — a lightweight Linux VM provisioned by the container
runtime. The host-side session wrapper holds the stdio connection to the container and bridges
between the supervisor protocol and the server's event bus.

```
Server (host)
  └── Session Wrapper (host process, one per active session)
        │
        ├── reads/writes stdio ◄──► container stdin/stdout
        ├── emits events to the event bus
        ├── receives chat messages from server, forwards to supervisor
        │
        └── container (lightweight Linux VM)
              └── Supervisor (PID 1, Bun/TS)
                    ├── manages agent process lifecycle
                    ├── frames agent output as protocol events
                    ├── routes incoming commands to agent stdin
                    └── agent process (Claude Code or other provider)
                          └── child processes (git, test runners, builds, etc.)
```

## 2. Container Runtime

### 2.1 Provider

The initial container runtime is `container` (apple/container) — Apple's tool for running Linux
containers as lightweight VMs on macOS with Apple silicon.

Each session gets its own VM. This provides:

- **Process isolation.** Processes in one session cannot see or affect processes in another.
- **Filesystem isolation.** Each session has its own filesystem. No shared mounts between sessions.
- **Resource boundaries.** CPU and memory can be limited per container.
- **Multi-process support.** The container hosts the supervisor, agent, and any processes the
  agent spawns — builds, test runners, language servers, etc.
- **Standard OCI images.** The base image is a standard container image, buildable and
  distributable through any OCI registry.

The container runtime is an implementation detail. The session wrapper interacts with containers
through the supervisor protocol (§4), not through container runtime APIs directly. A different
runtime (Docker, Podman, remote VMs) could be substituted by implementing the same lifecycle
operations: create, start, stop, destroy, attach stdio.

### 2.2 Base Image

The base image is a pre-built OCI image containing the tools an agent needs to work on code.

Contents:

- **Git and GitHub CLI** (`git`, `gh`) — for repository operations and PR management.
- **Bun** — JavaScript/TypeScript runtime. Also runs the supervisor process.
- **Rust and Cargo** — for Rust-based projects.
- **Agent CLI** — the coding agent executable (Claude Code initially).
- **Standard utilities** — coreutils, curl, ssh, etc.

The base image is built ahead of time and cached locally. Session creation pulls from the local
cache, so container startup is not blocked by image builds.

Projects with specialized toolchain needs may extend the base image or provide their own. The
image reference is configurable per project.

### 2.3 Container Lifecycle

Creation:

1. `container create` with the base image, resource limits, and environment variables.
2. `container start` to boot the VM.
3. Attach to the container's stdio (stdin/stdout) to establish the supervisor connection.
4. Wait for the supervisor to emit `{"ev":"system:ready"}` indicating it is up and accepting
   commands.

Workspace reuse (spec.md §10.2):

- `container stop` halts the VM. The container's filesystem persists.
- `container start` + re-attach resumes from the same filesystem state.
- The supervisor starts fresh on each boot, but the repo, branch, and any committed work persist.

Cleanup (spec.md §10.3):

- `container stop` + `container delete` removes the container and its filesystem.

## 3. Repo Provisioning

When a session is created, the supervisor clones the repository inside the container.

1. The host sends a `start` command (§4.1) that includes the repo URL and branch name.
2. The supervisor runs `git clone` inside the container. The clone uses the credentials provided
   via environment variables (§3.1).
3. A new branch is created off the project's default branch, or an existing task branch is
   checked out if the workspace is being reused.

Cloning inside the container means:

- The repo lives on the container's native filesystem — no bind mount performance penalty.
- Each session's repo copy is fully independent.
- Results are pushed via `git push` from inside the container. The host never needs direct
  filesystem access to the container's repo.

### 3.1 Credential Injection

Credentials are passed to the container as environment variables at creation time:

- `GITHUB_TOKEN` — for git operations and `gh` CLI.
- Agent-specific API keys (e.g., `ANTHROPIC_API_KEY`) — for the coding agent's AI provider.

Environment variables are set at `container create` time and are available to all processes
inside the container. They are not baked into the image.

This is the provisional approach. Future iterations may support:

- Repo-scoped tokens with narrower permissions.
- Mounted secret files instead of environment variables.
- Token refresh for long-running sessions.

## 4. Supervisor Protocol

The supervisor is a Bun/TypeScript process that runs as PID 1 inside the container. It
communicates with the host-side session wrapper over the container's stdio using a JSON-lines
protocol (one JSON object per line, newline-delimited).

### 4.1 Commands (Host → Container)

Commands are sent by the session wrapper to the supervisor over stdin.

**`start`** — Start (or restart) the agent process.

```json
{"cmd": "start", "repo": "https://github.com/owner/repo.git", "branch": "tasks/abc-123", "prompt": "..."}
```

On first start, the supervisor clones the repo and launches the agent with the given prompt.
On restart (workspace reuse), the supervisor skips cloning and launches the agent in the
existing working directory.

**`chat`** — Deliver a message to the agent's stdin.

```json
{"cmd": "chat", "text": "Try using the existing Parser class instead."}
```

The supervisor writes the text to the agent process's stdin. If the agent is not running, the
message is held until the next `start`.

**`stop`** — Gracefully stop the agent process.

```json
{"cmd": "stop"}
```

The supervisor sends SIGTERM to the agent, waits for exit (with a timeout), then sends SIGKILL
if necessary. The container remains running.

**`exec`** — Run a one-off command inside the container.

```json
{"cmd": "exec", "id": "req-1", "argv": ["git", "status"]}
```

The supervisor spawns the command, collects its output, and emits an `exec:result` event with
the matching ID.

### 4.2 Events (Container → Host)

Events are emitted by the supervisor to the session wrapper over stdout.

**`system:ready`** — Supervisor is initialized and accepting commands.

```json
{"ev": "system:ready"}
```

**`agent:started`** — Agent process was launched.

```json
{"ev": "agent:started", "pid": 42}
```

**`agent:stdout`** — Agent produced output.

```json
{"ev": "agent:stdout", "data": "I'll start by reading the existing code..."}
```

**`agent:stderr`** — Agent produced error output.

```json
{"ev": "agent:stderr", "data": "Warning: ..."}
```

**`agent:exit`** — Agent process exited.

```json
{"ev": "agent:exit", "code": 0, "signal": null}
```

**`exec:result`** — Response to an `exec` command.

```json
{"ev": "exec:result", "id": "req-1", "code": 0, "stdout": "On branch tasks/abc-123\n...", "stderr": ""}
```

### 4.3 Protocol Notes

- All messages are single-line JSON (no embedded newlines in string values — use `\n` escapes).
- The supervisor must not write anything to stdout except protocol events. Non-protocol output
  (e.g., from the supervisor's own logging) goes to stderr or a log file.
- If the supervisor encounters a malformed command, it ignores it and optionally logs a warning.
- The protocol is versioned implicitly by the supervisor binary version in the base image. The
  host and supervisor are built from the same codebase, so version skew is managed by image
  updates.

## 5. Session Wrapper (Host-Side)

The session wrapper is the host-side counterpart to the supervisor. There is one wrapper
instance per active session, managed by the server's session manager.

### 5.1 Responsibilities

- **Container lifecycle.** Create, start, stop, and destroy the container for its session.
- **Protocol bridge.** Read supervisor events from the container's stdout, write commands to
  its stdin.
- **Event emission.** Translate supervisor events into the server's event bus events
  (spec.md §8). For example, `agent:stdout` events are interpreted and may produce
  `agent:message`, `agent:question`, or `task:state:*` events depending on content.
- **Chat relay.** When the server receives a chat message for this session (from the human or
  orchestrator), the wrapper sends it as a `chat` command to the supervisor.
- **Health monitoring.** Detect if the supervisor or container becomes unresponsive and report
  failure.

### 5.2 Transport Interface

The session wrapper uses a transport abstraction so the communication mechanism can be changed
without affecting the rest of the session logic.

```typescript
interface SessionTransport {
  send(command: SupervisorCommand): void
  onEvent(cb: (event: SupervisorEvent) => void): void
  onClose(cb: (reason: string) => void): void
  close(): Promise<void>
}
```

The initial implementation of `SessionTransport` wraps the container's stdio streams. Future
implementations could use a socket, HTTP, or a remote VM API — the session wrapper and event
emission logic are unaffected.

## 6. Open Questions

- **Startup optimization.** Can we pre-clone repos into a volume and mount them to avoid
  cloning on every session creation? Trade-off: faster start vs. stale state.
- **Agent output parsing.** How much interpretation does the session wrapper do on
  `agent:stdout` events? This depends on the agent provider's output format and is likely
  provider-specific.
- **Multiple agents per session.** The current design assumes one agent per session (spec.md
  §9.6). If this changes, the supervisor protocol would need agent IDs on commands and events.
- **Container resource defaults.** What CPU/memory limits are appropriate for a typical session?
  Needs benchmarking with real agent workloads.
- **Image extensibility.** How does a project specify additional tools or dependencies for its
  sessions? Dockerfile extension? Features list? Runtime install via `postCreate` hook?
