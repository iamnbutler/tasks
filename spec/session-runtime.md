# Session Runtime

Status: Draft

This document specifies the session runtime architecture — how the platform creates isolated
execution environments, provisions workspaces, and manages agent processes. It is a companion to
the main spec (spec.md §10 Sessions and Agent Runner, §11 Workspace Management, §18 Security and
Safety).

## 1. Overview

The session runtime provides the execution environment for agent sessions. Each session runs in
an isolated container with its own filesystem, processes, and network namespace. Inside the
container, a supervisor process (PID 1) manages the agent lifecycle and bridges communication
with the host.

```
Host (Rust server)
  └── Session Manager (spec.md §10)
        │
        ├── creates containers via ContainerRuntime trait
        ├── communicates via JSON-line protocol over stdio
        │
        └── Container (apple/container lightweight VM)
              └── Supervisor (PID 1)
                    ├── clones repo and sets up workspace
                    ├── starts agent process
                    ├── forwards agent I/O as protocol events
                    └── handles stop/chat commands from host
```

The runtime crate (`crates/runtime/`) implements the host side. The supervisor binary
(`crates/supervisor/`) runs inside containers.

## 2. Container Provider

### 2.1 Isolation Model

Sessions run in lightweight Linux VMs using [apple/container](https://github.com/apple/container).
Each container provides:

- **Process isolation.** Processes in one session cannot see or affect processes in another.
- **Filesystem isolation.** Each container has its own root filesystem. No shared mounts between
  sessions. The host filesystem is not accessible from inside containers.
- **Network namespace.** Containers have their own network stack with unrestricted outbound access
  (required for git, package managers, AI provider APIs).

### 2.2 Resource Limits

Containers can be configured with resource limits at creation time:

- `cpus` — CPU core limit (fractional, e.g., `2.0` for 2 cores)
- `memory` — Memory limit (e.g., `"8G"`)
- `dns` — DNS server (defaults to `8.8.8.8`)

These are passed to the `container create` command and enforced by the VM runtime.

### 2.3 Container Lifecycle

The `ContainerRuntime` trait defines the container lifecycle operations:

```rust
trait ContainerRuntime {
    async fn create(&self, config: &ContainerConfig) -> Result<String, ContainerError>;
    async fn start(&self, container_id: &str) -> Result<StdioTransport, ContainerError>;
    async fn stop(&self, container_id: &str) -> Result<(), ContainerError>;
    async fn destroy(&self, container_id: &str) -> Result<(), ContainerError>;
    async fn container_exists(&self, container_id: &str) -> Result<bool, ContainerError>;
}
```

1. **Create.** Allocates a container with the given config. Returns a container ID.
2. **Start.** Boots the container and attaches to its stdio. Returns a transport handle.
3. **Stop.** Sends shutdown signal to the container.
4. **Destroy.** Removes the container and cleans up resources.
5. **Exists.** Checks if a container exists (for restart recovery, spec.md §14.3).

## 3. Workspace Provisioning

### 3.1 Secret Injection

Secrets are injected as environment variables at container creation time. They are available to
all processes inside the container but are never written to disk.

Required secrets:
- `GITHUB_TOKEN` — Used for git clone/push operations and `gh` CLI. Embedded in HTTPS URLs
  for authentication.
- `ANTHROPIC_API_KEY` — Agent provider API key.

Optional configuration:
- `AGENT_CMD` — Command to run the agent (default: `claude`)
- `AGENT_ARGS` — Arguments for the agent command
- `AGENT_USER` — Non-root user to run the agent (default: `agent`)

### 3.2 Repository Setup

When the supervisor receives a `start` command, it provisions the workspace:

1. **Git configuration.** Creates `.gitconfig` for both root and agent users with:
   - User identity (`tasks@localhost`)
   - Safe directory setting for `/workspace`

2. **Clone.** Clones the repository to `/workspace`. The `GITHUB_TOKEN` is embedded in the
   clone URL for HTTPS authentication:
   ```
   https://x-access-token:{token}@github.com/owner/repo
   ```

3. **Branch.** Checks out (or creates) the specified branch.

4. **Ownership.** Changes ownership of `/workspace` to the agent user (clone runs as root).

If the workspace already contains a git repository (workspace reuse), the clone step is skipped.

## 4. Supervisor Protocol

The supervisor communicates with the host via a JSON-line protocol over stdio. Commands flow
host→supervisor on stdin; events flow supervisor→host on stdout.

### 4.1 Commands (Host → Supervisor)

Commands are single-line JSON objects with a `cmd` discriminator:

**start** — Initialize workspace and start the agent.
```json
{"cmd": "start", "repo": "owner/repo", "branch": "task-123", "prompt": "Implement feature X..."}
```

**chat** — Send a message to the running agent.
```json
{"cmd": "chat", "text": "Can you also add tests?"}
```

**stop** — Gracefully stop the agent process.
```json
{"cmd": "stop"}
```

**exec** — Execute an arbitrary command in the container (for debugging/inspection).
```json
{"cmd": "exec", "id": "req-1", "argv": ["git", "status"]}
```

### 4.2 Events (Supervisor → Host)

Events are single-line JSON objects with an `ev` discriminator:

**system:ready** — Supervisor is initialized and ready to accept commands.
```json
{"ev": "system:ready"}
```

**agent:started** — Agent process has been spawned.
```json
{"ev": "agent:started", "pid": 1234}
```

**agent:stdout** — Agent wrote to stdout.
```json
{"ev": "agent:stdout", "data": "Analyzing codebase..."}
```

**agent:stderr** — Agent wrote to stderr.
```json
{"ev": "agent:stderr", "data": "[debug] loading config"}
```

**agent:exit** — Agent process exited.
```json
{"ev": "agent:exit", "code": 0, "signal": null}
```

**exec:result** — Result of an exec command.
```json
{"ev": "exec:result", "id": "req-1", "code": 0, "stdout": "On branch main...", "stderr": ""}
```

### 4.3 Stream Conventions

- **stdout** is reserved for protocol events. One JSON object per line, newline-delimited.
- **stderr** is for supervisor diagnostic logging (prefixed with `[supervisor]`).
- Agent stdout/stderr are captured and forwarded as `agent:stdout`/`agent:stderr` events.

### 4.4 Agent Process Management

The supervisor manages the agent process lifecycle:

1. **Start.** On receiving `start`, the supervisor:
   - Provisions the workspace (§3.2)
   - Spawns the agent via `sudo --preserve-env -u agent` (Claude Code refuses root)
   - Emits `agent:started` with the PID

2. **I/O forwarding.** Agent stdout/stderr are read line-by-line and forwarded as events.

3. **Chat injection.** On receiving `chat`, the supervisor writes the text to the agent's stdin.

4. **Graceful stop.** On receiving `stop`:
   - Sends SIGTERM to the agent process
   - Waits up to 5 seconds for graceful exit
   - Sends SIGKILL if the process doesn't terminate
   - Emits `agent:exit` with the final status

## 5. Session Lifecycle

The full session flow, from the host perspective:

1. **Create container.** Host calls `runtime.create(config)` with the container image and
   environment variables (secrets, agent config).

2. **Start container.** Host calls `runtime.start(container_id)`, which boots the container
   and returns a transport for stdio communication.

3. **Wait for ready.** Host waits for `system:ready` event indicating the supervisor is
   initialized.

4. **Start agent.** Host sends `start` command with repo URL, branch name, and initial prompt.

5. **Monitor events.** Host processes events:
   - `agent:started` — agent is running
   - `agent:stdout`/`agent:stderr` — forward to session chat, emit to event bus
   - `agent:exit` — session completed

6. **Human/orchestrator interaction.** If a participant sends a chat message, the host sends
   a `chat` command to inject it into the agent's stdin.

7. **Stop session.** When the session needs to end (mode change to Stop, task cancelled):
   - Host sends `stop` command
   - Waits for `agent:exit`
   - Calls `runtime.stop()` then `runtime.destroy()`

## 6. Container Image

The container image is built with `container build` (apple/container CLI), not Docker. The
image includes:

- Base Linux environment (Debian-based)
- Git, curl, common build tools
- Node.js, Python, Rust (language runtimes)
- The `claude` CLI (agent provider)
- A non-root `agent` user for running the agent
- The supervisor binary (cross-compiled from `crates/supervisor/`)

Build command:
```sh
make container-image   # cross-compile supervisor + build image
```

The supervisor is cross-compiled on the host for `aarch64-unknown-linux-gnu` to avoid building
inside Docker. See CLAUDE.md for toolchain prerequisites.
