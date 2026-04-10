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

Rebuild local state from GitHub. Clears all tasks and merge queue entries, then re-polls all tracked projects to recreate them from open issues and PRs. Accounting data and project configuration are preserved.

```bash
tasks rebuild
```

Use this to recover from a desync between local state and GitHub (e.g. after a crash, manual database changes, or a prolonged outage).

**Output:**

```
Cleared 12 tasks and 3 merge queue entries
Polling iamnbutler/tasks...
  15 issues, 4 PRs processed

Rebuild complete: 12 tasks, 3 merge queue entries created
```

> This command runs synchronously and reports results directly. The API equivalent (`POST /api/rebuild`) triggers re-population asynchronously via the GitHub poller.

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

*This documentation is automatically maintained. Last updated: 2026-04-10*
