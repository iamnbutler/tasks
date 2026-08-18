Three interacting changes about what a vm-pool refusal means. A Scout whose VM
could not be allocated was charged a dispatch attempt *and* left stranded in
`Scouting` until the next boot, so a momentarily full pool cost a task a restart
and three of them rejected work nothing had judged; a build in the identical
position was spared only by the coincidence that
`finalize_build_unsuccessfully` counts `if started`. The fix is two independent
mechanisms plus the field that made the first of them expressible.
`ServiceEvent::Error` gains a structural `ServiceErrorKind` (vm-pool protocol
revision 2, `#[serde(default)]`, unknown values decaying rather than failing the
decode), `ClientError::Service` carries it, and one function —
`FailureClass::for_service_error` — states the reading for both dispatchers:
`Capacity` is `Transport` because a full pool is a property of the moment, and
everything else including `Image` and `Unspecified` stays a `Verdict` because a
reference that does not resolve refuses identically forever. Separately,
`Scout::start` becomes the boundary so one `match` in `dispatch` undoes the
`Scouting` claim on every pre-session failure, and `pool_health::PoolHealth`
becomes the third dispatch hold beside `github_hold` and `UpdateWatch` at the
same two gates — its evidence a `status` round trip and never a classified
refusal, since the natural clearing signal for the latter is the allocation the
hold prevents. The third change gives `VmRuntime::stop` a verdict: `Ok(())` is
the claim that the VM is not running, `PoolError::StopFailed` is "could not
confirm", and both the reclaim and the ordinary `deallocate` now forget a
ledger id only on the first — so a container that refuses to stop is asked again
by the next daemon instead of being dropped from the only record of its
existence.

The three compose deliberately and stay independent. #967's hold means the
ordinary case stops *making* the refused allocation rather than recovering from
it, so #930's waiver now applies to exactly one thing: the probe-to-allocate
race, five seconds wide, which the unwind makes cost a tick instead of a
restart. The hold reads `status.available` and never #930's `kind`, both because
reading a refusal would close a circle and because `available` is ungated on the
pool running right now while `kind` needs a vm-pool restart to appear at all —
which is also why `run::report_error_kinds` says so on every connect rather than
leaving an operator to find it in a rejected task. #950's `PoolError::StopFailed`
and #930's exhaustive `PoolError::kind()` meet in the same enum and the match is
complete. Reporting follows the two existing holds exactly (`ServerStatus.pool`,
`tasks status`, the app's Server window), printing `0 of N` rather than "full"
because `0 of 0` is a `VM_POOL_MAX_VMS` that can never dispatch and `0 of 6` is
work or a leak holding every slot. No migration, no new environment variable, no
image rebuild — but the vm-pool daemon must be restarted for the `kind` field to
appear, and until it is, refusals read `Unspecified` and are charged, which is
the safe direction and is now announced.

## What this delivers, and what it does not

The stranding is **fixed**, not merely documented. The reviewer of spec 1
required this section to open by saying plainly that a refused task remains
stranded and that only the durable half was fixed — that was true of #930 alone,
and it is the one place the batch overrides a required item: #967 ships in the
same branch and returns the task to `Queued` on every pre-session failure, which
`tests/scout.rs` asserts against a real `pool exhausted` over a real socket.
What is still true, and worth a reader's attention, is that a *permanently* full
pool now holds forever rather than burning attempts — correctly, and visibly on
`/status` — and that nothing here resizes a pool in another process.

## Review feedback

### Spec 1 (#930)

1. **Say plainly that a refused task is still stranded; rewrite the test comment
   that overstates it; quote #967 in the CLAUDE.md note.** Partly declined, and
   this is the one item the batch overrides: #967 is in this branch, so the task
   is *not* stranded after this change. Said so plainly in the section above
   rather than burying it. The `tests/scout.rs` comment was written to claim only
   what the test shows — it states that it drives `Scout::dispatch` directly,
   past the gate, and why that still means what it claims. The CLAUDE.md note is
   the new load-bearing rule rather than a pointer to #967, since the follow-up
   landed.
2. **Put the waiver/stranding coupling in the comment beside the `Capacity`
   arm.** Done, and strengthened to the reviewer's stronger reading: the comment
   on the `Capacity => Transport` arm says the hold is *mandatory rather than
   preferable*, names `crate::pool_health` and the unwind as what "clears by
   itself" actually means, spells out the 500ms `DISPATCH_TICK` consequence, and
   ends "Do not delete the hold and keep this arm."
3. **`reject_exhausted`'s doc comment.** Done. It said `New`, which no path
   writes; it now says `Queued`, names both paths that write it
   (`finalize_failed` and `unwind_unstarted`), and records that before #967 the
   allocate-refusal path wrote nothing at all.
4. **Keep the builder arm and the connect-time warning.** Both kept, as
   instructed.
5. **The `attach`-is-gated / `kind`-is-not distinction is worth one clause.**
   Done, in `ERROR_KIND_PROTOCOL_VERSION`'s own doc and in the CLAUDE.md bullet:
   an old peer rejects an unknown *command* at decode time while an absent
   *field* is rescued by `serde(default)`, so the reasoning behind
   `ATTACH_PROTOCOL_VERSION` does not generalise. The instruction not to make
   this constant a gate is written on the constant.

### Spec 2 (#950)

1. **Do not delete the root `CLAUDE.md` paragraph — it states two limits and
   only one goes away.** Done. It now states three things: a refusal is retried
   across boots and that is new; a reported success is still the CLI's word,
   since `ContainerRuntime` trusts exit 0 rather than verifying the container
   died; and the interrupted-reclaim sentence stands unchanged.
2. **A `NOT_AN_ANSWER` deny-list ahead of `ALREADY_GONE`.** Done, with the
   needles the reviewer named, checked first and decisively, pinned by
   `a_runtime_that_is_not_running_is_not_an_answer_about_the_container` in both
   directions — five runtime-down wordings (including two containing "not
   found") read as failure, and a plain "not found" about a container still
   reads as gone. One consequence is documented rather than hidden: the
   container-subject rule means `container vm-123 is not running`, with an id
   between subject and predicate, matches no needle and resolves to failure. It
   is the safe direction, and loosening it to `isnotrunning` would match "the
   container runtime is not running", which is exactly the reading the rule
   forbids.
3. **One summary `warn!` in `reclaim_carried_over`, not one per id.** Done —
   count, ids, and the *distinct* unrecognised texts, formatted over what the
   loop already holds. The reasoning ("loud" stops being true at three hundred
   ids) is on the function and in both CLAUDE.md files.
4. **The `deallocate` comment.** Done. "The only place a VM leaves the ledger
   through this pool's own work" is replaced by one that describes the branch
   and the distinction under it — the slot is this pool's accounting, the ledger
   entry is a claim about a container.
5. **Fix the `ledger.rs` "no Scout, Builder or CI run" prose while rewriting that
   module doc.** Done — it now reads "no test or CI run on Linux", removing the
   app vocabulary from the vendored tree.
6. **Say plainly that the needle list is unconfirmed.** Stated here: **the
   `ALREADY_GONE` and `NOT_AN_ANSWER` lists have never been run against a real
   `container stop`.** apple/container is macOS-only and this build ran on
   Linux. Whoever runs `container stop does-not-exist-vm; echo $?` on a Mac is
   the first. The failure direction is safe by construction — an unrecognised
   answer retries rather than forgets — and the symptom is a "keeping them in
   the ledger" warning at boot with the exact text to add printed beside it.

### Spec 3 (#967)

1. **The Blockers section is wrong about #930; put the consequence in the
   CLAUDE.md bullet; never wire the hold to `kind`.** Done. The bullet states
   that the two are separate mechanisms both because they answer different
   questions *and* because `status.available` is ungated on the pool running now
   while `kind` needs a vm-pool restart — so there is a window in which the hold
   is the only thing between a full pool and burned attempts. The hold reads
   `status.available` and nothing else; `pool_health.rs`'s module doc says why in
   two numbered reasons.
2. **State and test which loop writes the `Note`.** Done. `probe_due` claims the
   slot, and the announcement is driven by the `Transition` that `observe`
   returns *under that claim* — never by the `hold` predicate, which two loops
   reading every tick would turn into a `Note` per tick. Pinned twice: a unit
   test (`two_loops_racing_across_an_edge_see_one_transition_each_way`) and an
   integration test with a real scout loop and a real build lane sharing one
   record over one real pool, asserting one `Note` on the `Exhausted` edge and
   one on the `Freed` edge.
3. **Keep the reporting half, `0 of N`, `total`, the paused-server probe, and
   both disable-one-half tests.** All kept as specified.

## Directions for this implementation

1. **`PoolError::kind()` must be complete once both land.** Done — `StopFailed`
   answers `ServiceErrorKind::Runtime` (a stop that could not be confirmed is
   the container runtime failing to answer), and the match is exhaustive with no
   wildcard, so a future variant is a compile error rather than a silent
   `Other`.
2. **Do not let #930's end-to-end test survive as a green test that no longer
   exercises a refusal.** Resolved by **driving `Scout::dispatch` directly, past
   the gate**, and saying so in the test's own doc comment. This is sound rather
   than a workaround: #967's hold lives in `run::top_up` and the build lane, not
   in the dispatcher, so a direct call reproduces exactly the probe-to-allocate
   race the hold leaves open — which is the only situation in which the
   classification still applies to a scout. The refusal is real (a real pool of
   one slot with the slot taken by the test, a real `pool exhausted` over a real
   socket), and the test asserts all three halves of the batch: `Transport` /
   `Waive`, no session row and no spec, and the task back in `Queued`. It then
   frees the slot and the same task, from the state the refusal left it in,
   scouts to a spec.
3. **Keep the two mechanisms independent.** Done, and there is no code path
   between them: `pool_health` imports `PoolStatus` and `ClientError` and never
   `ServiceErrorKind`; the two gates never read a `FailureClass`; and the
   classification never reads the health record.
4. **Run `make test` in the foreground; report `#958` if seen.** Done, in the
   foreground, with `cargo-nextest` present. `#958`'s assertion failure
   (`ScoutFailed` where `ScoutStoppedEarly` was expected) did **not** appear.
   The 7 LEAKs are the documented scout-timeout ones.
5. **Account for every review item per spec.** Above.

Verification: PASSED — `make test` → 827 tests run, 827 passed, 0 failed (6
slow, 7 leaky — the documented scout-timeout ones), plus doctests (3 passed
across the workspace, all in `vm_pool_client`). Also `cargo clippy --workspace
--all-targets` clean, `cargo fmt --all --check` clean, `make app-check` and
`make app-test` (204 tests) green, and `cargo doc -p tasks` / `-p
vm-pool-manager` introduce no new warnings (vm-pool is down one: the broken
`crate::ContainerRuntime::stop` intra-doc link went with the paragraph it sat
in).
