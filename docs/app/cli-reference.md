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

Rebuild state from GitHub. Clears all tasks and merge queue entries, then re-polls all tracked projects to reconstruct state from scratch.

```bash
tasks rebuild
```

Requires `GITHUB_TOKEN`. Preserves projects, accounting data, and event logs.

> **Warning:** Destructive. All current task state will be cleared. Use to recover from data corruption or to resync after database issues.

**Behavior:**
- Open issues → tasks (via the standard label-based scheduler rules)
- Open, non-draft PRs → merge queue entries (only if a linked task is found by branch name; PRs without a linked task are skipped)

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
