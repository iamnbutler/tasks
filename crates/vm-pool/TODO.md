# vm-pool TODO

## Architecture direction

vm-pool is pure infrastructure. It manages VMs, pools, priorities, health, and provides a typed channel for passing commands/events through. It does not encode application business logic.

### Key principles

- The supervisor is PID 1, owned by vm-pool. Its children (agents, automation binaries, ssh servers, etc.) are the application's concern.
- The event/command framework should be generic — vm-pool owns pool-level commands (allocate, deallocate, status, snapshot, health), and the application defines its own command/event vocabulary that flows through as typed passthrough.
- Children inside the VM talk to the supervisor via a unix socket. The supervisor multiplexes child events onto the host transport (stdio JSON-line to the pool service).
- No mocks, ever. All tests use real processes.

## Immediate: crates.io publish

- [ ] Write README suitable for crates.io (short, what it is, status, requirements)
- [ ] Add `homepage`, `keywords`, `categories` to workspace Cargo.toml
- [ ] Add `repository.workspace = true` to all crate Cargo.tomls
- [ ] Version: `0.1.0-alpha.1`
- [ ] `cargo publish --dry-run` each crate
- [ ] Publish to crates.io

`ServiceConfig::state_dir` (the VM ledger's home) is a breaking change to
`vm-pool-service`'s public API — it landed before this publish, which is the
cheap order. Anything else of that shape should land before it too, not after.

## Generic protocol ✅

Protocol is now generic over an `AppProtocol` trait. vm-pool handles
infrastructure messages (Ping/Pong/Shutdown/Ready); applications define
their own command/event vocabulary via `AppProtocol::Command` and
`AppProtocol::Event`. Built-ins: `NullProtocol` (no app messages) and
`ShellProtocol` (the original shell-execution behavior, preserved as an
opt-in).

- [x] Split protocol into pool-level commands (fixed) and VM-passthrough commands (generic)
  - Pool commands: `Allocate`, `Deallocate`, `Status`, `Snapshot`, `Restore`, `TailLogs`, `SubscribeLogs`
  - Passthrough: `Send { vm_id, command: P::Command }` where `P: AppProtocol`
- [x] Make `Pool<R, P>` generic over runtime `R` and `P: AppProtocol` (threading `P::Command` / `P::Event` throughout)
- [x] `VmTransport<P>` framing generic over the protocol's command/event types
- [x] Supervisor becomes a library + binary. `run_supervisor<P, H, Fut>` handles infra messages; the binary specializes with `ShellProtocol`.

## Request/response correlation ✅

The host↔service socket muxes command responses and broadcast VM app events
onto one stream. Without correlation the client returned "the next line" as
the response to a command, so an event arriving mid-request was consumed as
the response (request failed, event lost). Concurrent VM workloads were
impossible and single-VM use was racy.

- [x] `Request { id, command }` / `Response { id: Option<u64>, event }`
      envelopes in `protocol` (host↔service only; the VM stdio protocol is
      untouched)
- [x] Service echoes the request id; the VmApp forwarder pushes with `id: None`
- [x] Client reader task routes by id: pending-request map for `Some(id)`,
      event fan-out for `None`
- [x] `ClientHandle` (Clone, `&self` methods) + `subscribe_events()` for
      concurrent use of one connection

Still open:

- [ ] Per-connection request timeouts (a wedged service leaves requests
      pending until the socket closes)
- [ ] Backpressure/flow control for event fan-out (subscribers that fall
      more than 1024 events behind currently lose the oldest)

## Supervisor rework

The supervisor currently just runs shell commands. It needs to become a real process manager.

- [ ] Supervisor listens on a unix socket inside the VM (e.g. `/run/supervisor.sock`)
- [ ] Child process management: start, stop, restart, hot-patch binaries
- [ ] Children connect to the supervisor socket to emit events
- [ ] Supervisor multiplexes child events onto the host transport (stdout JSON-line)
- [ ] Infrastructure commands (ping, shutdown, process management) are supervisor-owned
- [ ] Application commands are forwarded to the appropriate child via the unix socket

## Container runtime

The `ContainerRuntime` is implemented but untested with a real container image.

- [ ] Build a container image with the supervisor baked in as entrypoint
- [ ] Test `ContainerRuntime` end-to-end: start container, send commands, receive events, stop
- [ ] Add DNS config to `VmConfig`
- [ ] Orphan detection: on startup, check for stale containers via `container list`

## Snapshots

Metadata persistence is done. Actual save/restore needs Virtualization.framework.

- [ ] Investigate apple/container snapshot support (may expose save/restore)
- [ ] If not exposed, look into direct Virtualization.framework Swift interop
- [ ] Wire snapshot save/restore into pool lifecycle (pause VM, save state, resume)
- [ ] Prewarm snapshots: boot + initialize, snapshot, restore for instant start

## Event system improvements

- [ ] Per-connection log subscription filtering (currently broadcasts everything)
- [ ] Event persistence to disk (currently in-memory only)
- [ ] Event compaction / retention policy for long-running services
- [ ] `EventLog.events` is an unbounded `Vec` and `attach` scans it linearly.
      Fine at attach frequency (startup only), but the log's growth is the
      real issue — see the retention item above.
