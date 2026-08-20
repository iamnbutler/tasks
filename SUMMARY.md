**This change is inert until someone runs `make images` on a Mac.** The whole
check lives in `builder-supervisor`, which reaches nothing until the Builder
image is rebuilt — until then every build reports `verification: null`, which
renders as "no run on record" everywhere, is never green, and routes every batch
to a human exactly as it does today. That degradation is safe by construction
(`#[serde(default)]` on the new wire field), but a change that looks live and is
not is worth one sentence where it will actually be read. Confirm it in the app
before assuming the check is running.

`SUMMARY.md`'s `Verification: PASSED|FAILED|NOT RUN` trailer was agent-authored
prose, grepped by the host, gating a write to GitHub — a decision resting on text
written by the party being graded, which is the defect `FailureClass` already
forbids one level up. This replaces it with a check: `builder-supervisor` runs
the project's own suite itself, inside the VM, against the **swept** tree the
bundle carries, and stamps a structured `Verification { status, detail }` on
`BuildEvent::Completed`. The larger prize is not where verification happens but
that **a red suite never reaches GitHub** — it fails the build inside the VM as a
`Verdict`, where before it opened a pull request, parked a batch in
`awaiting_merge` and spent a reviewer's attention. `VerificationStatus` is
`Passed | Undeclared | Unavailable | TimedOut` with deliberately **no `Failed`**,
so "shipped and red" is unrepresentable rather than merely avoided; `is_green()`
is the only way anything asks, and the VM wire's forgiving `Deserialize` decays
an unknown status to `Unavailable`, never toward green. The suite is declared at
`.tasks/verify` and read out of the build's **base** commit, so an agent cannot
weaken its own gate; a red run buys exactly one bounded repair round on the same
conversation and worktree; and every state that is not a green run — including a
supervisor image too old to have one — reads as "no passing run backs this". The
trailer, its parser, the app's second parser and the prompt instruction that
produced it are deleted rather than kept as a fallback, and `pr_text` now appends
a **host-authored** sentence generated from the field, which nothing parses back.

The suite runs after the reconciliation and the sweep and immediately before
packaging — the sweep is what turns the working tree into the branch and the
reconciliation is what decides which tip the build *is* (#891) — so `run_build`'s
tail became a loop, because a repair round's commits must travel that same path.
`crates/tasks/src/deadline.rs` grows `Deadline::remaining()`, answered off the
same `Expiry::remaining` the firing decision uses, so the in-VM suite budget is
sized against what the host will actually allow: remaining minus a 120s packaging
reserve, floored at 60s, which is what keeps the outer `BUILDER_TIMEOUT_SECS`
expiry defensible as a `Verdict` rather than a coin toss about which clock fired.

## Review feedback

1. **A red verdict, once reached, must not be erased by an inconclusive
   re-run — done.** `run_build` carries `first_red` separately from the round's
   own observed status: once any round has returned red, no later status may
   package a bundle. Per the direction, this is *not* implemented by making the
   repair round's timeout return red — that would be a lie about what the second
   run observed, and `detail` is read by humans. The observed status stays honest
   and appears in the failure reason ("the re-run then reported timed_out (…),
   which does not overturn a red run"); only the decision to fail is sticky. A
   *first*-round timeout or unavailability still ships, exactly as specified.
   Pinned by `a_red_verdict_is_not_erased_by_an_inconclusive_re_run`, which
   drives it end to end: round one red, the repair round trades the failure for a
   suite that hangs, and the build fails.
2. **Report which gate ruled — done.** `Verification.detail` always names the
   blob SHA of the `.tasks/verify` that ran, matching or not, per the direction
   that a field appearing only on disagreement is one nobody learns to read. The
   comparison is against the **trunk's** copy, not this build's own diff, which
   is the bug the reviewer identified: a stacked build's base already contains an
   earlier build's weakened script. `declaration_changed` is reported, never
   refused. The trunk reaches the VM as a new `BuildCommand::Start.trunk_branch`
   (`SCOUT_BASE_BRANCH`), because `base_branch` is precisely the thing that is
   wrong for a stacked build; an unreachable trunk is reported as an *unmade
   comparison*, never as agreement. Two tests separate the cases:
   `a_branch_that_weakens_its_own_gate_is_still_judged_by_the_base_one` (no
   divergence to report — base *is* trunk) and
   `a_gate_weakened_by_an_earlier_build_in_the_stack_is_reported_against_the_trunk`.
3. **Say what this does not replace, in `CLAUDE.md` — done.** The new design rule
   states both halves: what a green run now guarantees, and that it tested the
   branch against **its own base** and is silent on whether it composes with a
   trunk that moved under it. `landing_section` and `verification_line` say so in
   those words, and the note that "the orchestrator's own run is stronger
   evidence than the Builder's trailer" is now false in its stated reason (both
   are checks) while true for a different one (branch versus composition) is
   written down where the old claim was. Pinned by
   `every_landing_arm_says_a_passing_run_does_not_cover_the_composition` across
   all six (level × can_verify) combinations, and by
   `no_landing_arm_describes_verification_as_something_the_build_reported`.
4. **Do not revert #995's correction — nothing to carry.** #995 landed as commit
   `71e1cb7`, "The landing page for nate.rip/tasks", and it ships **no**
   `.github/workflows/` file: its own message says "deliberately no Actions
   workflow, which is #1015's move since it changes the landing rule". There is
   no `.github/` directory in my base and no repair to those six doc sites. So
   "this repository has no `.github/workflows` and no branch protection" is still
   true as written, and I left every occurrence of it alone — I only rewrote the
   verification half of the sentences I touched. **This is the case the direction
   flagged as the one where my wording will collide with #995 later rather than
   the other way round:** whoever lands the workflow will find my reworded
   `landing_section` arms and `brief.rs` sentences and needs to repair the
   workflow clause in them, which is unchanged from what they would have found
   before this build.

## Directions for this implementation

- **`make images` at the very top, not in a bullet** — done, first paragraph.
- **Read #995's sentences as they exist in my base, and say so if it did not
  land** — done, and it did not land in the relevant sense; see review item 4.
- **Red-is-sticky is about the build, not the round; do not fake the second
  round's status** — done, see review item 1. The observed status and the
  decision to fail are separate values.
- **Report the gate blob SHA always, including on a match; an unavailable trunk
  comparison says so rather than reading as agreement** — done, see review
  item 2.
- **Run `make test` and report the real number** — done, below. The trunk had
  moved well past the 894 the spec measured against: the clean tree is **964**,
  not the 914 the direction estimated, and this change takes it to **992**.
- **Run `cd app-gpui && cargo test --bin tasks-gpui` for the `changes.rs`
  change, and do not conclude the app is broken from `make app-test`** — done;
  213 passed. I used the narrower command and did not run `make app-test`, whose
  link failure on a Linux builder my own spec had already established.
- **Account for every review item under `## Review feedback`** — done above.

## Departures worth flagging

Two, both additive and neither in conflict with the spec.

The spec's `suite_budget_secs` returns a `SuiteBudget` enum (`Run` | `Skip`)
rather than a bare number, because "there is not enough budget to start" and
"the host stated no budget at all" are different facts that must not round off
into one: the first is `TimedOut` and the second is `Unavailable`. And
`BuildCommand::Start` gains `trunk_branch` alongside `budget_secs` — a third
`#[serde(default)]` field, needed for review item 2, since the supervisor cannot
otherwise know what the trunk is (`base_branch` is another build's branch on a
stacked run, and `origin/HEAD` would be a second notion of "trunk" that can drift
from `SCOUT_BASE_BRANCH`).

One thing a reader should know: `.tasks/verify` for this repository runs `make
test-ci`, and I measured it here rather than estimating — **43s warm**, exiting
0. `BUILDER_TIMEOUT_SECS` does not need to rise.

Verification: PASSED — `make test` (992 passed, 0 failed, up from 964 on a clean
tree, i.e. 28 new tests), plus doctests; `cargo clippy --workspace --all-targets`
clean; `cargo fmt --all --check` clean; `make app-check` clean; `cd app-gpui &&
cargo test --bin tasks-gpui` 213 passed; and `sh .tasks/verify` — the gate this
change adds — run directly, green in 43s.
