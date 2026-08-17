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

## Configuration

`VM_POOL_MAX_VMS` (default `DEFAULT_MAX_VMS` = 6) sizes the pool.

A **slot** is a VM *this pool allocated* — an entry in its own map, what
`allocate` counts and what `PoolStatus::total` reports. Nothing else consumes
one. A VM the container runtime started for its own purposes (`buildkit`,
servicing `container build`) is an ordinary host process this pool never
allocated and does not reconcile against; it costs host memory, not a slot.

**Leave slack.** Allocation at the ceiling is refused, not queued, and a caller
that reads `pool exhausted` as "this work failed" charges it to the work. A VM
whose owner died between allocate and deallocate holds its slot until the sweep
reclaims it, so a pool sized exactly to its steady state is one leak away from
refusing everything.

Three rules the implementation follows, each of which has a way of being
undone by a later "cleanup":

- **Resolution is `max_vms_from_env()`, public and separate from
  `ServiceConfig::from_env()`.** The service has two entry points — this
  crate's `main`, and any embedder that hand-builds a `ServiceConfig` because
  it needs a runtime or an app protocol `main` cannot name. A knob only one of
  them honours is worse than no knob, because it is documented and ignored.
- **It is not in `Default::default()`.** `default()` is what tests and
  embedders build configs with, and one that reads the ambient environment lets
  whoever's shell is running the suite decide what it asserts.
- **Parsing is a pure `max_vms_from(Option<String>)`.** `set_var` is `unsafe`
  in edition 2024 and races every other thread in the test binary, so the tests
  never touch the process environment.

A value that is not a positive integer **refuses to start**. Not a fallback:
`0` binds the socket, answers `status` cheerfully and fails *every* allocate,
silently reproducing the exhaustion the knob exists to configure, and a typo
that defaults back to 6 runs a capacity nobody chose. Unset, empty and
whitespace read as unset, which is a different thing from wrong.

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
- Protocol versioning: `PROTOCOL_VERSION` in `protocol/`, reported by the
  service on `ServiceEvent::PoolStatus` and read back by
  `PoolStatus::speaks(v)`. The service outlives its clients — it is a daemon
  that a client upgrade does not restart — so a new client routinely talks to
  an old one. An added *field* survives that with `#[serde(default)]`; an added
  *command* does not, because an old peer rejects the line at decode time. The
  version rides `status` rather than a new `hello` for exactly that reason: a
  handshake command would be rejected by the very peers it exists to identify,
  whereas `status` has been in the protocol since its first revision and an
  absent field reads as `PRE_VERSIONING` — an answer, not a missing value. Each
  addition gets its own gate constant (`ATTACH_PROTOCOL_VERSION`) so callers
  ask for the capability they need rather than for a bare number; `const _: ()
  = assert!(…)` beside them makes a gate above what the build speaks a compile
  error. Policy about what to *do* with a "no" belongs to the caller — the
  application, never this tree.
- **A live socket has an owner, and this process is not it.** `run()` used to
  `remove_file` the socket path unconditionally and then bind it, so a second
  daemon silently displaced a live one: the first went on listening on an
  unlinked inode — healthy, `pgrep`-able, resolvable by `lsof` (which reads by
  path, and the path had been recreated underneath it) and unreachable forever
  — while the client reconnected to the path, found the *new* pool, and handed
  it the queued work; when the second exited it left a dead socket file plus
  every VM it had started, owned by nobody. `bind_socket` is one `connect`
  ahead of the unlink: something answers ⇒ refuse and name the path, the
  connection is refused ⇒ a dead daemon's leftover, so unlink it and come up
  (the recovery that used to need a human with `rm`). The order is
  `symlink_metadata` → is-it-a-socket → probe → unlink → bind;
  `symlink_metadata` so a dangling symlink is not followed into a stat error,
  and a path that exists and is **not** a socket is refused rather than
  deleted. **Every unreadable answer counts as occupied** — only
  `ECONNREFUSED` and `ENOENT` are free, and a third-kind error or a connect
  that does not return inside `PROBE_TIMEOUT` refuses — because a wrong
  refusal costs one error message and one restart while a wrong takeover costs
  the running daemon every VM it holds. It lives here rather than in a caller
  so both entry points get it (`vm-pool-service`'s `main`, and `tasks
  vm-pool`), and no app vocabulary crosses the boundary. Note that the probe is
  answered by the kernel out of the listen backlog rather than by the accept
  loop, so a daemon wedged *above* its accept still refuses the second start —
  that is the intent, and why the error message leads with "stop the running
  daemon first".
- Testing: TDD with unit tests + integration tests (supervisor spawn)

## Development Phases

1. **Foundation** ✅ — Protocol types with VmId newtype, supervisor with real Execute, real process transport (no mocks)
2. **Images** ✅ — Image types, versioning, filesystem-backed ImageStore
3. **Pool** ✅ — VM lifecycle, allocation limits, health monitoring with timeout enforcement
4. **Snapshots** ✅ — Metadata persistence (Virtualization.framework integration pending)
5. **Service** ✅ — Unix socket API, configurable ServiceConfig, event streaming, error responses
6. **Integration** ✅ — Client crate with full async API and integration tests
