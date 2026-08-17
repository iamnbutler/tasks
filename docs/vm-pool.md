# vm-pool Specification

A standalone service that manages a pool of isolated Linux VMs for running workloads.

## Overview

vm-pool sits between Tasks (the orchestrator) and the raw VM infrastructure (apple/container). It provides:

1. **VM Pool** — Dynamic allocation from a shared pool
2. **Image Management** — Versioned, composable image types
3. **Snapshots** — Fast reset between tasks via save/restore
4. **Event Streaming** — Append-only log with real-time pub/sub
5. **Log Forwarding** — Stream VM logs to external consumers

## Architecture

```
┌─────────────┐     Unix socket      ┌─────────────┐
│    Tasks    │ ◀──────────────────▶ │  vm-pool   │
│  (client)   │   commands/events    │  (service)  │
└─────────────┘                      └──────┬──────┘
                                            │
                        ┌───────────────────┼───────────────────┐
                        │                   │                   │
                        ▼                   ▼                   ▼
                   ┌─────────┐         ┌─────────┐         ┌─────────┐
                   │  VM 1   │         │  VM 2   │         │  VM 3   │
                   │ (agent) │         │ (agent) │         │ (auto)  │
                   └─────────┘         └─────────┘         └─────────┘
```

## Components

### Protocol (`crates/protocol`)

Shared type definitions for:
- **VmCommand** — Commands sent to supervisor inside VM
- **VmEvent** — Events emitted by supervisor
- **ServiceCommand** — Commands sent by Tasks to service
- **ServiceEvent** — Events emitted by service to Tasks

### Supervisor (`crates/supervisor`)

PID 1 process running inside each VM:
- Receives commands over stdin (JSON lines)
- Emits events to stdout
- Manages workload execution
- Forwards logs to host

### Transport (`crates/transport`)

Host-side communication with VMs:
- Spawns VM process
- JSON-line message framing over stdio
- Async send/receive

### Events (`crates/events`)

Event infrastructure:
- Append-only event log
- Pub/sub for real-time streaming
- Per-VM log buffers for tailing
- Event types: lifecycle, logs, protocol events

### Images (`crates/images`)

Image management:
- Image types: base, agent, automation
- Version tags and content digests
- Local storage and caching
- Build via `container build`

### Pool (`crates/pool`)

VM pool management:
- Allocation/deallocation with limits
- Capacity (`PoolConfig::max_vms`, default 6, set with `VM_POOL_MAX_VMS`): a
  slot is a VM this pool allocated, so a VM the container runtime started for
  its own purposes is not one. Leave slack — allocation at the ceiling is
  refused, not queued, and a leaked VM holds its slot until its own event
  stream ends, which for an agent VM whose owner died early can be most of an
  hour
- Reclamation, of two kinds: a VM that dies while this pool still counts it
  frees its slot the instant its event stream ends, and a VM left running by a
  *previous* daemon on this socket is stopped by the next one, off the
  `VmLedger` (`pool/src/ledger.rs`) that daemon wrote ahead of each start
- Health monitoring
- Timeout enforcement
- Lifecycle state tracking

### Snapshot (`crates/snapshot`)

VM state persistence:
- Save via Virtualization.framework `saveMachineStateTo`
- Restore via `restoreMachineState`
- Snapshot storage and metadata

### Service (`crates/service`)

Main binary:
- Unix socket listener
- Command handling
- Event streaming to clients
- Health check loop

## API

The Tasks ↔ vm-pool socket is JSON lines. Because one connection carries both
command responses and asynchronously pushed events, every line is wrapped in a
correlation envelope. (The service ↔ VM stdio protocol is *not* enveloped —
`VmCommand` / `VmEvent` go on the wire bare.)

### Requests (Tasks → vm-pool)

Every command is wrapped in a `Request`. `id` is assigned by the client and is
unique per connection:

```json
{"id": 1, "command": {"type": "allocate", "image": "agent:v1.0.0", "config": {"cpus": 2, "memory_mb": 4096}}}
{"id": 2, "command": {"type": "deallocate", "vm_id": "vm-abc123"}}
{"id": 3, "command": {"type": "send", "vm_id": "vm-abc123", "command": {"type": "execute", "command": "ls -la"}}}
{"id": 4, "command": {"type": "snapshot", "vm_id": "vm-abc123", "name": "clean-state"}}
{"id": 5, "command": {"type": "restore", "vm_id": "vm-abc123", "snapshot": "clean-state"}}
{"id": 6, "command": {"type": "status"}}
{"id": 7, "command": {"type": "tail_logs", "vm_id": "vm-abc123", "lines": 100}}
{"id": 8, "command": {"type": "subscribe_logs", "vm_id": "vm-abc123"}}
{"id": 9, "command": {"type": "attach", "vm_id": "vm-abc123", "since_seq": 0, "limit": 256}}
```

`attach` is how a client picks up a VM it was not following — after its own
restart, say. The workload never stopped and the service never stopped logging
it, so the reply carries whether the pool still holds the VM plus the
application events recorded for it. `limit` is the caller's, and mandatory in
spirit: the reply is one line on a line-oriented socket, and a long-running
workload emits thousands of events. The newest are kept, because a terminal
event is by construction the last one emitted.

### Responses and events (vm-pool → Tasks)

Every line is a `Response`. The direct answer to a command echoes that
command's `id`:

```json
{"id": 1, "event": {"type": "vm_allocated", "vm_id": "vm-abc123", "image": "agent:v1.0.0"}}
{"id": 6, "event": {"type": "pool_status", "total": 6, "available": 4, "allocated": 2}}
{"id": 7, "event": {"type": "log_tail", "vm_id": "vm-abc123", "lines": []}}
{"id": 3, "event": {"type": "error", "message": "send failed: ..."}}
```

Asynchronously pushed events omit `id` entirely:

```json
{"id": 9, "event": {"type": "vm_attached", "vm_id": "vm-abc123", "present": true, "replay": [{"seq": 12, "event": {"type": "command_completed", "exit_code": 0}}], "dropped": 0}}
```

```json
{"event": {"type": "vm_app", "vm_id": "vm-abc123", "seq": 13, "event": {"type": "output", "stream": "stdout", "data": "..."}}}
{"event": {"type": "vm_ready", "vm_id": "vm-abc123"}}
{"event": {"type": "vm_stopped", "vm_id": "vm-abc123"}}
{"event": {"type": "vm_crashed", "vm_id": "vm-abc123", "error": "..."}}
{"event": {"type": "vm_log", "vm_id": "vm-abc123", "stream": "stdout", "line": "..."}}
```

Clients route by `id`: `id` present ⇒ resolve that pending request; `id`
absent ⇒ deliver to the event stream. This is what makes concurrent requests
on a single connection safe — a VM event landing between a command and its
response can never be mistaken for the response.

`vm_app` carries the event log's `seq`, which is what lets a reattaching
client splice a replay against live traffic: subscribe first, then attach,
then discard live events at or below the replay's last `seq`. Subscribing
second would open a window whose events neither source covers. The field is
`#[serde(default)]`, so a peer that predates `attach` still decodes.

`present: false` does not mean the work is lost: if the pool reaped the VM
after the workload finished, its terminal event is still in `replay`. What
counts as terminal is the application's business, not vm-pool's.

## Log Forwarding

VMs emit logs via three streams:
- **stdout** — Workload output
- **stderr** — Workload errors
- **supervisor** — Supervisor internal logs

The service:
1. Captures all VM output
2. Stores in per-VM circular buffers (last N lines)
3. Forwards to event log for persistence
4. Streams to subscribed clients in real-time

Clients can:
- **Tail** — Get last N lines from a VM
- **Subscribe** — Stream logs in real-time
- **Query** — Get historical logs from event log

## Image Types

### Base
Ubuntu 24.04 with common tooling:
- Build essentials, Git, SSH
- Node.js, Bun, Rust
- GitHub CLI

### Agent
Base + interactive agent support:
- Claude Code
- Development tools
- Pre-configured for human-in-the-loop

### Automation
Base + headless execution:
- Minimal overhead
- No interactive tools
- Optimized for CI-style tasks

## Snapshots

Snapshots save the complete VM state (memory + disk) to disk:

```
~/.local/state/vm-pool/snapshots/
├── clean-agent-v1.0.0.vmstate
├── clean-automation-v1.0.0.vmstate
└── custom-checkpoint.vmstate
```

Use cases:
1. **Fast reset** — Restore to clean state between tasks (~5ms vs ~2s cold boot)
2. **Checkpointing** — Save progress during long-running tasks
3. **Prewarming** — Create snapshots after initialization for instant start

## Development Phases

### Phase 1: Foundation
- [x] Repo setup
- [x] Protocol crate with VmId newtype, encode/decode helpers, serde tests
- [x] Supervisor binary with real shell execution
- [x] Transport crate with MockTransport and integration tests

### Phase 2: Images
- [x] Base image Dockerfile
- [x] Image types (agent, automation)
- [x] Images crate with filesystem-backed metadata store

### Phase 3: Pool
- [x] Allocation/deallocation with limits
- [x] Health monitoring with timeout enforcement
- [ ] VM lifecycle with apple/container (ContainerRuntime trait)

### Phase 4: Snapshots
- [x] Snapshot metadata storage
- [ ] Virtualization.framework integration
- [ ] Pool integration for snapshot-based restore

### Phase 5: Service
- [x] Unix socket API with configurable ServiceConfig
- [x] Event streaming to clients
- [x] Error responses for all failure paths
- [x] Log tailing
- [ ] Per-connection log subscription filtering

### Phase 6: Integration
- [x] Tasks client library (vm-pool-client crate)
- [ ] Migration from tasks repo
