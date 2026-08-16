The pipeline dead-ended at PR open. Half of #881 already shipped in PR #894 —
`TaskState::AwaitingMerge`, `watch_merges`, `ObligationKind::LandBatch`, the
Queue's **Awaiting merge** band — and it is verified here and left alone. What
was missing is that nothing drove a PR to landed, and the cause was one
sentence: the `land_batch` prompt bullet ended "landing it is the human's"
while the charter shipped `land_builds` **live**. That sentence is now
*generated* from the charter row (`orchestrator::landing_section`), the way the
authority and workdir sections already are, so the prompt and the endpoint can
no longer disagree. Under the shipped default it says landing is the
orchestrator's, that waiting is not the default, and names exactly three
carve-outs as the whole list — GitHub would refuse the merge, the build
reported no passing test run of its own, or nothing runnable here could have
checked it — because "hand it over when in doubt" is what the old sentence
effectively said and doubt is unbounded. The facts those questions need land on
the *brief*, where every other GitHub fact is read: `PrState` now carries
`mergeable_state` (off the body `pull_request_state` already fetches, so no
extra call) and `PrState::landing()` reads it before `mergeable`, which is
`false` only for a conflict and so reads a red PR as ready; the Builder is
asked for a `Verification: PASSED|FAILED|NOT RUN` trailer in SUMMARY.md, parsed
back by `builder::verification_report`, since with no workflows and no branch
protection in this repository GitHub's verdict is structurally incapable of
objecting to a change that does not work; and `verification_surface` states how
much of a batch sits under `app-gpui/`, narrowly, because the app compiles and
unit-tests on a Linux builder and only its rendering takes a Mac. Anything the
parser does not recognize is `Unreported`, never a pass — so every batch parked
before the trailer existed reads as "no run on record" and goes to a human,
which is the direction a mistake here has to fall. Stacked builds still spend
one `merge_reached_trunk` and only when stacked, asking about the merge commit
once merged and the base branch while open; `GITHUB_BUDGET` now bounds the
whole brief rather than each call inside it; and `pipeline()` names each parked
batch from `list_builds_awaiting_merge` rather than `world.builds`, which
recency-filters at 14 days and would drop exactly the batch stranded longest.
There is no migration, no `BuildEvent` field and no builder-image rebuild:
mergeability is read live and never cached, and the verification line rides the
summary that is already stored and already the PR body.

The second spec (#883) reports a test that no longer fails — `cae70ef` fixed
the instance — so this is the part that commit did not do. `poll`, `nudge` and
`obligations` were awaited **unbounded** and unnamed at shutdown while
scouts/builds and the orchestrator turn already had leashes, which is how a
one-line bug (a loop that never observed the shutdown flag) reached the
operator as a 75s SIGKILL with nothing in `serve.log` naming it.
`run::drain_background` now awaits all three under one shared 10s deadline and
warns per stuck loop by name; shared rather than per-task, so the whole drain
is 10 + 30 + 30 = 70s and still fits inside the 75s `reload` allows, and loops
after the deadline are still asked so the log names the one that is stuck
rather than the first handle awaited. Alongside it, `TASKS_ENV_FILES=off`
closes a live route by which ambient config decided that suite's result:
`Command::env_remove` is the *opposite* of a scrub, since the real environment
is the only thing a `.env` entry loses to, so removing `TASKS_DEFAULT_MODE`
from a child is precisely what lets this (gitignored) checkout's `.env` decide
it. The new test carries a control that proves the file really can win, without
which the assertion is vacuous, and an unreadable switch value refuses to boot
rather than being ignored back into the behaviour it was turning off. The two
`--drain-timeout 60` values become `DRAIN_TIMEOUT = 20`, since 60 sat exactly
on nextest's kill threshold and a drain that genuinely timed out could never
print its own assertion.

One thing outside both specs was fixed because it was already broken: `app-gpui`
did not compile at all on the branch base — `cancel_refusal`'s match never
gained a `TaskState::AwaitingMerge` arm when PR #894 added the state, and
`app-gpui` is not a workspace member so `make test` cannot see it. One arm
restores it; `make app-test` is 113 passing again. Verification: `make test` →
**604 passed, 0 failed** (the 6 leaky and 2 slow are the documented
scout-timeout/cancel tests, confirmed identical on the branch base), doctests
pass, `cargo clippy --workspace --all-targets` is clean, `cargo fmt --all` is
clean, and `make app-test` passes. Two pre-existing `cargo fmt` diffs in
`app-gpui/src/{sections/detail.rs,state.rs}` are left alone rather than swept
into this diff.

Verification: PASSED — make test (604 passed, 0 failed), cargo test --doc, cargo clippy --workspace --all-targets, cargo fmt --all --check, make app-test (113 passed)
