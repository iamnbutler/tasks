# Self-Update Mechanism Design

This document describes the self-update mechanism for the Tasks server, enabling it to detect, download, and apply updates automatically while maintaining high availability.

## Overview

The self-update system allows the Tasks server to update itself from the `main` branch without manual intervention. It consists of four components:

1. **Update Checker** - Background task that polls `origin/main` for new commits
2. **Rebuild Scope Detection** - Analyzes changed files to determine minimal rebuild scope
3. **Update Executor** - Graceful shutdown with session wait, exits with code 100
4. **Wrapper Script** - Catches exit 100, pulls changes, rebuilds, restarts

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    tasks-runner.sh                          │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                   while true; do                      │  │
│  │  ┌─────────────────────────────────────────────────┐  │  │
│  │  │              Tasks Server Binary                 │  │  │
│  │  │  ┌──────────────────────────────────────────┐   │  │  │
│  │  │  │          Update Checker Task              │   │  │  │
│  │  │  │  - git fetch origin main (every 5 min)   │   │  │  │
│  │  │  │  - Compare HEAD vs origin/main           │   │  │  │
│  │  │  │  - Analyze changed files → rebuild scope │   │  │  │
│  │  │  │  - Emit UpdateAvailable event            │   │  │  │
│  │  │  └──────────────────────────────────────────┘   │  │  │
│  │  │                                                  │  │  │
│  │  │  On /api/self-update/apply:                     │  │  │
│  │  │    1. Lower mode to Stop                        │  │  │
│  │  │    2. Wait for sessions (with timeout)          │  │  │
│  │  │    3. Write .update-scope file                  │  │  │
│  │  │    4. Exit with code 100                        │  │  │
│  │  └─────────────────────────────────────────────────┘  │  │
│  │                        │                              │  │
│  │                   exit code 100                       │  │
│  │                        ▼                              │  │
│  │  ┌─────────────────────────────────────────────────┐  │  │
│  │  │           Update Sequence                       │  │  │
│  │  │  1. git pull origin main                       │  │  │
│  │  │  2. Read .update-scope                         │  │  │
│  │  │  3. Rebuild (container/server/frontend)        │  │  │
│  │  │  4. Restart server                             │  │  │
│  │  └─────────────────────────────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## Rebuild Scope Detection

Changed files are mapped to rebuild requirements using a hierarchy. Higher scopes include all lower scopes:

| Scope | Trigger Patterns | Rebuild Actions |
|-------|------------------|-----------------|
| `container` | `crates/supervisor/**`, `src/runtime/Dockerfile`, `Makefile` | `make container-image` + server + frontend |
| `server` | `crates/**` (except supervisor), `Cargo.*` | `cargo build --release` + frontend |
| `frontend` | `web/**` | `bun install && bun run build` |

Detection algorithm:
```
git diff --name-only HEAD..origin/main
```

Files are matched against patterns in priority order. The highest matching scope determines the rebuild.

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `TASKS_UPDATE_CHECK_ENABLED` | `true` | Enable update checking |
| `TASKS_UPDATE_CHECK_INTERVAL` | `300` | Check interval in seconds |
| `TASKS_UPDATE_AUTO_APPLY` | `false` | Auto-apply when no sessions active |
| `TASKS_UPDATE_SESSION_TIMEOUT` | `300` | Max wait for sessions to drain |

## API Endpoints

### GET /api/self-update

Returns current update status:

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

### POST /api/self-update/apply

Triggers update with optional force flag:

```json
{
  "force": false
}
```

Response:
```json
{
  "status": "applying",
  "message": "Waiting for 2 active sessions to complete"
}
```

## Event Types

### system:update_available

Emitted when a new update is detected:

```json
{
  "target_commit": "def5678",
  "rebuild_scope": "server",
  "commit_summary": "Fix SSE presence guard (#279)"
}
```

### system:update_applying

Emitted when update is in progress:

```json
{
  "target_commit": "def5678",
  "sessions_remaining": 2
}
```

## Wrapper Script

The `scripts/tasks-runner.sh` wrapper script handles the outer loop:

```bash
#!/usr/bin/env bash
# See scripts/tasks-runner.sh for full implementation

while true; do
    ./target/release/tasks run --web
    exit_code=$?

    case $exit_code in
        100)
            # Update requested
            perform_update
            ;;
        0)
            # Clean shutdown
            break
            ;;
        *)
            # Unexpected exit
            log "Server exited with code $exit_code"
            break
            ;;
    esac
done
```

### Robustness Features (Phase 4)

The wrapper script includes these robustness features:

1. **Build Failure Handling**
   - Detects build failures
   - Falls back to running previous binary
   - Logs error and emits event to log file

2. **Network Failure Handling**
   - Exponential backoff on git fetch failures
   - Doesn't spam events on repeated failures
   - Recovery when network returns

3. **Partial Update Recovery**
   - Tracks update state in `.update-state` file
   - Can resume from last successful step
   - Cleans up partial state on success

4. **Signal Handling**
   - SIGTERM/SIGINT forwarded to server
   - Graceful shutdown on signals
   - PID file management

5. **Logging**
   - Logs to `$DATA_DIR/runner.log`
   - Log rotation support
   - Structured log format

6. **Health Check Integration**
   - Waits for health endpoint after restart
   - Configurable timeout

## Service Files

### systemd (Linux)

`scripts/tasks.service`:

```ini
[Unit]
Description=Tasks Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/path/to/scripts/tasks-runner.sh
Restart=on-failure
RestartSec=5
User=tasks
WorkingDirectory=/path/to/tasks

[Install]
WantedBy=multi-user.target
```

### launchd (macOS)

`scripts/com.tasks.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.tasks</string>
    <key>ProgramArguments</key>
    <array>
        <string>/path/to/scripts/tasks-runner.sh</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>/path/to/tasks</string>
</dict>
</plist>
```

## Implementation Phases

| Phase | Issue | Scope |
|-------|-------|-------|
| 1 | #319 | Core infrastructure: update checker, scope detection, exit mechanism, wrapper script |
| 2 | #320 | API endpoints and events |
| 3 | #321 | Frontend UI: update banner component |
| 4 | #322 | Robustness: build failure handling, service files, signal handling |

## Design Decisions

### Why a wrapper script?

The wrapper script approach was chosen over in-process `exec()` for several reasons:

1. **Clean process replacement** - No orphaned state or file handles
2. **Build failure handling** - Can fall back to previous binary if build fails
3. **Service manager integration** - Works naturally with systemd/launchd
4. **Debugging** - Easier to inspect and debug the update process

### Why exit code 100?

Exit code 100 was chosen because:
- It's above the standard Unix error codes (1-127)
- It's distinct from common shell errors (126, 127, 128+)
- It's easy to remember and unlikely to conflict

### Why default to manual updates?

Auto-apply is opt-in (`TASKS_UPDATE_AUTO_APPLY=false` by default) because:
- Updates may interrupt active work
- Users may want to review changes before applying
- Predictable behavior is preferred for production systems

## Security Considerations

1. **Trust model** - Updates come from `origin/main`, which requires push access
2. **Build verification** - Consider adding commit signature verification
3. **Rollback** - Keep previous binary for rollback capability
4. **Audit trail** - Log all update operations with timestamps

## Future Considerations

1. **Partial frontend updates** - Hot-reload frontend without server restart
2. **Orchestrator integration** - Factor pending updates into dispatch decisions
3. **Notification channels** - Slack/email notifications for available updates
4. **Update scheduling** - Schedule updates during low-activity periods
