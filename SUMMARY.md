# Reclaim leaked and orphaned VMs — a slot at the moment of death, an orphan at the next bind

Two distinct leaks are fixed by two mechanisms, and the docs that promised a
single sweep for both are corrected. A **slot leak** (a VM that died while the
pool still counted it) is now reclaimed event-driven, at the instant of death,
from a signal `forward_vm_events` already had and was spending on a `debug!`
line while the pool went on counting the VM until `vm_timeout` — two hours.
`PoolState` is extracted from `Pool` and held as an `Arc` so each per-VM
forwarder can hold a `Weak` to it: enough to free a slot, not enough to start or
stop a VM, and it cannot resurrect a dropped pool. The dead VM's owner can still
`deallocate` it, via a `reclaimed` acknowledgement that is *consumed* — so a
second `deallocate` is `VmNotFound` again and `VmNotFound` keeps its one honest
meaning — and that teardown is still the **full** one, because a supervisor that
died inside a container that is still running looks identical from here. An
**orphan leak** (a VM whose whole daemon went away; `container run` outlives the
process that spawned it) is stopped by the next daemon on that socket, from a
new write-ahead `VmLedger` (`crates/vm-pool/pool/src/ledger.rs`) keyed by socket
path under a new `ServiceConfig::state_dir`. It is discharged strictly *between*
`bind_socket` returning and the accept loop starting — never during
construction, where the safety argument would be circular and a second pool
started against a *live* one would kill that pool's in-flight scouts and Builder
and then exit on `AlreadyRunning`. `Pool::adopt_ledger` (reads, inert) and
`Pool::reclaim_carried_over` (stops) are two calls for that reason, and the
first sits on the impl block *without* the `VmRuntime` bound so the split is in
the types and not only in the prose.

Two limits are stated rather than implied, at every site rather than in a
footnote. Orphan recovery against `ContainerRuntime` is **single-shot**: its
`stop` returns `Ok(())` whether or not `container stop` succeeded, so each
carried id is forgotten after one attempt and the true sentence is "the
successor asked the runtime to stop it", not "it is stopped". The `Err` branch
that keeps an id for the next boot is implemented and tested, and starts working
for free if `stop` ever gets a verdict. What *is* recoverable on every runtime is
an **interrupted** reclaim, and only because `enable` **seeds** the in-memory set
with the carried ids — without that the first `record` or `forget` would rewrite
the file from an empty set and erase every carried id at once. Supporting
changes: `NoRuntime` is now a struct that holds each allocation's event sender
(with it dropped, the forwarder returns immediately and frees the slot it just
filled), `allocate` records write-ahead and spawns the forwarder *after*
`vms.insert` under the still-held write lock, and a ledger is `disabled()` unless
a service enables it, so no test writes into a real state directory. The
store-driven `run::sweep_leaked_vms` is untouched and stays — it asks a *store*
question, the forwarder asks a *liveness* question, the ledger asks a
*runtime-ownership* question, and none of the three subsumes another. Four false
claims found in passing were corrected: `teardown.rs`'s module doc *and* the
runtime message a human reads in the event feed, `scout.rs`, and
`sweep_leaked_vms`'s self-contradictory "the row is cleared either way, so the
next sweep retries whatever did not land"; `report_capacity`'s `NoSlack` warning
now reads as a bounded window rather than a permanent ratchet. No schema, no
migration, no VM image — the pool runs on the host, so this needs no
`make images` to take effect.

Verification: PASSED — `make test`: **756 tests run, 756 passed**, 0 failed (3
slow, 7 leaky). The 7 leaky are the documented expected condition and are
**identical to the baseline**: `make test` on a stashed tree gives 732 passed
with the same 7 leaky test names, so this change adds 24 tests and no leaks.
`cargo test --doc --workspace` (run by `make test`) passes. `cargo fmt --all
--check` clean; `cargo clippy --workspace --all-targets` zero warnings. Note one
deliberate deviation from the spec's own accounting: the spec said `grep -rn
"health loop"` would return nothing, but three hits remain — they are the
*corrected* statements (in `teardown.rs`, `crates/vm-pool/CLAUDE.md` and
`ledger.rs`) explaining that the health loop only ages VMs out at `vm_timeout`
and knows nothing about what a client tracks, which is precisely why the two new
mechanisms exist. Every site that made the false claim is gone. `container stop`
itself is untouched and, being macOS-only, is executed by no test here; the
bookkeeping around it is fully covered.
