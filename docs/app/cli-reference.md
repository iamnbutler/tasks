# CLI Reference

Tasks provides a command-line interface for managing the platform.

## Usage

```bash
tasks <COMMAND> [OPTIONS]
```

## Commands

### `run`

Start the Tasks server.

```bash
tasks run [OPTIONS]
```

**Options:**

| Option | Description |
|--------|-------------|
| `--web` | Enable web UI (serves on port 4800) |

**Examples:**

```bash
# Headless mode
tasks run

# With web interface
tasks run --web
```

### `add-project`

Add a GitHub repository to monitor.

```bash
tasks add-project <REPO>
```

**Arguments:**

| Argument | Description |
|----------|-------------|
| `<REPO>` | Repository in `owner/repo` format |

**Example:**

```bash
tasks add-project iamnbutler/tasks
```

### `remove-project`

Remove a project from monitoring.

```bash
tasks remove-project <ID>
```

**Arguments:**

| Argument | Description |
|----------|-------------|
| `<ID>` | Project ID (use `list-projects` to find) |

### `list-projects`

List all configured projects.

```bash
tasks list-projects
```

**Output:**

```
ID  Repository              Status
1   iamnbutler/tasks        active
2   example/other-repo      active
```

### `rebuild`

Rebuild state from GitHub by clearing tasks and merge queue, then re-polling all tracked projects from scratch. Useful for recovering from data corruption or after the platform has been offline for an extended period.

```bash
tasks rebuild
```

**Behavior:**

- Clears all tasks and merge queue entries from the database
- Preserves: accounting data, event logs, projects table, operating mode
- Re-polls each tracked project and recreates tasks from open issues
- Recreates merge queue entries from open, non-draft PRs

> **Warning:** This is a destructive operation. All current task state (including in-progress agent sessions) will be lost. The platform must not be running (`run`) when you execute this.

**Example:**

```bash
tasks rebuild
# Cleared 12 tasks and 3 merge queue entries
# Polling iamnbutler/tasks...
#   8 issues, 2 PRs processed
# Rebuild complete: 8 tasks, 2 merge queue entries created
```

## Environment Variables

| Variable | Description | Required |
|----------|-------------|----------|
| `GITHUB_TOKEN` | GitHub API token with repo access | Yes |
| `ANTHROPIC_API_KEY` | Anthropic API key for Claude | Yes |
| `TASKS_DATA_DIR` | Custom data directory | No |

## Configuration

Tasks loads configuration from a `.env` file at the project root:

```env
GITHUB_TOKEN=ghp_your_token_here
ANTHROPIC_API_KEY=sk-ant-your_key_here
TASKS_DATA_DIR=/custom/path/to/data
```

## Exit Codes

| Code | Description |
|------|-------------|
| 0 | Success |
| 1 | General error |
| 2 | Configuration error |

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
