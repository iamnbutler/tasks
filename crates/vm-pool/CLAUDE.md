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
whose owner died between allocate and deallocate holds its slot until that VM's
own event stream ends — which for an agent VM whose owner died early can be
most of an hour — so a pool sized exactly to its steady state spends that
window refusing everything.

`ServiceConfig::state_dir` is where this daemon's own durable state lives —
today just the VM ledger, one file per socket path. Point it somewhere that
survives a reboot: a host that clears `/tmp` would clear exactly the record
needed after a daemon restart. The embedder sets it (`tasks` uses its data
dir); `Default` derives it the same way `snapshot_dir` is derived.

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
- **What that closes is a live incumbent, not a concurrent start.** The probe
  answers for the instant it ran, and nothing holds that answer through the
  `remove_file` and the `bind` after it. Two daemons starting together against
  the same *stale* path therefore both see "nothing is listening": A unlinks
  and binds, then B — already past its own probe — unlinks A's now-live socket
  and binds over it, and A is back on an unlinked inode. That is the
  displaced-daemon failure above, reassembled out of two processes that each
  did the right thing. So the rule is "refuses to displace a live daemon", and
  it must not be read as "binding is safe" — which is why the window is named
  at the unlink and on `bind_socket`'s rustdoc, where the account of the old
  failure is written in the past tense and otherwise reads as closed. It is
  named rather than fixed because it needs two starts inside the same few
  milliseconds against a socket nobody owns, which no workflow here performs
  (an upgrade stops one daemon and starts the next). If it is ever taken, the
  fix is an advisory `flock` on a sibling lockfile — `LOCK_EX | LOCK_NB`,
  taken **ahead of** the probe, held for the process lifetime, refusal
  reported as `AlreadyRunning` — and **the probe stays even then**, because a
  lock cannot see an incumbent that predates it, which is exactly the daemon a
  vm-pool upgrade leaves running. It is a design and not a patch: `flock(2)`
  locks are held per *open file description*, so two opens in one process
  conflict with each other and an fd merely leaked to hold the lock "for the
  process lifetime" would fail the second of the two `bind_socket` calls
  `a_stale_socket_is_reclaimed` makes on one path in one test process. The
  lock has to come back as a guard, which changes a `pub` signature both entry
  points and several tests use; the live-incumbent refusal would then come
  from the lock rather than the probe, so
  `a_live_socket_is_refused_and_its_owner_stays_reachable` keeps asserting
  what it means only if the lock error maps onto `AlreadyRunning`; and
  `vm-pool-service` has no `libc`/`rustix`/`fs2` dependency today, while
  vm-pool stays independently publishable.
- **There are two leaks, and they need two mechanisms.** A **slot leak** is a
  VM that died while this pool still counted it; a **orphan leak** is a VM
  whose whole daemon went away, since `container run` outlives the process that
  spawned it. Neither the health loop nor an embedder's own sweep catches
  either: the health loop only ages VMs out at `vm_timeout` (two hours) and
  knows nothing about what any client tracks, and an embedder's sweep asks a
  *store* question about its own concluded work. The slot leak is fixed
  event-driven, at the instant of death, from a signal `forward_vm_events`
  already had and was spending on a `debug!` line — the end of a VM's event
  stream. The orphan leak is fixed by the *next* daemon on that socket, from
  `VmLedger`: a write-ahead record, keyed by socket path, of the ids this pool
  started. Both survive being described as "the sweep", which is why the docs
  that said so were wrong in a way nobody could act on.
- **The ledger is discharged strictly between `bind_socket` returning and the
  accept loop starting.** Not during construction, where the safety argument
  would be circular: a second pool started against a *live* one would stop that
  pool's in-flight scouts and Builder and then exit on `AlreadyRunning`. After
  the bind it is a proof rather than an assumption — `bind_socket` admits one
  live daemon per socket path, the ledger is named for that path, and no
  command can have been processed because nothing is accepting yet (a client
  that connected in between is sitting in the kernel's backlog with its
  `Allocate` unread). `Pool::adopt_ledger` (reads, inert) and
  `Pool::reclaim_carried_over` (stops) are two calls for the same reason, and
  the first sits on the impl block *without* the `VmRuntime` bound so the split
  is in the types and not only in the prose.
- **Orphan recovery against `ContainerRuntime` is single-shot; an interrupted
  reclaim is not.** `ContainerRuntime::stop` returns `Ok(())` whether or not
  `container stop` succeeded — it `warn!`s a spawn error and `debug!`s a
  non-zero exit — and changing that contract was out of scope. So the honest
  sentence is "the successor asked the runtime to stop it", never "it is
  stopped", and that is the sentence used at every site rather than a footnote.
  An id whose `stop` reports `Err` is kept for the next boot; that branch is
  implemented and tested, and it is what starts working for free if `stop` ever
  gets a verdict. What *is* recoverable on every runtime is a reclaim
  interrupted partway through, and only because `enable` **seeds** the
  in-memory set with the carried ids — without that, the first `record` or
  `forget` rewrites the file from an empty set and erases every carried id at
  once, stopped or not.
- **A ledger, not `container ls`.** Two independent reasons, either fatal. VM
  names carry no daemon identity (`vm-<micros>-<counter>`), and pointing a
  second pool at another `VM_POOL_SOCKET` is a configuration
  `BindError::AlreadyRunning`'s own message suggests — so a sweep that stopped
  every unrecognised `vm-*` would tear down a *live peer's* VMs, which is the
  wrong-takeover `bind_socket` exists to prevent, arriving through a different
  door. And apple/container is macOS-only, so the parser would be the one
  load-bearing line of the fix that no Linux agent or CI run could ever
  execute. The ledger never asks what exists; it remembers what this pool
  started.
- Testing: TDD with unit tests + integration tests (supervisor spawn)

## Development Phases

1. **Foundation** ✅ — Protocol types with VmId newtype, supervisor with real Execute, real process transport (no mocks)
2. **Images** ✅ — Image types, versioning, filesystem-backed ImageStore
3. **Pool** ✅ — VM lifecycle, allocation limits, health monitoring with timeout enforcement
4. **Snapshots** ✅ — Metadata persistence (Virtualization.framework integration pending)
5. **Service** ✅ — Unix socket API, configurable ServiceConfig, event streaming, error responses
6. **Integration** ✅ — Client crate with full async API and integration tests
