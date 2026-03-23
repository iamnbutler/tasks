# Tasks Documentation

Tasks is a human-in-the-loop platform that orchestrates AI coding agents to get project work done. It polls GitHub for issues, dispatches isolated agent sessions in lightweight Linux VMs, and routes results through a quality-gated merge queue.

## Quick Links

- [Getting Started](getting-started.md) - Installation and first run
- [Deployment](deployment.md) - Production deployment and service setup
- [Architecture](architecture.md) - System design and components
- [CLI Reference](cli-reference.md) - Command-line interface
- [API Reference](api-reference.md) - REST API endpoints
- [Web UI Guide](web-ui.md) - Using the web interface
- [Configuration](configuration.md) - Environment and settings

## Overview

### How It Works

1. **Issue Tracking**: Tasks monitors GitHub repositories for issues and pull requests
2. **Agent Dispatch**: When work is identified, isolated container sessions are spawned
3. **AI Execution**: Coding agents (Claude Code) work on tasks autonomously
4. **Quality Gate**: Results pass through human-supervised merge queue
5. **Merge**: Approved changes are merged to the target branch

### Operating Modes

| Mode | Description |
|------|-------------|
| **Play** | Fully autonomous - agents work and approved PRs merge automatically |
| **Pause** | Agents work but merges require manual flush |
| **Stop** | All autonomous activity halted |

### Key Features

- **Session Isolation**: Each agent runs in its own lightweight Linux VM (apple/container)
- **Audit Trail**: Append-only event log for complete transparency
- **Human Control**: Three operating modes give humans control over autonomy level
- **Merge Queue**: Quality-gated approval workflow before changes land

## Project Structure

```
tasks/
├── crates/           # Rust backend
│   ├── app/          # Binary entry point
│   ├── agent/        # LLM abstraction layer
│   ├── models/       # Domain types
│   ├── server/       # Core server logic
│   ├── store/        # SQLite persistence
│   └── ...
├── web/              # React frontend
├── spec/             # Platform specification
└── docs/             # Documentation
```

---

*This documentation is automatically maintained. Last updated: <!-- LAST_UPDATED -->*
