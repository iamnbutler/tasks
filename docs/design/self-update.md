# Self-Update Mechanism Design

**Issue:** #305
**Author:** Based on design proposal by @iamnbutler
**Date:** 2026-03-23

## Overview

The Tasks server is often used to work on itself. When fixes are shipped to main, the running server doesn't receive benefits until manually restarted. This design introduces a self-update mechanism that detects new commits on main, determines what needs rebuilding, and performs a graceful restart.

## Goals

1. **Automatic detection**: Server polls `origin/main` for new commits
2. **Minimal rebuild**: Only rebuild what changed (server, container image, or frontend)
3. **Graceful transition**: Wait for sessions to complete, avoid data loss
4. **Safe rollback**: Build before restart so failures don't leave the server down
5. **User control**: Updates can be manual or automatic based on configuration

## Architecture

### Components

```
                           ┌──────────────────────┐
                           │  Update Checker      │
                           │  (background task)   │
                           └──────────┬───────────┘
                                      │ detects new commits
                                      ▼
                           ┌──────────────────────┐
                           │  Rebuild Detector    │
                           │  (analyze git diff)  │
                           └──────────┬───────────┘
                                      │ determines scope
                                      ▼
┌─────────────┐   trigger  ┌──────────────────────┐
│  Human API  │───────────▶│  Update Executor     │
│  (or auto)  │            │  (graceful shutdown) │
└─────────────┘            └──────────┬───────────┘
                                      │ exit(100)
                                      ▼
                           ┌──────────────────────┐
                           │  Wrapper Script      │
                           │  (pull, build, run)  │
                           └──────────────────────┘
```

### 1. Update Checker

A background task that periodically checks for updates by:
1. Running `git fetch origin main`
2. Comparing `HEAD` with `origin/main`
3. If different, analyzing changed files to determine rebuild scope
4. Emitting `UpdateAvailable` event with scope information

**Location:** `crates/app/src/update.rs`

**Configuration:**
```rust
pub struct UpdateConfig {
    /// Enable update checking (default: true)
    pub enabled: bool,
    /// Check interval in seconds (default: 300 = 5 min)
    pub check_interval: Duration,
    /// Auto-apply when no sessions active (default: false)
    pub auto_apply: bool,
    /// Path to the repo root (default: current working directory)
    pub repo_path: PathBuf,
}
```

### 2. Rebuild Scope Detection

Analyzes `git diff HEAD..origin/main --name-only` to determine what needs rebuilding:

| Changed Files | Rebuild Scope |
|---------------|---------------|
| `crates/supervisor/**`, `src/runtime/Dockerfile`, `Makefile` | `container` (includes server) |
| `crates/**` (any Rust crate) | `server` |
| `web/**` | `frontend` |
| Multiple scopes | Highest scope wins |

**Scope hierarchy:** `container` > `server` > `frontend`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RebuildScope {
    /// Only frontend changed — just rebuild web assets
    Frontend,
    /// Rust code changed — rebuild server binary
    Server,
    /// Container-related files changed — rebuild container image + server
    Container,
}
```

### 3. Update State

Server maintains update state exposed via API and events:

```rust
pub struct UpdateState {
    /// Whether an update is available
    pub available: bool,
    /// Current commit hash
    pub current_commit: String,
    /// Available commit hash (if update available)
    pub target_commit: Option<String>,
    /// What needs rebuilding
    pub rebuild_scope: Option<RebuildScope>,
    /// Commit message summary for the update
    pub commit_summary: Option<String>,
    /// Last check timestamp
    pub last_checked: Option<DateTime<Utc>>,
}
```

### 4. Update Executor

When an update is triggered (manually or automatically):

1. **Lower mode to Stop** — prevents new session dispatch
2. **Wait for active sessions** — or force-stop after timeout
3. **Write update scope** — `.update-scope` file for wrapper script
4. **Exit with code 100** — special exit code signals "update requested"

```rust
pub async fn apply_update(&self, server: &Server) -> Result<!, UpdateError> {
    // 1. Lower mode to Stop
    server.set_mode(Mode::Stop, &Actor::System).await?;

    // 2. Wait for sessions to complete (with timeout)
    let timeout = Duration::from_secs(300); // 5 minutes
    self.wait_for_sessions(server, timeout).await?;

    // 3. Write scope file
    let scope_file = self.data_dir.join(".update-scope");
    std::fs::write(&scope_file, self.rebuild_scope.to_string())?;

    // 4. Exit with update code
    std::process::exit(100);
}
```

### 5. Wrapper Script

The server should be launched via a wrapper script that handles the restart:

**`scripts/tasks-runner.sh`:**
```bash
#!/bin/bash
set -e

REPO_DIR="${TASKS_REPO_DIR:-$(pwd)}"
DATA_DIR="${TASKS_DATA_DIR:-$HOME/.local/state/tasks}"
SCOPE_FILE="$DATA_DIR/.update-scope"

while true; do
    # Run the server
    cargo run --release -- run --web
    EXIT_CODE=$?

    # Check if this is an update exit
    if [ $EXIT_CODE -eq 100 ]; then
        echo "Update requested, pulling and rebuilding..."

        # Pull latest
        cd "$REPO_DIR"
        git pull origin main

        # Read rebuild scope
        SCOPE="server"
        if [ -f "$SCOPE_FILE" ]; then
            SCOPE=$(cat "$SCOPE_FILE")
            rm "$SCOPE_FILE"
        fi

        # Rebuild based on scope
        case "$SCOPE" in
            container)
                echo "Rebuilding container image..."
                make container-image
                echo "Rebuilding server..."
                cargo build --release
                ;;
            server)
                echo "Rebuilding server..."
                cargo build --release
                ;;
            frontend)
                echo "Rebuilding frontend..."
                cd web && bun install && bun run build && cd ..
                ;;
        esac

        echo "Restarting server..."
        continue
    fi

    # Any other exit code — break the loop
    echo "Server exited with code $EXIT_CODE"
    break
done
```

## API

### Endpoints

**GET /api/self-update** — Check update status
```json
{
  "available": true,
  "current_commit": "abc1234",
  "target_commit": "def5678",
  "rebuild_scope": "server",
  "commit_summary": "Fix SSE presence guard (#279)",
  "last_checked": "2026-03-23T15:00:00Z"
}
```

**POST /api/self-update/apply** — Trigger update
```json
{
  "force": false  // If true, skip waiting for sessions
}
```

Response:
```json
{
  "status": "applying",
  "message": "Waiting for 2 active sessions to complete"
}
```

### Events

**`system:update_available`**
```json
{
  "target_commit": "def5678",
  "rebuild_scope": "server",
  "commit_summary": "Fix SSE presence guard (#279)"
}
```

**`system:update_applying`**
```json
{
  "target_commit": "def5678",
  "sessions_remaining": 2
}
```

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `TASKS_UPDATE_CHECK_ENABLED` | `true` | Enable update checking |
| `TASKS_UPDATE_CHECK_INTERVAL` | `300` | Check interval (seconds) |
| `TASKS_UPDATE_AUTO_APPLY` | `false` | Auto-apply when idle |
| `TASKS_UPDATE_SESSION_TIMEOUT` | `300` | Max wait for sessions (seconds) |

## Implementation Plan

### Phase 1: Core Infrastructure

**Files to create/modify:**
- `crates/app/src/update.rs` — Update checker, state, and executor
- `crates/app/src/config.rs` — Add update configuration
- `crates/app/src/run_loop.rs` — Spawn update checker task
- `scripts/tasks-runner.sh` — Wrapper script

**Deliverables:**
- Update checker background task
- Rebuild scope detection
- Update state management
- Exit code 100 mechanism

### Phase 2: API and Events

**Files to create/modify:**
- `crates/app/src/web.rs` — Add API endpoints
- `crates/events/src/lib.rs` — Add event types

**Deliverables:**
- GET/POST endpoints
- Event emission
- API state integration

### Phase 3: Frontend Integration

**Files to create/modify:**
- `web/src/components/update-banner.tsx` — Update notification UI
- `web/src/lib/api.ts` — API client functions
- `web/src/App.tsx` — Banner placement

**Deliverables:**
- Update available banner
- Apply update button
- Progress indication

### Phase 4: Robustness

**Deliverables:**
- Build failure handling (wrapper script retries old binary)
- Network failure handling (exponential backoff)
- Partial update recovery
- Systemd/launchd service files

## Alternatives Considered

### 1. In-process exec()

Replace the running process with a new binary using `exec()`.

**Rejected because:**
- Requires building before the update check (slow)
- No clean way to handle build failures
- Process state (file handles, sockets) may leak

### 2. Hot reload with dynamic libraries

Load new code as shared libraries at runtime.

**Rejected because:**
- Significantly complex to implement
- Rust doesn't support hot reloading well
- State migration between versions is hard

### 3. Blue-green deployment

Run two server instances and switch between them.

**Rejected because:**
- Overkill for a single-user application
- Requires port management and load balancing
- Adds operational complexity

## Open Questions

1. **Partial updates**: Should we support updating just the frontend without restarting the server? (Currently: no, all scopes restart)

2. **Orchestrator awareness**: Should the orchestrator factor pending updates into dispatch decisions? (e.g., don't start new long tasks if update is waiting)

3. **Notifications**: Do we need notifications beyond events? (Slack, email, etc.)

4. **Version pinning**: Should we support pinning to specific commits/tags instead of always tracking main?

## Security Considerations

- The update mechanism only pulls from the configured remote (origin)
- No arbitrary code execution — only rebuilding from source
- Exit code 100 is only honored by the wrapper script
- API endpoint should require human presence or authentication

## Testing Strategy

1. **Unit tests**: Rebuild scope detection, state management
2. **Integration tests**: Full update cycle in CI (mock git operations)
3. **Manual testing**: Real update on development machine
