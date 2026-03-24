# Configuration

Tasks is configured through environment variables and a `.env` file.

## Environment Variables

### Required

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | GitHub personal access token with `repo` scope |
| `ANTHROPIC_API_KEY` | Anthropic API key for Claude access |

### Server

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_DATA_DIR` | Data storage directory | `~/.local/state/tasks/` |
| `TASKS_WEB_PORT` | Web server port | `4800` |

### Session Limits

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_MAX_SESSIONS` | Global max concurrent agent sessions | `5` |
| `TASKS_MAX_SESSIONS_PER_PROJECT` | Default max sessions per project | `1` |
| `TASKS_MAX_RETRIES` | Max retry attempts for failed tasks | `3` |

### Timing

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_POLL_INTERVAL` | GitHub poll interval (seconds) | `60` |
| `TASKS_DISPATCH_INTERVAL` | Dispatch tick interval (seconds) | `30` |
| `TASKS_ORCHESTRATOR_EVAL_INTERVAL` | Orchestrator evaluation interval (seconds) | `15` |
| `TASKS_PROGRESS_THRESHOLD` | Minimum session duration to count as progress (seconds) | `60` |
| `TASKS_WORKSPACE_STALE_THRESHOLD` | Age at which idle workspaces are cleaned up (seconds) | `604800` (7 days) |
| `TASKS_CLEANUP_INTERVAL` | Workspace cleanup scan interval (seconds) | `900` (15 min) |
| `TASKS_CONFLICT_MAX_AGE` | Max age for stale conflict entries before cleanup (seconds) | `86400` (24 hours) |

### Container

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_CONTAINER_IMAGE` | Container image for agent sessions | `tasks-agent:latest` |
| `TASKS_CONTAINER_MEMORY` | Container memory limit | `8G` |

### Memory Pressure

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_MEMORY_WARN_PCT` | Memory % at which to log a warning | `75` |
| `TASKS_MEMORY_SOFT_LIMIT_PCT` | Memory % at which to pause dispatch | `85` |
| `TASKS_MEMORY_HARD_LIMIT_PCT` | Memory % at which to emergency-stop sessions | `92` |

> Memory thresholds must be ordered: `WARN < SOFT < HARD`, or the server will refuse to start.

### Desktop App

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_SERVER_URL` | Server URL (for desktop app) | `http://localhost:4800` |

### Logging

| Variable | Description | Default |
|----------|-------------|---------|
| `RUST_LOG` | Log level filter | `info` |

### Self-Update

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_UPDATE_CHECK_ENABLED` | Enable update checking | `true` |
| `TASKS_UPDATE_CHECK_INTERVAL` | Check interval (seconds) | `300` |
| `TASKS_UPDATE_AUTO_APPLY` | Auto-apply updates | `false` |
| `TASKS_UPDATE_SESSION_TIMEOUT` | Session drain timeout (seconds) | `300` |

### Wrapper Script

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_NET_MAX_RETRIES` | Max network retry attempts | `10` |
| `TASKS_RETRY_DELAY` | Initial retry delay (seconds) | `5` |
| `TASKS_MAX_RETRY_DELAY` | Maximum retry delay (seconds) | `300` |
| `TASKS_HEALTH_TIMEOUT` | Health check timeout (seconds) | `30` |
| `TASKS_LOG_MAX_SIZE` | Log rotation size (bytes) | `10485760` |

## .env File

Create a `.env` file at the project root:

```env
# Required
GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
ANTHROPIC_API_KEY=sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxx

# Optional
TASKS_DATA_DIR=/custom/path/to/data
TASKS_MAX_SESSIONS=5
TASKS_WEB_PORT=4800
```

## GitHub Token

### Required Scopes

Your GitHub token needs the following scopes:

| Scope | Purpose |
|-------|---------|
| `repo` | Full repository access |
| `read:org` | Read organization data (if using org repos) |

### Creating a Token

1. Go to GitHub Settings → Developer settings → Personal access tokens
2. Click "Generate new token (classic)"
3. Select the required scopes
4. Copy the token to your `.env` file

## Anthropic API Key

### Getting a Key

1. Sign up at [console.anthropic.com](https://console.anthropic.com)
2. Navigate to API Keys
3. Create a new key
4. Copy to your `.env` file

### Rate Limits

Be aware of Anthropic API rate limits. Tasks uses Claude for:

- Orchestrator decision-making
- Agent coding sessions
- Quality evaluation

## Data Directory

### Default Location

```
~/.local/state/tasks/
├── db.sqlite           # SQLite database
├── server.log          # Server log file (auto-truncated at 3000 lines)
├── events/             # Event logs (per-task)
│   └── {task-id}/
│       └── events.jsonl
└── workspaces/         # Container workspaces
```

### Custom Location

Set `TASKS_DATA_DIR` to use a different location:

```env
TASKS_DATA_DIR=/var/lib/tasks
```

### Permissions

Ensure the data directory is writable:

```bash
mkdir -p ~/.local/state/tasks
chmod 755 ~/.local/state/tasks
```

## Container Configuration

### Cross-Compilation

The supervisor binary must be cross-compiled for the container target:

```bash
# Verify toolchain
make check-linker

# Build supervisor + container image
make container-image
```

### Container Runtime

Tasks uses [apple/container](https://github.com/apple/container) for isolation. Containers are lightweight Linux VMs, not Docker containers.

## Logging

### Log Level

Control logging verbosity with `RUST_LOG`:

```env
RUST_LOG=info          # Default
RUST_LOG=debug         # More verbose
RUST_LOG=tasks=debug   # Debug only for tasks crates
```

### Log Output

Logs are written to stderr and also to `~/.local/state/tasks/server.log`. Redirect stderr as needed:

```bash
cargo run -- run --web 2>&1 | tee tasks.log
```

## Security Considerations

### Token Security

- Never commit `.env` files to git
- Add `.env` to `.gitignore`
- Use separate tokens for development and production

### Network Access

Agent containers have network access for:

- GitHub API calls
- Package manager downloads
- Anthropic API calls

Consider network isolation for sensitive environments.

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
