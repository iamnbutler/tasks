# vm-pool

A standalone service that manages a pool of isolated Linux VMs for running workloads.

## Project Structure

- `crates/` — Rust crates
  - `protocol/` — Shared command/event type definitions (VmId, VmCommand, VmEvent, ServiceCommand, ServiceEvent)
  - `supervisor/` — PID 1 binary that runs inside VMs, executes shell commands
  - `pool/` — VM allocation, limits, health monitoring; includes transport, events, images, snapshot modules
  - `service/` — Main binary + library, Unix socket API with configurable ServiceConfig
  - `client/` — High-level async client for communicating with the service
  - `test-support/` — Test-only helpers (locating the binaries tests exec);
    dev-dependency only, never depended on by shipping code
- `images/` — Dockerfiles for each image type
  - `base/` — Ubuntu + common tooling
  - `agent/` — Base + Claude Code + dev tools
  - `automation/` — Base + minimal tooling for headless tasks
- `spec/` — Design documents

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

## Building

```sh
cargo build                          # build all crates
cargo build -p vm-pool-supervisor   # build supervisor only
cargo test --workspace               # run all tests
```

## Testing

No mocks — the pool tests spawn a real supervisor process. That binary lives
in a sibling package, so `CARGO_BIN_EXE_*` is not available to the tests that
need it, and the obvious fallback (`cargo build` inline) takes cargo's
build-directory lock: every such call is a place the suite can stall behind
rust-analyzer, an editor save hook, or a build in another terminal.

So tests never shell out to `cargo build` themselves. They call
`vm_pool_test_support::supervisor_binary()`, which:

1. uses `$VM_POOL_TEST_BIN_DIR/<bin>` (then `/<package>`) when that exists,
2. otherwise builds — once per test process, memoized, using `$CARGO` so a
   non-default toolchain stays consistent with the outer `cargo test`.

Prebuild the directory and export the variable:

```sh
cargo build -p vm-pool-supervisor
VM_POOL_TEST_BIN_DIR=$PWD/target/debug cargo test --workspace
```

The tasks workspace's `make test` does exactly this. The variable is
vm-pool-local by design: vm-pool is vendored infrastructure and must stay
independently testable and publishable, so it does not read the host
project's equivalent (`TASKS_TEST_BIN_DIR`). A bare `cargo test` with nothing
exported still works — it just builds once.

Note the package/binary names differ: package `vm-pool-supervisor` declares
`[[bin]] name = "supervisor"`, so the file cargo writes is `supervisor`. The
helper checks the bin name first and the package name second, so pointing the
variable straight at `target/debug` works with no copying.

## Key Decisions

- Host runtime: Rust (edition 2024)
- VM backend: apple/container (Virtualization.framework)
- Host ↔ service IPC: Unix socket with JSON-line protocol, framed in
  `Request { id, command }` / `Response { id: Option<u64>, event }` envelopes.
  The service echoes the request id on the direct response; pushed events
  (VM app events) carry no id. This lets one connection multiplex concurrent
  requests and event streaming. See `spec/spec.md` → API.
- Host ↔ VM communication: JSON-line protocol over stdio
- Image format: OCI (built with `container build`)
- Snapshots: Virtualization.framework save/restore APIs
- Type safety: VmId newtype, strongly-typed protocol enums
- Testing: TDD with unit tests + integration tests (supervisor spawn)

## Development Phases

1. **Foundation** ✅ — Protocol types with VmId newtype, supervisor with real Execute, real process transport (no mocks)
2. **Images** ✅ — Image types, versioning, filesystem-backed ImageStore
3. **Pool** ✅ — VM lifecycle, allocation limits, health monitoring with timeout enforcement
4. **Snapshots** ✅ — Metadata persistence (Virtualization.framework integration pending)
5. **Service** ✅ — Unix socket API, configurable ServiceConfig, event streaming, error responses
6. **Integration** ✅ — Client crate with full async API and integration tests
