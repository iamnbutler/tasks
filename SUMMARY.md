# Reclaim leaked and orphaned VMs, and make the docs describe what actually happens

The issue reported that no sweep exists and that `CLAUDE.md` describes one. The
second half is right and the first is not, which is what changes the fix: a
sweep does exist — `run::sweep_leaked_vms`, on every vm-pool connect — but it
lives on the tasks side, outside the `crates/vm-pool/` tree the issue grepped,
and it answers a narrower question than six documentation sites claimed for it.
It is driven entirely by the database and never asks the runtime anything, so it
could not have reclaimed the four orphaned VMs from the incident: their pool was
gone, and `Pool::deallocate` answers `VmNotFound` for an id its map never had,
*before* it ever reaches `runtime.stop`. That is why a human had to stop those
containers by hand. So there are two distinct leaks here and this ships a
mechanism for each. A **slot leak** — a VM the pool still counts and no longer
has — is now reclaimed event-driven, at the instant of death, from a signal the
pool already had and was discarding: the end of a VM's event stream is an exact
statement that the VM is gone, and it was being spent on a `debug!` line while
the pool went on counting the VM until `vm_timeout` aged it out two hours later.
An **orphan leak** — a VM whose whole daemon went away — is reclaimed at the
next daemon's boot from `VmLedger`, a write-ahead record of what this pool
started, kept at `<state_dir>/vms-<socket>.json` and discharged inside
`Service::with_runtime`, before `run()` binds the socket, so a pool never
advertises capacity its predecessor's VMs still consume.

The ledger is deliberately a *record of what this pool started*, never an
*inventory of the host*: VM ids carry no daemon identity and running a second
pool on another `VM_POOL_SOCKET` is a configuration `BindError::AlreadyRunning`
itself suggests, so a `container ls` sweep would tear down a live peer's VMs —
the wrong-takeover `bind_socket` exists to prevent, through a different door —
and apple/container is macOS-only, so that parser could never be executed by a
test or by CI. Its safety is a proof instead: one live daemon per socket path,
and the file is named for that path. Around it, `PoolState` is extracted from
`Pool` so a forwarder holds a `Weak` — enough to free a slot, not enough to
start or stop a VM; `deallocate` is idempotent for a VM the pool reclaimed
itself (via a set whose entry the first call consumes) while still running the
full teardown, because a closed transport means the *host* side died and a
supervisor that died inside a live container looks identical from here; and
`VmNotFound` keeps its one meaning, so no client can stop a container by
guessing a name. The tests drive a genuine VM death with `kill -9 $PPID` inside
the supervisor's own `sh -c` child — real processes, on Linux, no container
runtime — and assert the freed slot actually carries work again. Finally the
documentation: all six sites now say which mechanism ends which leak and how
long it takes, plus two claims that were outright false — `teardown.rs`'s "its
health loop reaps VMs the server stops tracking" (it knows nothing about what
the server tracks), which is why abandoning a teardown felt safe, and
`sweep_leaked_vms`'s self-contradictory "the row is cleared either way, so the
next sweep retries whatever did not land".

Verification: PASSED — `make test-ci` (722 tests run, 722 passed, 7 leaky —
the documented expected condition for the scout-timeout tests) plus its
`cargo test --doc --workspace` (exit 0); `cargo fmt --all --check` clean and
`cargo clippy --workspace --all-targets` with zero warnings.
