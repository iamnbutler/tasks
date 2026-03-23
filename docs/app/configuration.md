# Configuration

Tasks is configured through environment variables and a `.env` file.

## Environment Variables

### Required

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | GitHub personal access token with `repo` scope |
| `ANTHROPIC_API_KEY` | Anthropic API key for Claude access |

### Optional

#### Server

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_DATA_DIR` | Data storage directory | `~/.local/state/tasks/` |
| `TASKS_WEB_PORT` | Web UI port | `4800` |

#### Sessions & Dispatch

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_MAX_SESSIONS` | Global max concurrent sessions | `5` |
| `TASKS_MAX_SESSIONS_PER_PROJECT` | Max sessions per project | `1` |
| `TASKS_MAX_RETRIES` | Max retry attempts for failed tasks | `3` |
| `TASKS_POLL_INTERVAL` | GitHub poll interval (seconds) | `60` |
| `TASKS_DISPATCH_INTERVAL` | Dispatch tick interval (seconds) | `30` |
| `TASKS_ORCHESTRATOR_EVAL_INTERVAL` | Orchestrator evaluation interval (seconds) | `15` |
| `TASKS_PROGRESS_THRESHOLD` | Min session duration to count as progress (seconds) | `60` |

#### Containers

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_CONTAINER_IMAGE` | Container image for agent sessions | `tasks-agent:latest` |
| `TASKS_CONTAINER_MEMORY` | Container memory limit | `8G` |

#### Memory Management

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_MEMORY_WARN_PCT` | Memory % to trigger a warning | `75` |
| `TASKS_MEMORY_SOFT_LIMIT_PCT` | Memory % to pause dispatch | `85` |
| `TASKS_MEMORY_HARD_LIMIT_PCT` | Memory % to emergency-stop sessions | `92` |

#### Cleanup

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_WORKSPACE_STALE_THRESHOLD` | Idle workspace age before cleanup (seconds) | `604800` (7 days) |
| `TASKS_CLEANUP_INTERVAL` | Workspace cleanup scan interval (seconds) | `900` (15 min) |
| `TASKS_CONFLICT_MAX_AGE` | Max age for stale conflict entries before cleanup (seconds) | `86400` (24 hours) |

#### Desktop App

| Variable | Description | Default |
|----------|-------------|---------|
| `TASKS_SERVER_URL` | Server URL (for desktop app) | `http://localhost:4800` |

## .env File

Create a `.env` file at the project root:

```env
# Required
GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
ANTHROPIC_API_KEY=sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxx

# Optional
TASKS_DATA_DIR=/custom/path/to/data
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

Logs are written to stderr. Redirect as needed:

```bash
tasks run --web 2>&1 | tee tasks.log
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

*This documentation is automatically maintained. Last updated: 2026-03-23*
