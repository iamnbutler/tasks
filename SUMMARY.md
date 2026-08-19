# A Scout that cannot get a VM: classified, not charged; requeued, not stranded (#930, #950, #967)

All three specs are implemented in full, on one branch, with one set of tests
covering the two that are halves of the same change. Nothing is left out.

**#930 — the strike.** A refusal from vm-pool reached this process as nothing
but prose (`ClientError::Service(String)`), so `pool exhausted` and `no such
image` differed only as English and both cost a Scout a dispatch attempt: three
busy moments rejected a task nothing had ever judged. vm-pool's
`ServiceEvent::Error` now carries a `ServiceErrorKind` (protocol revision 2,
`#[serde(default)]`, hand-written `Deserialize` so an unknown kind decays
instead of making the error undecodable — an undecodable error response is
never delivered, which turns a refusal into a hang). `PoolError::kind()` sits
beside the enum, `SnapshotError::kind()` is `Other` throughout on purpose, all
six service sites fill the field, and one function —
`FailureClass::for_service_error` — states the reading for both dispatchers.
Only `Capacity` is waived: the line is *whether the condition clears by
itself*, so `Image`, `Runtime` and `Unspecified` stay charged, `Unspecified`
above all, since that is what a vm-pool older than the field says and it is the
routine case. That the fix is inert until the pool is restarted is now said out
loud once per connect (`run::report_error_kinds`) instead of discovered in a
rejected task.

**#967 — the task.** `Scout::dispatch` claimed the task before it allocated and
the refusal returned before any session row existed, so nothing put the task
back to `Queued` and `next_dispatchable` could not see it again until the next
boot. `Scout::start` is now the boundary and one `match` in `dispatch` unwinds
the `Scouting` claim on every pre-session failure — reading the state back so it
can only undo its own change, best-effort so a bookkeeping failure cannot
replace the error being reported, and writing no `Note` because `record_outcome`
already writes one. Because waiving the strike *removes the backstop* that used
to bound the retry, the hold is mandatory rather than preferable:
`pool_health::PoolHealth` is the third dispatch hold beside `github_hold` and
`UpdateWatch`, at the same two gates, with `status.available` as its evidence
and never a classified refusal. `probe_due` claims the slot, so the two gates
share one round trip *and* exactly one of two racing callers writes the edge
`Note`. It reports on `/status`, `tasks status` and the app's Server window as
`0 of N`.

**#950 — the runtime's verdict.** `ContainerRuntime::stop` swallowed both a
non-zero `container stop` and a failure to spawn it into `Ok(())`, which made
`Err` unreachable in production and reduced orphan recovery to one attempt per
VM. `VmRuntime::stop` now has a written contract — `Ok(())` is a claim about the
world, `PoolError::StopFailed` is "could not confirm" — and both
`reclaim_carried_over` and `deallocate` forget an id only on the claim, so an
unconfirmed stop is carried to the next boot and asked again. `deallocate` still
returns `Ok(())`: freeing the slot and forgetting the id are different
questions. The classification (`reads_as_already_gone`) is pure, unit-tested on
Linux, and resolves an answer it cannot classify to **failure**, because a wrong
failure costs one CLI call and one log line per boot while a wrong success is
the silent leak.

Deployment: `cargo build` plus a **vm-pool restart** — the protocol revision is
host↔service only, so no image rebuild is needed, and until the pool is
restarted #930's waiver does not apply (the connect-time warning says so).

## Review feedback

**Spec 1 (#930)**

1. *Say plainly that a refused task is still stranded.* **Conflicts with the
   batch, and I have followed the batch — this is the one item I did not do as
   written.** #967, which the reviewer filed for exactly this, is spec 3 of the
   same build, so in this tree the stranding is fixed and a summary saying
   otherwise would be false. What the reviewer's underlying point required is
   done: the `CLAUDE.md` rule names #930 and #967 together and says neither half
   works alone, and the overstated end-to-end test comment is rewritten to claim
   only what the test shows — it now says explicitly that nothing in production
   re-dispatches a task sitting in `Scouting` and that the test calls
   `Scout::dispatch` directly.
2. *Put the waiver/stranding coupling in the comment beside the `Capacity` arm.*
   Done, in both places a reader lands: the arm in `scout.rs` and
   `FailureClass::for_service_error`'s doc, each saying the hold is mandatory
   rather than preferable because this waiver removes the backstop.
3. *`reject_exhausted`'s doc is doubly wrong.* Fixed — it said `New` where the
   state is `Queued`, and it now also names the pre-session path and #967.
4. *Keep the builder arm and the connect-time warning.* Both kept, with the
   builder test asserting the two dispatchers return the *same* class rather
   than only that this one is right.
5. *Say in one clause why a field needs no gate where a command does.* Added to
   `ERROR_KIND_PROTOCOL_VERSION`'s doc and to `report_error_kinds`.

**Spec 2 (#950)**

1. *Do not delete the root `CLAUDE.md` paragraph; it states two limits.* Done —
   the paragraph now says three things: a refusal is retried across boots (new),
   a reported success is still the CLI's word (unchanged), and the
   interrupted-reclaim sentence stands verbatim.
2. *Deny-list ahead of `ALREADY_GONE`.* Done: `NOT_AN_ANSWER`
   (`containerruntime`, `daemon`, `socket`, `connectionrefused`, `cannotconnect`,
   `couldnotconnect`, `nosuchfileordirectory`) is consulted first and overrides,
   pinned by a test using a runtime-down wording that contains "not found"
   ("Error: socket not found"). One consequence is stated rather than hidden, in
   the code and in a test: a message naming *both* ("error response from daemon:
   no such container") reads as a failure. That costs one stop per boot and
   announces itself, which is the direction this whole change falls in.
3. *One summary `warn!` rather than one line per stuck id.* Done in
   `reclaim_carried_over`: it collects the count and the *distinct* unrecognised
   texts and warns once. No new bookkeeping — formatting over what the loop
   already held.
4. *The `deallocate` comment.* Rewritten where `forget` became conditional, and
   it now names the other exit (`allocate`'s failure path) that made the old
   sentence imprecise even before this change.
5. *Fix the `ledger.rs` app-vocabulary sentence while rewriting that doc.* Done
   — "no Scout, Builder or CI run" is now "no Linux agent or CI run".
6. **The needle list is unconfirmed against a real `container stop`.** Nobody
   has run `container stop does-not-exist-vm` on a Mac against this list; it is
   written from the shapes CLIs use. The failure direction is the safe one by
   construction (unrecognised ⇒ keep and retry, never forget), and an
   unrecognised failure logs the verbatim text and names `ALREADY_GONE` as the
   constant to extend. Whoever runs that command is the first.

**Spec 3 (#967)**

1. *The Blockers section is wrong about #930.* Accepted and acted on: #930
   waives on a structural `kind`, not a predicate over reason text. The hold is
   deliberately **not** wired to that field — it reads `status.available`, which
   is ungated and works against the pool running right now, whereas the `kind`
   needs a pool restart to become true. The `CLAUDE.md` rule states that
   asymmetry as the reason the two are separate mechanisms.
2. *State and test which loop writes the `Note`.* Done: the announce is driven
   by the `Transition` returned under the `probe_due` claim, never by the `hold`
   predicate; both the module doc and `announce_pool` say so, a unit test pins
   that one probe is claimed per interval however many gates ask, and the
   integration test asserts **exactly one** `Note` on each of the `Exhausted`
   and `Freed` edges with both loops eligible to write it.
3. *Keep the reporting half, `0 of N`, `total`, and both disable-one-half
   tests.* Kept. Disabling the scout gate alone and the build gate alone were
   each run once against this tree: the build half fails exactly as the spec
   predicted (`left: Failed, right: Queued`), and the scout half fails as
   `left: Scouting, right: Queued` rather than the spec's `Rejected` — because
   #930 is merged here, so the loop churns and writes waiver `Note`s instead of
   burning attempts, which is what the spec's own pitfall predicted for that
   combination.

## Directions

- *Read all three before changing anything; they belong on one branch.* Done.
- *Two attempts already charged, neither about the work — start fresh, and an
  honest partial beats an optimistic finish.* Nothing was carried over; all
  three specs are complete, and every claim below is from a run I made.
- *The base is newer than the specs; say which premises are stale.* The one
  premise verified for me (`ContainerRuntime::stop` swallowing everything into
  `Ok(())`) still held. Stale-but-harmless: every line number in all three specs
  has moved (`ContainerRuntime::stop` is at `pool/src/lib.rs:276`, not `:189`;
  `deallocate`'s unconditional `forget` was at `:806`, not `:748`), and spec 3's
  "twelve test call sites in `crates/tasks/tests/run.rs`" is eleven. Nothing in
  #1011 or #1012 conflicted: #1011 rewrote `bridge`, which this change does not
  touch, and #1012 is the service/launchd work, which is orthogonal.
- *#930 and #967 are two halves of one change; make the shared vm-pool edit
  once.* Done — one edit to `crates/vm-pool/pool/src/lib.rs` carries both
  `PoolError::kind()` (#930) and `StopFailed` + the stop verdict (#950), and no
  second waiver mechanism was added beside the first.
- *Do not add a hold — that is #1017 — but do not leave a 500 ms flood either.*
  **This direction conflicts with spec 3, and per the rules I followed the spec
  and am saying so here.** Spec 3 as approved *is* the hold
  (`pool_health::PoolHealth`, named in its title, its summary, its file list and
  its reviewer's required changes), and the direction's other half — no 500 ms
  flood — cannot be satisfied without it once #930 waives the strike that used
  to bound the loop. I read "that is #1017" as referring to a different hold
  than the capacity one spec 3 specifies; if #1017 is in fact this hold, then
  this branch implements it and #967/#1017 should be closed together. What I did
  **not** add is anything beyond spec 3's scope: no runtime-health hold, no
  retry, no backoff, no pre-flight capacity check inside `dispatch`.
- *Live evidence: a down container runtime charged twelve tasks and stranded
  every one.* That shape is covered by the unwind (the task returns to `Queued`
  whatever refused it) but only partly by the hold: a runtime that is down while
  vm-pool is up still answers `status` with free slots, so dispatch proceeds and
  the failures are charged as pre-agent setup failures, which the strike rule
  deliberately keeps charged. Making *that* legible is a runtime-health signal
  and is not in any of these three specs — flagging rather than smuggling it in.
- *Run the suite and report what happened.* Reported below and in this section;
  `make test` was run to completion on this tree.
- *`SUMMARY.md` is the PR body, accounting for each spec by number.* Above.

Verification: PASSED — `make test` (892 tests, 892 passed, 0 failed; 6 slow and
7 leaky, all in the documented scout-timeout/cancel family plus the two new
pool-hold tests, which sleep by design), plus `cargo test --doc --workspace`
green as part of it, `cargo clippy --workspace --all-targets` clean, `cargo fmt
--all` clean, and `make app-check` / `make app-test` green for the GUI half.
