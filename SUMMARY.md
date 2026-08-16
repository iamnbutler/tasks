`POST /sessions/{id}/cancel` and `POST /builds/{id}/cancel` stop work that is
already in flight — and, the part that matters, they stop it by **interrupting
the dispatcher's drain** rather than by removing its VM. That distinction is the
whole issue: the dispatcher is parked on a vm-pool event stream that a destroyed
VM simply leaves silent, so killing the container by hand (or calling
`deallocate` and nothing else) leaves the row claiming `running`, the serial
build lane occupied, and nothing telling the operator the cancel took. A request
is now recorded as a durable `cancellations` row, which whoever is *following*
the run reads — over the store's event broadcast in the common case, and over a
5s poll for the two things a broadcast cannot cover: a lagged subscriber, and a
request made while nothing was watching, which is exactly what a run picked back
up by `resume_in_flight` after a restart is. The interrupt then travels the same
`tokio::select!` the wall-clock deadline already used (`crates/tasks/src/cancel.rs`,
`biased` toward the work, so an outcome already in hand is never discarded for a
cancel that arrived in the same poll), and teardown continues down the existing
`deallocate_bounded` path.

A cancelled run concludes as `cancelled` — never `failed` — with the actor and
rationale in `exit_reason`, which is the only thing that later tells a
deliberate stop from a crash. It costs the work nothing: no dispatch attempt, no
build strike. A cancelled build's specs go back to `approved` and their tasks to
`ready_to_build`; a cancelled scout keeps whatever it had checkpointed (with the
cancel's rationale stamped onto the notes' `reason`, so the next attempt reads
both the leads and why the last look was called off) and its task returns to the
**backlog** rather than the queue — the one exception to "picked-up work stays
picked up", because leaving it queued has the dispatch loop start a replacement
scout within the tick. A queued build is cancelled by the request itself, since
nothing is following it to notice. The endpoints are governed by a new charter
capability, `cancel_runs`, shipped `live` and uncapped like the other eight: what
makes that safe is the mandatory rationale and the `decisions` row, not a
pre-approval gate, and a cancel that waits for a human arrives after the run it
was meant to stop has finished. The ack's load-bearing field is `concluded`,
which the server polls for up to 3s rather than assuming — `false` means
recorded-and-not-yet-stopped, not failed. Also here: `BuildStatus::Cancelled`
and the previously-unwritten `SessionStatus::Cancelled`, a
`run_cancel_requested` event (whose fields are `run_kind`/`run_id`, because
`kind` is serde's tag for the payload enum and does not compile), client methods,
and a "Cancel Run" row verb in the app, enabled only while something is actually
in flight. Cancelled builds are excluded from the orchestrator nudge, under the
same echo rule as a review verdict. Five integration tests in
`crates/tasks/tests/cancel.rs` run against real vm-pool VMs, real supervisors and
real agents gated on a file that is never created, so every cancelled run
provably could not have ended on its own; they assert the row concluded and the
work came back, because no test that only checks "the VM went away" would catch a
regression here.
