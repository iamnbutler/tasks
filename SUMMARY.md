# Reattach to in-flight work on boot instead of writing it off

A `tasks serve` restart no longer destroys the scout, build, or orchestrator
turn that was running. The server was never the thing doing the work — scouts
and builds run under their own supervisors inside VMs, and vm-pool is a
separate daemon that keeps those VMs alive across the restart — so orphaning
them was only ever forced by the absence of a reattach primitive. vm-pool grows
one: `ServiceCommand::Attach` (generic over `AppProtocol`, no app vocabulary
enters the crate) answers whether the pool still holds a VM and replays the
application events recorded for it, and `ServiceEvent::VmApp` now carries the
event log's `seq` so a replay and live traffic splice exactly rather than
approximately. The replay is a *query* over the existing append-only log, not
new retention, and it is bounded by the caller — an unbounded one would be a
single enormous JSON line on a line-oriented socket. On boot the server reads
`vm_id` off every still-`running` session and build, picks up the survivors
(`resume_in_flight`), and only then reconciles what is genuinely gone
(`reconcile_orphaned_work_except`). The invariant that makes that safe is that
a reattach *always concludes its row* — on success, on failure, and on "not
resumable" — since reconciliation now skips the rows it owns. The orchestrator
turn is a local child and cannot be reattached, so it gets the honest
alternative instead: shutdown waits it out rather than aborting it, and a turn
that really was interrupted is reported in the feed at the next boot. Shutdown
also holds the HTTP port through the whole drain, so a restart is a hand-over
rather than an outage.

Three consequences are worth calling out. Reattachment *narrows* orphaning
rather than removing it: `present: false` is not "lost" (a VM reaped after its
run finished still has its terminal event in the replay), but gone-and-silent
still fails the row exactly as before, and a server that cannot reach vm-pool
at all falls back to the old behaviour wholesale. Nothing about a restart is
charged to the work: a session that could not be resumed does not burn a
`dispatch_attempts` strike, and a build that could not be resumed does not
spend one of its specs' build attempts — three restarts would otherwise reject
a perfectly good task or block a perfectly good spec. And replayed output is
deliberately not re-persisted: there is no durable watermark for what the dead
process already wrote, so the transcript states the seam in one marker line
instead of silently doubling its tail (`ScoutEvent::Started` now persists the
branch on arrival, because the bounded window is exactly what drops it). The
new integration tests are real, not seam-level: gated agent fixtures block on a
file the test creates, so a run is provably still in flight when the first
process is killed, and a second process comes up through the same
`resume_in_flight` → `reconcile_startup` sequence `run()` uses — with the build
test asserting the branch actually lands in the remote git repo afterwards.

`cargo test --workspace` plus `cargo test --doc --workspace` (the documented
`test-cargo` fallback; `cargo-nextest` was unavailable in this environment) are
green, and `cargo fmt` / `cargo clippy --workspace --all-targets` are clean.
