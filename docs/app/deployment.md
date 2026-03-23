# Deployment

This guide covers deploying Tasks as a persistent service with self-update support.

## Overview

For production use, Tasks runs under a wrapper script (`tasks-runner.sh`) that:
- Handles automatic updates when the server requests them
- Manages process lifecycle and signals
- Provides build failure recovery
- Logs to persistent files

## Quick Start

### Development (Manual)

```bash
# Build and run directly
cargo build --release
./target/release/tasks run --web
```

### Production (Wrapper Script)

```bash
# Run with self-update support
./scripts/tasks-runner.sh --web
```

## Service Installation

### Linux (systemd)

1. **Create a tasks user** (optional but recommended):

   ```bash
   sudo useradd -r -s /bin/false -d /opt/tasks tasks
   ```

2. **Install the application**:

   ```bash
   sudo git clone https://github.com/iamnbutler/tasks.git /opt/tasks
   sudo chown -R tasks:tasks /opt/tasks
   ```

3. **Create environment file**:

   ```bash
   sudo mkdir -p /etc/tasks
   sudo tee /etc/tasks/environment << 'EOF'
   GITHUB_TOKEN=ghp_your_token_here
   ANTHROPIC_API_KEY=sk-ant-your_key_here
   TASKS_DATA_DIR=/var/lib/tasks
   TASKS_UPDATE_CHECK_ENABLED=true
   EOF
   sudo chmod 600 /etc/tasks/environment
   ```

4. **Create data directory**:

   ```bash
   sudo mkdir -p /var/lib/tasks
   sudo chown tasks:tasks /var/lib/tasks
   ```

5. **Build the server**:

   ```bash
   cd /opt/tasks
   sudo -u tasks cargo build --release
   sudo -u tasks bun install && sudo -u tasks bun web build
   ```

6. **Install and start the service**:

   ```bash
   sudo cp /opt/tasks/scripts/tasks.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable tasks
   sudo systemctl start tasks
   ```

7. **View logs**:

   ```bash
   journalctl -u tasks -f
   ```

### macOS (launchd)

1. **Clone the repository**:

   ```bash
   git clone https://github.com/iamnbutler/tasks.git ~/tasks
   cd ~/tasks
   ```

2. **Build the server**:

   ```bash
   cargo build --release
   bun install && bun web build
   ```

3. **Create logs directory**:

   ```bash
   mkdir -p ~/Library/Logs/tasks
   ```

4. **Edit the plist**:

   ```bash
   cp scripts/com.tasks.plist ~/Library/LaunchAgents/
   # Edit ~/Library/LaunchAgents/com.tasks.plist
   # Replace YOUR_USERNAME with your actual username
   # Set GITHUB_TOKEN and ANTHROPIC_API_KEY
   ```

5. **Load the service**:

   ```bash
   launchctl load ~/Library/LaunchAgents/com.tasks.plist
   ```

6. **View logs**:

   ```bash
   tail -f ~/Library/Logs/tasks/stdout.log
   ```

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `TASKS_DATA_DIR` | `~/.local/state/tasks` | Data directory for SQLite and logs |
| `TASKS_WEB_PORT` | `4800` | HTTP server port |
| `TASKS_UPDATE_CHECK_ENABLED` | `true` | Enable update checking |
| `TASKS_UPDATE_CHECK_INTERVAL` | `300` | Update check interval (seconds) |
| `TASKS_UPDATE_AUTO_APPLY` | `false` | Auto-apply updates when idle |

### Wrapper Script Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `TASKS_RUNNER_LOG` | `$DATA_DIR/runner.log` | Wrapper script log file |
| `TASKS_PID_FILE` | `$DATA_DIR/tasks.pid` | PID file location |
| `TASKS_HEALTH_TIMEOUT` | `30` | Health check timeout (seconds) |
| `TASKS_HEALTH_ENDPOINT` | `http://localhost:4800/api/mode` | Health check URL |
| `TASKS_MAX_RETRIES` | `5` | Max git fetch retry attempts |
| `TASKS_RETRY_DELAY` | `5` | Initial retry delay (seconds) |

## Self-Update System

The self-update system allows Tasks to update itself from the main branch:

1. **Detection**: Server polls `origin/main` for new commits
2. **Notification**: Emits `system:update_available` event
3. **Apply**: User triggers update via API or auto-apply kicks in
4. **Execution**: Server exits with code 100
5. **Rebuild**: Wrapper script pulls, rebuilds, and restarts

### Manual Update Trigger

```bash
curl -X POST http://localhost:4800/api/self-update/apply
```

### Rebuild Scopes

Updates rebuild only what changed:

| Scope | Trigger | Actions |
|-------|---------|---------|
| `container` | `crates/supervisor/**`, `Dockerfile`, `Makefile` | Full rebuild |
| `server` | `crates/**` | Server + frontend |
| `frontend` | `web/**` | Frontend only |

## Robustness Features

### Build Failure Recovery

If a build fails during update:
1. The wrapper logs the error
2. Restores the backup binary (if available)
3. Restarts with the previous version

### Network Failure Handling

Git operations use exponential backoff:
- Initial delay: 5 seconds
- Max delay: 5 minutes
- Max retries: 5 (configurable)

### Partial Update Recovery

If an update is interrupted:
1. State is saved to `.update-state`
2. On restart, the wrapper resumes from the last successful step
3. Clean completion removes state files

### Signal Handling

The wrapper properly handles:
- `SIGTERM`: Graceful shutdown (forwarded to server)
- `SIGINT`: Graceful shutdown (forwarded to server)
- Server gets 30 seconds to shut down gracefully

## Troubleshooting

### Service won't start

Check logs:
```bash
# Linux
journalctl -u tasks -n 50

# macOS
tail -50 ~/Library/Logs/tasks/stderr.log
```

Common issues:
- Missing environment variables (GITHUB_TOKEN, ANTHROPIC_API_KEY)
- Incorrect paths in service files
- Permission issues on data directory

### Update stuck

Check state file:
```bash
cat ~/.local/state/tasks/.update-state
```

Clear stuck state:
```bash
rm ~/.local/state/tasks/.update-state ~/.local/state/tasks/.update-scope
```

### Build failures

Check runner log:
```bash
tail -100 ~/.local/state/tasks/runner.log
```

Common causes:
- Rust toolchain issues
- Missing cross-compilation toolchain
- Network issues during cargo fetch

## Security

### Recommended Practices

1. **Run as dedicated user**: Don't run as root
2. **Secure environment file**: `chmod 600` on files with tokens
3. **Firewall**: Only expose port 4800 if needed externally
4. **Updates**: Keep self-update enabled for security patches

### Service File Security

The systemd service includes hardening options:
- `NoNewPrivileges=true`
- `ProtectSystem=strict`
- `ProtectHome=true`
- `PrivateTmp=true`

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
