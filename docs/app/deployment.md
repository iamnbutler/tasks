# Deployment

This guide covers deploying Tasks as a long-running service on Linux (systemd) and macOS (launchd).

## Quick Start

### Development

For development, run the server directly:

```bash
cargo run -- run --web
```

### Production

For production, use the wrapper script which handles self-updates:

```bash
./scripts/tasks-runner.sh --web
```

## Prerequisites

### Required

- Rust toolchain (1.75+)
- bun (for frontend)
- git
- Cross-compilation toolchain for container builds

### Environment

Create an environment file with your credentials:

```bash
# ~/.config/tasks/env (macOS) or /etc/tasks/env (Linux)
GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
ANTHROPIC_API_KEY=sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxx
```

## Linux (systemd)

### Installation

1. **Clone the repository:**

   ```bash
   sudo mkdir -p /opt/tasks
   sudo git clone https://github.com/iamnbutler/tasks.git /opt/tasks
   cd /opt/tasks
   ```

2. **Build the application:**

   ```bash
   cargo build --release --package tasks-app
   cd web && bun install && bun run build && cd ..
   make container-image
   ```

3. **Create the tasks user:**

   ```bash
   sudo useradd -r -s /bin/false tasks
   ```

4. **Create data directory:**

   ```bash
   sudo mkdir -p /var/lib/tasks
   sudo chown tasks:tasks /var/lib/tasks
   ```

5. **Create environment file:**

   ```bash
   sudo mkdir -p /etc/tasks
   sudo cat > /etc/tasks/env << 'EOF'
   GITHUB_TOKEN=ghp_xxxx
   ANTHROPIC_API_KEY=sk-ant-xxxx
   EOF
   sudo chmod 600 /etc/tasks/env
   sudo chown tasks:tasks /etc/tasks/env
   ```

6. **Install the service file:**

   ```bash
   sudo cp scripts/tasks.service /etc/systemd/system/
   sudo systemctl daemon-reload
   ```

7. **Enable and start:**

   ```bash
   sudo systemctl enable tasks
   sudo systemctl start tasks
   ```

### Management

```bash
# Check status
sudo systemctl status tasks

# View logs
journalctl -u tasks -f

# Restart
sudo systemctl restart tasks

# Stop
sudo systemctl stop tasks

# Disable
sudo systemctl disable tasks
```

### Updating

The self-update mechanism handles updates automatically. To manually update:

```bash
cd /opt/tasks
sudo -u tasks git pull
sudo -u tasks cargo build --release --package tasks-app
sudo systemctl restart tasks
```

## macOS (launchd)

### User Agent Installation (Recommended)

Runs as your user, starts at login:

1. **Clone the repository:**

   ```bash
   mkdir -p ~/Developer
   git clone https://github.com/iamnbutler/tasks.git ~/Developer/tasks
   cd ~/Developer/tasks
   ```

2. **Build the application:**

   ```bash
   cargo build --release --package tasks-app
   cd web && bun install && bun run build && cd ..
   make container-image
   ```

3. **Create environment file:**

   ```bash
   mkdir -p ~/.config/tasks
   cat > ~/.config/tasks/env << 'EOF'
   export GITHUB_TOKEN=ghp_xxxx
   export ANTHROPIC_API_KEY=sk-ant-xxxx
   EOF
   chmod 600 ~/.config/tasks/env
   ```

4. **Create data directory:**

   ```bash
   mkdir -p ~/.local/state/tasks
   ```

5. **Install the plist:**

   ```bash
   # Edit the plist to match your paths if needed
   cp scripts/com.tasks.plist ~/Library/LaunchAgents/
   ```

6. **Load and start:**

   ```bash
   launchctl load ~/Library/LaunchAgents/com.tasks.plist
   launchctl start com.tasks
   ```

### Management

```bash
# Check status
launchctl list | grep tasks

# View logs
tail -f ~/.local/state/tasks/runner.log

# Stop
launchctl stop com.tasks

# Unload (disable)
launchctl unload ~/Library/LaunchAgents/com.tasks.plist

# Reload after plist changes
launchctl unload ~/Library/LaunchAgents/com.tasks.plist
launchctl load ~/Library/LaunchAgents/com.tasks.plist
```

### Updating

Updates happen automatically via the self-update mechanism. To manually update:

```bash
cd ~/Developer/tasks
git pull
cargo build --release --package tasks-app
launchctl stop com.tasks
launchctl start com.tasks
```

## Wrapper Script

The `scripts/tasks-runner.sh` wrapper provides production-ready features:

### Features

| Feature | Description |
|---------|-------------|
| Self-update | Automatically pulls and rebuilds on update signal |
| Build fallback | Restores backup binary if build fails |
| Network retry | Exponential backoff on network failures |
| Signal handling | Graceful shutdown on SIGTERM/SIGINT |
| PID management | Prevents duplicate instances |
| Log rotation | Rotates logs at 10MB |
| Health checks | Verifies server started successfully |

### Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_DATA_DIR` | Data directory | `~/.local/state/tasks` |
| `TASKS_NET_MAX_RETRIES` | Network retry attempts | `10` |
| `TASKS_RETRY_DELAY` | Initial retry delay (seconds) | `5` |
| `TASKS_MAX_RETRY_DELAY` | Maximum retry delay (seconds) | `300` |
| `TASKS_HEALTH_TIMEOUT` | Health check timeout (seconds) | `30` |
| `TASKS_LOG_MAX_SIZE` | Log rotation size (bytes) | `10485760` |

### Logs

The wrapper logs to `$DATA_DIR/runner.log`:

```bash
tail -f ~/.local/state/tasks/runner.log
```

### Manual Usage

```bash
# Run with web UI
./scripts/tasks-runner.sh --web

# Run headless
./scripts/tasks-runner.sh

# Pass additional arguments
./scripts/tasks-runner.sh --web --port 8080
```

## Self-Update

Tasks includes a self-update mechanism that:

1. Polls `origin/main` for new commits (every 5 minutes)
2. Shows an update banner in the web UI
3. On "Update Now", drains active sessions gracefully
4. Exits with code 100 to signal the wrapper
5. Wrapper pulls, rebuilds, and restarts

### Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_UPDATE_CHECK_ENABLED` | Enable update checking | `true` |
| `TASKS_UPDATE_CHECK_INTERVAL` | Poll interval (seconds) | `300` |
| `TASKS_UPDATE_AUTO_APPLY` | Auto-apply updates | `false` |
| `TASKS_UPDATE_SESSION_TIMEOUT` | Drain timeout (seconds) | `300` |

### Rebuild Scopes

The update system intelligently rebuilds only what changed:

| Scope | Triggers | Rebuilds |
|-------|----------|----------|
| `frontend` | `web/**` changes | bun install + build |
| `server` | `crates/**` changes | cargo build |
| `container` | Supervisor/Containerfile | make container-image |

## Troubleshooting

### Server won't start

1. Check logs: `journalctl -u tasks -f` or `tail -f ~/.local/state/tasks/runner.log`
2. Verify environment file exists and is readable
3. Check port 4800 is not in use: `lsof -i :4800`
4. Verify data directory permissions

### Build failures during update

The wrapper automatically falls back to the previous binary. Check:

1. Rust toolchain is installed and accessible
2. bun is installed (for frontend)
3. Cross-compilation toolchain (for container image)

### Update stuck

1. Check `.update-state` file: `cat ~/.local/state/tasks/.update-state`
2. Remove state files to reset: `rm ~/.local/state/tasks/.update-*`
3. Restart the service

### High memory usage

1. Check memory limits in service file
2. Reduce `TASKS_MAX_SESSIONS`
3. Check container memory with `container list`

## Security Considerations

### Token Security

- Store tokens in environment files with restricted permissions (600)
- Never commit tokens to git
- Use separate tokens for dev/prod

### Network Access

Agent containers have network access for:
- GitHub API
- Package managers (npm, cargo, pip)
- Anthropic API

Consider network policies for sensitive environments.

### Service Hardening

The systemd service includes security options:
- `NoNewPrivileges=true`
- `ProtectSystem=strict`
- `PrivateTmp=true`

Review and adjust based on your security requirements.

---

*See [Configuration](configuration.md) for all environment variables.*
*See [Self-Update Design](../design/self-update.md) for implementation details.*
