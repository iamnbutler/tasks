# The orchestrator's mind: accumulation, capture, and governed autonomy

Design for issues [#820](https://github.com/iamnbutler/tasks/issues/820)
(play mode should run the loop) and
[#821](https://github.com/iamnbutler/tasks/issues/821) (orchestrator
compaction), which are one problem. Resolves #744's open questions 13 ("can
the orchestrator Scout/Build directly?") and 14 ("who decides rejection
routing?").

Nothing in the current implementation is treated as settled here. In
particular the orchestrator's system prompt (`orchestrator.rs:438-515`) is
read as a description of today's behaviour, not as a constraint — we are
designing the orchestrator's decision-making, not preserving it.

## Decisions

**Accumulated context is the product, not an implementation detail.** The
orchestrator is one long-lived serial conversation that builds intuition
across tasks. This is the entire differentiator: without it, the platform is
N parallel Claude Code sessions with extra steps. Every design choice below
is downstream of protecting it.

The evidence is in #820's own record. Of the three real defects orchestrator
review caught — a `0009` migration colliding with one already on `main`, a
spec rebuilding a test harness sitting in an open PR, and "PRs opened" being
asserted as shipped work — the first two were catchable *only* by something
holding other in-flight work in context. Scoped per-spec review misses both.
Serial accumulation is therefore load-bearing, and parallelising verdicts is
rejected.

**The wall of text is the interface.** The human reads the orchestrator's
reasoning as it works, interrupts thinking that's going wrong, and corrects
after the fact. Decisions must be *narrated*, not merely recorded. The
transcript is load-bearing evidence, which is an argument for bounding how
much of it is read at once, never for bounding how much is kept.

**`orchestrator_messages` is three channels wearing one coat.** Conversation
(talking to the human), notification (what happened), and obligation (what is
owed) all live in one table consumed by one watermark. Every pathology in
both issues traces to that conflation, not to its individual symptoms —
patching them one at a time yields five fixes on a wrong shape.

**Accumulate freely, crystallize deliberately, compact aggressively.** Pure
accumulation is the fragile version of the thesis: the window fills, an
undesigned lossy process discards something, and what survives is arbitrary.
Two kinds of context behave differently and must be stored differently —
*narrative* (in flight, this week; decays harmlessly) and *intuition*
(durable, small, must never decay). Crystallization promotes the second out
of the flow so compaction only ever costs the first.

**Curate the window instead of letting the agent forage.** Context is the
budget intuition is bought with. Today the prompt spends 10-50k tokens per
cycle re-deriving state by paging `GET /events` from `since=1`
(`orchestrator.rs:493`). Server-computed briefs buy the same judgment for a
fraction of the window.

**Correction is the learning path.** An interruption saying "no, that's
wrong" is the highest-value token in the system. It currently lands in a
transcript that a failed resume deletes. Routing corrections into durable
case law makes the control mechanism and the learning mechanism the same
loop — which is the argument for audit-and-recourse over pre-approval on its
own terms.

**Authority lives in tables, never in the context window.** Anything the
orchestrator must not forget is rendered into the system prompt from durable
rows every turn, and enforced server-side. The prompt is the one re-seed
channel that already survives everything (`orchestrator.rs:126-145`).

**The orchestrator is custodian of the backlog, not just throughput.** It
owns capturing discovered work that would otherwise be lost, and retiring
work that is done or no longer relevant. This is the "person in charge when
the human isn't" role, and it is distinct from reviewing specs and
dispatching builds.

**Token spend is not a design constraint.** This tool is for someone with
effectively unlimited spend; the scarce resource is the human's attention, not
tokens. So no capability ships rate-limited, throughput beats frugality when
they conflict, and "this might cost a lot" is never on its own a reason to add
a gate. Where a cap exists it is a brake on a *misbehaving* capability, not a
budget. Designs that trade latency or autonomy for cost are the wrong shape
here, and reading a constraint as being about money when it was about control
is the specific error this section exists to prevent.

**Guardrails default off.** Cooling-off `0`, generous caps, escalation async
rather than blocking. They are tuning knobs, not gates. The human gate that
matters is merging to `main`, and the Change Queue from #744 was never built
— that absence is the floor under every mistake made above it, and should be
stated explicitly because the day it lands the risk calculus changes.

**No risk classifier.** Blast radius of a spec — text describing a *future*
diff — is not machine-computable, and a tier gate teaches the agent that
lower claims succeed. The server computes *facts* (staleness, overlap,
collision); the orchestrator makes judgments. Policy contributes mechanical
floors only: attempt caps, and the pipeline's own serialization.

## What today's shape costs

Five pathologies, one cause:

- **A dropped nudge is a dropped verdict.** `tick()` fires only on unanswered
  message rows, and a timed-out turn writes `(orchestrator error: ...)` as
  the reply and *settles the watermark* (`orchestrator.rs:94-112`). The
  obligation was a message; the message is consumed; the spec sits in
  `pending_review` forever and nothing will mention it again.
- **The echo loop.** `nudge_worthy` fires on `SpecQueueStatusChanged{to:
  Approved}` and `BuildCompleted` (`orchestrator.rs:330-350`), so the
  orchestrator is notified about its own actions and spends a turn
  acknowledging them — visible in the live chat today. Under autonomy this
  multiplies, and risks it second-guessing a verdict it already rendered.
- **Unbounded growth.** Obligations and world-state are stored as prose.
  `orchestrator_messages_since` has no `LIMIT`, and the gpui client refetches
  from seq 0 on every SSE-triggered refresh (`app-gpui/src/state.rs:108`), so
  bytes grow as messages × events.
- **Session loss is judgment loss.** Standing authority shares a channel with
  chat, so a failed `--resume` (`orchestrator.rs:139`, warn-level, invisible
  in-app) silently resets what the orchestrator knows it may do and what it
  has learned.
- **Chat and reality can disagree.** A tick that times out *after* the verdict
  curl succeeded writes failure to the transcript while the state change
  stands.

Two mechanical gaps compound these:

- **No build attempt cap.** `finalize_build_failed` returns specs to
  `approved` with no counter. Scouts have `MAX_DISPATCH_ATTEMPTS`; builds
  have nothing. Any automatic dispatch over that is an infinite retry loop.
- **Feedback is a mutable column.** `spec_queue.feedback`
  (`migrations/0001_init.sql:58`) is overwritten by the next verdict, and
  `SpecQueueStatusChanged` carries no actor — so "what did it decide, and
  why" is unanswerable after the fact. This is already out of step with
  CLAUDE.md's "append-only decisions keyed to immutable SHAs".

## The mind: four layers

| Layer | Holds | Lives in |
| --- | --- | --- |
| **World** | what is true now | the DB and GitHub — *queried per decision*, never carried |
| **Standing** | who it is, what it may do, what it has learned | charter + case law, rendered every turn |
| **Working** | what it owes, what it intends | obligations (derived) + commitments (stored) |
| **Conversation** | talking to the human | the chat session — and only this |

The important word is **derived**. An obligation is not written anywhere; it
is computed from pipeline state: *spec in `pending_review` with no decision
row past N minutes*, *approved spec past cooling with no build*, *build failed
under the cap*, *commitment whose trigger fired*, *spec naming discovered work
with no issue*. Obligations are delivered **into the conversation as turns** —
the wall of text is unchanged — but they are re-derived rather than consumed,
so a timeout costs latency, never the work.

## The capability surface

Five capabilities, each independently switchable. Reversibility is the axis
that matters, and it is worth noticing that the two custodial capabilities are
*cheaper* than the pipeline ones we have spent the most time worrying about.

| Capability | Action | Reversibility | Evidence standard |
| --- | --- | --- | --- |
| `capture_work` | file a new issue | trivial (close it) | dedup against open issues + recent captures |
| `retire_work` | close / deem irrelevant | trivial (reopen) | merged PR or named commit, queried live |
| `queue_tasks` | move backlog → queued | trivial (dequeue) | bulk intake never auto-dispatches |
| `dispatch_builds` | approved specs → Builder run | one wasted Builder run | attempt cap; specs already human-approved |
| `auto_review_specs` | render the verdict | one wasted Builder run + a closeable PR | staleness, overlap, verification quality |

### Capture — the orchestrator owns not losing information

Specs routinely surface work outside their own scope: a latent bug found
while exploring, a second call site that needs the same fix, a dependency
discovered mid-implementation. Today that information survives only if a
human reads the spec closely and remembers to file something. Mostly it is
lost.

This becomes an obligation type: *a spec names discovered work with no
corresponding issue*. The orchestrator drafts and files it.

- **Provenance is mandatory.** A captured issue records where it came from —
  "discovered while reviewing `spec_…` for #812" — so its calibration is
  auditable and the human can judge whether the capture instinct is set too
  loose.
- **Dedup is the hard part**, and the failure mode is issue spam. Requires
  searching open issues, checking recent captures, and consulting case law
  ("we decided not to do this"). Belongs in the computed brief (item 5), not
  in the agent's memory.
- **Lands in `backlog`**, not `queued`. Capture and queueing are separate
  capabilities on purpose.

### Retire — recalibration is real work nobody does

Two distinct operations that must not be conflated:

- **Retire the Tasks task.** Tasks-owned state, no GitHub write, fully safe.
  "Not relevant to us right now."
- **Close the GitHub issue.** A GitHub write, and the interesting case.

Closing does *not* violate "never persist a GitHub-owned fact". That rule is
about where truth lives, not about who may act: the server performs the
write, the poller still observes closure as the source of truth, and we never
pre-mark the task closed in anticipation. Write path and read path stay
separate, and the existing "issue closure retires work automatically" flow is
untouched.

The risk is not the write, it is the judgment — and the orchestrator has
already been wrong here, asserting "PRs opened" as a count of shipped work.
So the evidence standard is explicit: **a merged PR referencing the issue, or
a named commit, queried live.** Never an inference from pipeline activity.

The more valuable half is staleness — an issue filed against a subsystem that
has since been rewritten, or superseded by work that took a different shape.
That judgment requires exactly the accumulated context this design protects,
and it is the sweeping-up a human never gets to.

### The first closed loop

`capture_work` plus `queue_tasks` closes the loop: the orchestrator can file
work, queue it, scout it, review the spec, and dispatch the build. This is the
first point at which the system generates its own work, and it deserves to be
named as such.

**There is no governor, and this section originally invented one** (corrected
2026-08-14). It read the manual-queue rule as a *cost* guard and proposed
per-day spend budgets to replace it. That was a misreading: the concern was
never billing, it was that adding a repo with 11,000 issues must not turn into
11,000 Scout runs and 11,000 PRs nobody chose — bulk intake becoming bulk
*work*.

That invariant is already upheld by the pipeline's shape, which is a much
better place for it than a number someone picked. Backlog never dispatches.
`SCOUT_MAX_CONCURRENT` bounds exploration. **Builds are serial** — `build_loop`
awaits each run inline — so the 11,000-PR outcome is structurally impossible
rather than rate-limited. And a batch that keeps failing is retired by the
attempt cap instead of retrying forever.

So no capability ships with a rate limit. Per-day caps remain settable as a
manual brake for a capability caught misbehaving, but they are not a default
and not part of the safety story: the point of the system is that work moves
without being asked, and a cap that fires is a capability that should have been
turned down instead.

CLAUDE.md's manual-queue rule is reworded accordingly — the invariant to
preserve is *bulk intake never becomes bulk work*; deliberate per-task queueing
by an accountable actor is fine.

### The GitHub write path

Both custodial capabilities are GitHub writes, and today the orchestrator
already performs them **outside the governed system**: `GITHUB_TOKEN` is
stripped from the child (`orchestrator.rs:181`) so `gh` authenticates on its
own keychain, and the prompt explicitly permits filing issues. This is the
same side channel through which the `Closes #N` incident happened — no
decision row, no rate cap, no ledger entry.

So this is not adding a capability. It is bringing an existing ungoverned one
inside the system: `POST /issues` and `POST /tasks/{id}/close` on the tasks
server, which perform the GitHub write, create or update the task row, and
record a decision — restoring "GitHub writes go through the server, never
through agents". The `gh` side channel should lose write access in the
orchestrator's configuration once the governed path exists; until then, any
claim that the ledger is complete is false.

## Build order

Items 1-3 are substrate with no design commitments. 4-5 are where the mind is
actually designed. 6-7 are where it earns trust.

### 1. Session ledger + usage parsing + loud rotation

Make the resource the product runs on measurable, and its loss audible.

Parse `usage` in `parse_stream_line` — every field is currently discarded
(`orchestrator.rs:265-300`). **Correction (#827): the `result` record's usage
is not a context size.** It aggregates across every internal turn of one
`claude --print` invocation, each of which re-reads the cached prefix, so it
measures what the tick *spent* — 2.7M on a live server, against a far smaller
window. The absolute reading is the same arithmetic (`input_tokens +
cache_read_input_tokens + cache_creation_input_tokens`) taken off the **last
main-chain `assistant` record**, which is the prompt behind a single model
call and self-corrects even across turns the server never drove. Both are
kept, under names that say which is which: `last_context_tokens` (the gauge)
and `last_tick_tokens` (the bill). Add `orchestrator_sessions`
(`cc_session_id` PK, `started_at`, `ended_at`, `end_reason`,
`last_context_tokens`, `last_tick_tokens`, `summary`), stamp `cc_session_id`
onto message rows, and make the `run_fresh` fallback emit an event plus a
visible seam in the chat.

*Today:* `--resume` fails at 3pm, the chat continues seamlessly, and the thing
writing it has forgotten the morning. You find out when it re-litigates
something settled. *After:* a seam reading "session restarted — prior context
lost", and a gauge saying the last session died at 240k tokens rather than
from a transient crash.

You cannot set a compaction threshold without the number, or audit a verdict
without knowing which memory regime produced it.

### 2. Decisions ledger as an index into the prose

Make the narration queryable without replacing it. Append-only `decisions`
(migration `0012`): subject, actor, verdict, rationale, evidence JSON, and —
load-bearing — **`transcript_seq`**, pointing at the message where the
reasoning lives. Written in the same transaction as the state change (events
append after commit; fine for telemetry, not for an audit trail).
`SpecQueueStatusChanged` grows `actor` and `decision_id`.

Two fixes ride along because they are mechanically dependent:

- **Echo filter.** With an actor on events, `format_nudge` drops self-caused
  ones. This is not information worth having in the wall of text — being told
  what you just did is not thinking.
- **Build attempt cap.** Under item 3, "approved spec with no build" becomes a
  standing obligation, so without a counter one poison batch is an infinite
  Builder loop.

*Later:* "show me everything auto-approved that then failed to build" is a
query, and each row hands you the transcript position where you can read what
it was thinking at the time.

The ledger must exist before item 3, because obligations are *defined* in
terms of it ("no decision row for this spec"). It is also the corpus item 4
is later mined from, so human corrections should be captured here from day
one even while nothing acts on them.

### 3. Obligations derived from state

Stop letting a message row be the only reason work happens. A reconcile pass
computes the open obligation set each tick and delivers obligations into the
conversation as turns; they are re-derived rather than consumed. Nudges demote
from mechanism to latency optimisation.

*Today:* a spec lands, the tick starts, the orchestrator spends nine minutes
reading it and hits the 600s timeout — the error becomes the reply, the
watermark advances, the spec is never reviewed again. *After:* the obligation
is still open next pass; twice-failed obligations escalate rather than
evaporate.

This is the concrete meaning of "don't get blocked, keep working", and it
removes the scariest property of autonomy: that a crash at the wrong moment
drops work silently rather than delaying it.

**The set has to cover the whole pipeline, not just its stalls.** As first
built there were two kinds — `review_spec` and `unblock_spec` — and the gap
between them was the entire reason play mode looked idle after item 6 shipped.
`dispatch_builds` was `live`, so the orchestrator had the authority to batch
approved specs into a Builder run, and *nothing ever asked it to*. Its own
verdicts are filtered out of nudges (correctly — being told what you just did
invites second-guessing it), so approving a spec produced silence, and the
approved spec sat there. Permission with no trigger is not autonomy; it just
looks like an orchestrator waiting to be told.

So `dispatch_build` is a third kind: **an approved spec that no `queued` or
`running` build is carrying.** Queued counts as carried, because builds are
serial and the queue is where a dispatched batch legitimately waits. A build
that fails returns its specs to `approved` and re-raises the obligation on
purpose; the attempt cap is what ends that loop, by moving the spec to
`blocked` and thus to `unblock_spec`.

The general rule this makes explicit: **every state the pipeline can rest in
either is terminal or has an obligation that names who owes what.** A state
that is neither is a place work goes to be forgotten, and the fact that a
capability exists to move it along is no help if nothing says so.

Two supporting pieces, both about the difference between having permission and
using it well:

- The turn tells the orchestrator to **batch** when several specs are unbuilt.
  A Builder run takes a *list* — one branch, one PR — and the obvious reading
  of N obligations is N dispatches, which would scatter related work across N
  PRs. The brief beneath each one already says which specs touch the same
  files, so the judgment is supported where it is asked for.
- The prompt tells it to dispatch **in the same turn it approves**, treating
  the obligation as the safety net rather than the path. Waiting out the grace
  period is a dropped ball being caught, not the system working.

### 4. Case law + crystallization — DEFERRED

**Not built in this pass** (decided 2026-08-14). What makes a correction a
*law* rather than a passing instruction is the least specified thing in this
design, and the honest way to find out is to run the system and look at what
accumulates in the decisions ledger. Deferring is safe: authority comes from
the charter (item 6), not from case law, so item 7's "the summary carries no
authority" property holds without it.

What we give up meanwhile is repeat-mistake immunity. It rewrote a Builder PR
body from `Implements #N` to `Closes #N` and presented it as a bug fix; that
wording is deliberate (`builder.rs:459`, `neutralize_closing_keywords` at
`:489`) and exists so agents cannot write GitHub-owned state. Until case law
exists, a restarted session will make that same "fix" with the same
confidence. With everything `live`, nothing catches that in advance — the
ledger catches it afterwards, which is the trade this design takes everywhere
and the reason item 4 is the one that matters most.

Sketch for when it lands: append-only `orchestrator_notes` (kind `correction`
| `instruction`, content, source decision, `created_at`, `retired_at` —
retire, never delete), rendered into the system prompt every turn.

### 5. Context curation — the computed brief

Per obligation, the server computes what the orchestrator would otherwise
forage for: in-flight builds, open PRs with mergeable state queried live,
recent decisions, staleness of this spec's base against `main`, file overlap
with other pending specs and open PRs, and — for capture — recent captures
and matching open issues. In exchange, the prompt stops instructing it to page
`GET /events` from `since=1`.

*The migration collision, reproduced:* a spec proposes `0009_something.sql`.
Migrations are at `0011`, so this still collides today. Catching it currently
requires the orchestrator to remember or go look; in the brief it is one
computed line — "spec touches `migrations/0009_*`; `0009` exists on main
(`0009_orchestrator_watermark.sql`)". Not a score, not a verdict. A fact that
makes the right judgment cheap.

*The duplicate harness, reproduced:* "files_touched overlaps PR #NNN (open, 4
files in common)". The orchestrator still decides whether that is duplication
or coincidence — that is judgment — but no longer needs to have been paying
attention at the right moment.

This is the direct answer to intuition-per-token, and it converts "caught it
because it happened to remember" into "caught it because it was told".

**As built** (`crates/tasks/src/brief.rs`). Briefs ride the turns where a
decision is made — a spec landing, an obligation coming due — so their cost
tracks decisions rather than time, and every line in one block is read from a
single snapshot so the facts agree with each other.

Two of the plan's items shipped in a different form than written:

- *Sequence-number clashes* generalize the migration case: a filename whose
  numeric prefix is already taken, checked both against the base branch (one
  contents call per affected directory) and against other live specs. The
  second half is the one file-overlap cannot see — `0009_a.sql` and
  `0009_b.sql` share no path.
- *Base staleness was not built*, because nothing records the commit a spec was
  written against. `builds.base_sha` exists; specs have no equivalent, and
  inventing one from the scout session's timestamp would be a guess dressed as
  a fact. Recording a base SHA on the spec is the prerequisite; until then the
  brief does not claim to know.

One property turned out to be load-bearing and is worth stating: **silence in a
brief means unchecked, not fine.** A clean spec gets an explicit "nothing
found" line, a GitHub failure gets a line saying what was skipped, and the
system prompt says the same thing. The failure mode this avoids is the
orchestrator learning to read a short brief as an all-clear, which would make
the feature actively worse than foraging.

### 6. Charter + shadow capabilities

`orchestrator_charter`: one row per capability with level `off` | `shadow` |
`live`, plus optional params. Human-writable only. The
prompt's authority section is **generated** from it, so there is exactly one
statement of what the orchestrator may do — otherwise hand-written prose
saying "reviews are the human's" (`orchestrator.rs:504`) contradicts a charter
that says otherwise, and a degraded session picks whichever it likes.
Enforcement is server-side on the mutating endpoints, because prompt text is
precisely what a restarted session misweighs.

**Shadow means narrated, not silent.** The orchestrator writes the verdict it
*would* render into the conversation — "I'd approve this; verification is
falsifiable, they set `CARGO=/nonexistent-cargo` to prove no shell-outs; base
is 3 commits behind with no overlap. `[shadow: no action taken]`" — and a
decision row is written with `enforced=false`. The human's existing
interception loop becomes the calibration data: after a week, *did shadow
match me?* Flip on evidence, not nerve.

**As built** (migration `0015`, `orchestrator_charter` + `decisions.enforced`).
Two things landed differently from the sketch, both in the same direction —
away from trusting the prompt:

- **Shadow is a server behaviour, not an instruction.** The orchestrator calls
  the endpoint exactly as it would when live; the server records the decision
  with `enforced = 0`, applies nothing, and answers `{"shadowed": true}` rather
  than a normal success body. Telling the agent to narrate instead of acting
  would have made the calibration data depend on prompt compliance, which is
  precisely the thing that degrades — and shadow exists to evaluate a
  capability nobody trusts yet.
- **A shadow verdict discharges its obligation.** Obligations are defined as
  "no decision row", and a shadow row is a decision row. This is right: the
  orchestrator has done everything it is permitted to do, and re-reminding it
  forever would be nagging about work it cannot finish. What remains is the
  human's turn.

Decision rows count *enforced* ones only where any count is taken — a shadow
decision changed nothing in the world, so reading the two as the same thing
would make an evaluation look like a history.

**Shipped defaults** (decided 2026-08-14, then twice revised — the sketch above
survives only as the record of a wrong turn).

The first draft started everything at `off`. Wrong safe: the orchestrator could
already queue, dispatch, and — when told to — review, so an all-off charter
would have *removed* function rather than governed it. The second draft fixed
that but kept `auto_review_specs` and `retire_work` in `shadow`, plus invented
daily caps. Both were struck the same day, for the same reason.

**What ships: all five `live`, none capped.** The charter is a kill switch, not
a promotion ladder.

Shadow's problem is not that it is too cautious. It is that it is the most
expensive possible setting for the resource that is actually scarce here.
`auto_review_specs: shadow` spent one real day in production and the shape was
immediate: the orchestrator read the spec, verified the central claim against
the source, wrote a correct and well-argued verdict — and then handed it back
as prose for the human to read and re-enter by hand. Tokens were never the
constraint; attention was. Shadow spends attention to buy evidence about
whether the agent can be trusted to spend less of it. And the evidence it buys
is inferior to the real thing, because a verdict that costs nothing to be wrong
about is not the same verdict as one that ships.

What makes `live` safe is not a preceding trial. It is the ledger underneath:
every write lands in `decisions` with its rationale and its actor, so a bad call
is visible, attributable, and reversible after the fact. Audit and recourse, not
pre-approval — the same posture as play mode, applied to the charter itself.

That also disposes of the regression the previous draft accepted: "approve spec
X" relayed through the conversation applies again, because `auto_review_specs`
is live. The server still cannot distinguish a relayed instruction from an
autonomous verdict, and the fix is still not a "the human told me to" flag —
that would hand the charter's keys to the thing it governs. The fix is that the
capability is granted, so the distinction stops mattering.

`shadow` stays in the enum, aimed at the case it is genuinely shaped for:
**demotion**. A capability seen making bad calls can be dropped to shadow —
keeping its reasoning in the ledger while it stops acting — or to `off`
outright. That is a response to evidence, which is the direction the evidence
actually flows.

### 7. Owned rotation

The server watches the gauge from item 1 — `last_context_tokens`, and only
that one; `last_tick_tokens` is spend and would fire the threshold on an
expensive tick in an empty session. Past a threshold (env var beside
`ORCHESTRATOR_TIMEOUT_SECS`) it runs a one-shot summarize agent over the
durable transcript — the same shape as briefings — stores the summary in
`orchestrator_sessions`, starts a fresh `--session-id` seeded with it, and
writes the seam. The resume-failure path becomes **the same code path** with
`reason=resume_failed`: today's silent loss and tomorrow's proactive
compaction are one mechanism.

*What the summary may contain:* narrative and commitments — "mid-thread on
orchestrator autonomy; `build_47b3af95` (#810 + #811) is running; committed to
reviewing its PR, resolving the expected Makefile conflict against #816's
`.PHONY` block, merging, then dispatching #812". *What it must not contain:*
anything about what the orchestrator may do. Charter and case law arrive from
tables on the same turn regardless. That property is what licenses running
rotation early and often — a bad summary costs continuity, never authority.

The alternative is CC's built-in auto-compaction, which fires at a threshold
we do not control, discards what it chooses, leaves no seam, and degrades
indistinguishably from the resume bug at `orchestrator.rs:139`.

**Spine:** 1 measures → 2 records → 3 guarantees the work happens → (4 makes
it stick, deferred) → 5 makes it cheap → 6 turns it on → 7 keeps it alive.

## Accepted costs

**The orchestrator is the pipeline's serial bottleneck by design.**
Obligations queue behind ticks under load. The mitigation is curation making
turns cheaper, not parallelism — parallelising verdicts would forfeit the
cross-cutting catches that justify the whole design. Fan-out is acceptable for
read-only *investigation* reporting into the one serial conversation: the
double diamond applied to the orchestrator's own thinking.

**Auditability by narration, not by replay.** A decision that depended on the
whole conversation cannot be re-run against identical context, so "why did it
approve this" is answered by reading rather than re-executing. That makes the
transcript load-bearing evidence — another argument for bounding the read
path rather than the history.

**API-level governance is partial until the side channel closes.** With
`--dangerously-skip-permissions` and its own `gh` auth, the orchestrator can
write GitHub outside every governed path. Item 6 is only as complete as that
configuration allows.

## Open questions

1. **What makes a correction a law?** Deferred deliberately (item 4) — to be
   discovered by running the system and reading what accumulates in the
   decisions ledger, not decided up front.
2. **Does the human review the rotation summary, or just see it?** Editable
   handoff notes are a good product moment and a new surface to build.
3. **Where does the retirement judgment escalate?** "No longer relevant" is
   the one custodial call with no cheap evidence standard.
4. **What does a demotion look like from the human's side?** Now that `shadow`
   is only reached downward, something has to make dropping a misbehaving
   capability a one-gesture move — there is no charter UI at all today.
5. **What is the compaction threshold relative to the model's context?**
   `ORCHESTRATOR_CMD` is configurable, so a hardcoded 200k is wrong in both
   directions.
