# Self-Update Mechanism

Tasks includes a self-update mechanism that allows the server to detect new commits on `origin/main`, notify users, and seamlessly restart with the updated code.

## Overview

The self-update system consists of four components:

1. **UpdateChecker** - Background task that polls for new commits
2. **API Endpoints** - HTTP endpoints for checking/triggering updates
3. **Frontend UI** - Update banner with one-click apply
4. **Wrapper Script** - Shell script that handles rebuild and restart

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    tasks-runner.sh (wrapper)                      │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                    Tasks Server                             │  │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │  │
│  │  │ UpdateChecker │  │ API Endpoints │  │ Event Bus        │  │  │
│  │  │ (poll origin) │  │ /self-update  │  │ system:update_*  │  │  │
│  │  └──────┬───────┘  └──────────────┘  └──────────────────┘  │  │
│  │         │                                                    │  │
│  │         ▼                                                    │  │
│  │  Detect new commit → Emit event → Show banner → Apply       │  │
│  │                                                              │  │
│  │  Exit code 100 → wrapper pulls, rebuilds, restarts          │  │
│  └────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## Update Flow

### Detection

1. `UpdateChecker` runs in background (every 5 minutes by default)
2. Executes `git fetch origin main`
3. Compares `HEAD` with `origin/main`
4. If different, analyzes changed files to determine rebuild scope
5. Emits `system:update_available` event with scope information

### Triggering Update

1. User sees update banner in web UI
2. User clicks "Update Now"
3. `POST /api/self-update/apply` is called
4. Server emits `system:update_applying` event
5. Server drains active sessions (waits for completion or timeout)
6. Server writes rebuild scope to `.update-scope` file
7. Server exits with code 100

### Rebuild and Restart

1. Wrapper script (`tasks-runner.sh`) catches exit code 100
2. Runs `git pull origin main`
3. Reads `.update-scope` to determine what to rebuild
4. Rebuilds necessary components
5. Restarts the server

## Rebuild Scopes

The update system intelligently determines what needs rebuilding:

| Scope | Description | Rebuild Steps |
|-------|-------------|---------------|
| `none` | No rebuild needed | Just restart |
| `frontend` | Only web UI changed | `bun install && bun run build` |
| `server` | Rust crates changed | `cargo build --release` |
| `container` | Supervisor/Dockerfile changed | `make container-image` |
| `server_and_frontend` | Both server and UI | Both rebuilds |
| `server_and_container` | Server and container | Both rebuilds |
| `all` | Major changes | Full rebuild |

### Scope Detection

Files are mapped to scopes by pattern:

```
web/**           → frontend
crates/**        → server
Cargo.toml       → server
Cargo.lock       → server
crates/supervisor/** → container
Containerfile    → container
```

## Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_UPDATE_CHECK_ENABLED` | Enable update checking | `true` |
| `TASKS_UPDATE_CHECK_INTERVAL` | Check interval (seconds) | `300` |
| `TASKS_UPDATE_AUTO_APPLY` | Auto-apply updates | `false` |
| `TASKS_UPDATE_SESSION_TIMEOUT` | Session drain timeout (seconds) | `300` |

## API Endpoints

### GET /api/self-update

Returns current update status.

**Response:**
```json
{
  "update_available": true,
  "current_commit": "abc1234",
  "latest_commit": "def5678",
  "rebuild_scope": "server",
  "changed_files": ["crates/server/src/lib.rs"],
  "checked_at": "2024-01-15T10:30:00Z"
}
```

### POST /api/self-update/apply

Triggers an update. Returns immediately; actual update happens asynchronously.

**Response:**
```json
{
  "status": "applying",
  "message": "Update initiated, server will restart shortly"
}
```

## Events

| Event | Payload | Description |
|-------|---------|-------------|
| `system:update_available` | `{commit, scope, files}` | New version detected |
| `system:update_applying` | `{commit, scope}` | Update in progress |

## Wrapper Script

The `scripts/tasks-runner.sh` wrapper provides:

### Core Features
- Runs server in a loop
- Catches exit code 100 for updates
- Pulls changes and rebuilds based on scope
- Restarts server after update

### Robustness Features (Phase 4)

#### Build Failure Handling
- Backs up current binary before rebuild
- Falls back to backup on build failure
- Logs error and emits event

#### Network Failure Handling
- Exponential backoff on `git fetch` failures
- Initial delay: 5 seconds
- Maximum delay: 5 minutes
- Tracks consecutive failures

#### Partial Update Recovery
- Writes state file during multi-step update
- Can resume from last successful step
- Cleans up partial state

#### Signal Handling
- Forwards SIGTERM/SIGINT to server
- Graceful shutdown on signals
- PID file management

#### Logging
- Logs to `$DATA_DIR/runner.log`
- Log rotation at 10MB
- Timestamps on all entries

#### Health Checks
- Polls health endpoint after restart
- Confirms server is operational
- Retries with backoff

## Deployment

### systemd (Linux)

```ini
# /etc/systemd/system/tasks.service
[Unit]
Description=Tasks Server
After=network.target

[Service]
Type=simple
ExecStart=/path/to/tasks-runner.sh --web
Restart=on-failure
User=tasks

[Install]
WantedBy=multi-user.target
```

### launchd (macOS)

```xml
<!-- ~/Library/LaunchAgents/com.tasks.plist -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.tasks</string>
  <key>ProgramArguments</key>
  <array>
    <string>/path/to/tasks-runner.sh</string>
    <string>--web</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
```

## Security Considerations

### Git Operations
- Only pulls from configured origin
- Requires fast-forward merges only
- No force pushes or rebases

### Build Isolation
- Builds run in project directory
- No network access during build (except cargo/npm)
- Environment controlled by wrapper

### Rollback
- Backup binary retained for one update cycle
- Can manually restore from backup
- Git history available for revert

## Error Handling

| Error | Recovery |
|-------|----------|
| Network failure | Exponential backoff, continue current version |
| Build failure | Restore backup binary, continue |
| Merge conflict | Exit with error, require manual intervention |
| Timeout | Force restart, log warning |

## Implementation Phases

1. **Phase 1**: Core infrastructure - UpdateChecker, scope detection, wrapper script
2. **Phase 2**: API endpoints and events
3. **Phase 3**: Frontend UI components
4. **Phase 4**: Robustness, error handling, deployment

---

*See [Deployment Guide](../app/deployment.md) for production setup instructions.*
