# Tasks Specification

Welcome to the Tasks platform specification.

Tasks is a human-in-the-loop platform that orchestrates coding agents to get project work done. It combines a long-running server, an AI orchestrator, and a fleet of implementor agents into a system where:

- A human describes work in natural language. The orchestrator decomposes it into well-formed issues and dispatches agents to implement them.
- Each task gets an isolated session with its own sandbox, git branch, and agent process. The human can drop into any session to steer, answer questions, or add context.
- A merge queue manages the pipeline from completed work to shipped code. The human controls how much autonomy the system has — from fully manual review to fully autonomous merging.
- An append-only event system provides a complete audit trail of everything that happens across all tasks, agents, and decisions.

## Documents

This specification is organized into the following documents:

### Core Specification

- **[Tasks Platform Specification](./spec.md)** — The main platform specification covering architecture, domain model, operating modes, merge queue, event system, sessions, and more.

### Detail Specifications

- **[Session Runtime](./session-runtime.md)** — Container provider, supervisor protocol, and workspace provisioning. Expands spec.md §10-11, §18.
- **[GitHub Integration](./github.md)** — Normalized model, GraphQL queries, and polling mechanisms. Expands spec.md §12.

### Reference

- **[Symphony (Legacy)](./symphony-legacy.md)** — Historical specification from the predecessor project. Kept for reference.
- **[Contributing](./CONTRIBUTING.md)** — Guidelines for writing and maintaining spec documents.

## Status

Current specification status: **Draft v1**

## Versioning

This specification is versioned. Use the version selector in the navigation to view previous versions of the specification.
