## An `attach` vm-pool cannot decode degrades to reconciliation instead of failing the run

vm-pool is a separate long-lived daemon that a server restart does not restart,
so a freshly built server routinely talks to a service running an older binary.
Against one that predates `ServiceCommand::Attach` (#842), the service rejected
the line at decode time, the client surfaced that as an ordinary
`ClientError::Service`, and `Scout::reattach` / `Builder::reattach` — which are
contractually obliged to conclude the row they were handed — wrote the run off
as FAILED. Both runs were alive and recoverable: the code path that exists to
save work was destroying it. This makes "can this peer be attached to at all?"
a question about the *service*, asked once per boot **before any row is
claimed**, and answers it with a version the service reports on `PoolStatus` —
a command every version that has ever run already answers.

`vm-pool-protocol` gains `PROTOCOL_VERSION` / `PRE_VERSIONING` /
`ATTACH_PROTOCOL_VERSION` (with `const _: () = assert!(…)` beside them, so a
future bump that leaves the gate above what the build speaks is a compile error
rather than a silent, permanent "cannot reattach") and a `#[serde(default)]
protocol_version` on `ServiceEvent::PoolStatus`. The version rides `status`
rather than a new handshake command deliberately: a `hello` would be rejected
by exactly the peers it exists to identify, whereas `status` has been in the
protocol since its first revision and an absent field decodes as
`PRE_VERSIONING` — an answer, not a missing value. The client exposes
`PoolStatus::speaks(v)`; `crates/tasks` owns the policy as
`reattach::AttachSupport` / `attach_support()`, and `resume_in_flight` returns
empty — claiming nothing — when the answer is too old, unanswerable, or
unreachable, so `reconcile_startup` writes the rows off exactly as a server
without reattachment did. `dispatch_loop` reports the skew on every connect
(not a gate: an old daemon still runs scouts and builds fine), because the bill
otherwise only arrives at the next restart, by which point the work it costs is
already in flight. Note that the operational fix is unchanged — a running
vm-pool must still be restarted before it can serve `attach`; what changes is
the cost of forgetting.

Tests are real peers, no mocks: protocol round trips including an old-shaped
`pool_status` line, a client test against a raw Unix-socket peer emitting the
old wire form, and
`a_vm_pool_that_predates_attach_falls_back_to_reconciliation`, which stands up
a fake pre-attach vm-pool on a real socket holding both a `running` session and
a `running` build and asserts nothing is claimed, both rows are written off
with `exit_reason == "orphaned by server restart"` (`"attach failed: …"` would
mean the server claimed the row and killed the run itself), the spec is still
`Approved` with its task back to `ReadyToBuild`, and — the crux — that `attach`
was never sent at all. It fails when the gate is removed. `cargo fmt` and
`cargo clippy --workspace --all-targets` are clean and 490/491 nextest tests
plus doctests pass; the one failure,
`tasks::reload when_idle_waits_for_the_drain_and_restores_the_mode`, times out
at 60s on a clean checkout of `main` too (verified by stashing) — pre-existing
and unrelated, and worth its own issue.
