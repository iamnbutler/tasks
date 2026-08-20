# Tasks (v2)

A human-in-the-loop platform that orchestrates coding agents (headless Claude
Code) to get project work done, built around the Double Diamond architecture
(issue #744): parallel Scout exploration → spec queue → serial Builder
implementation.

## Load-bearing design rules

- **The Scout/Builder information barrier is inviolable.** Builders never see
  Scout code — the spec is the deliverable. Specs are text, so a Builder run
  can batch N specs into one branch. Never propose reusing Scout branches.
- **Salvage is never a spec.** A Scout writes two files with two meanings:
  `SPEC.md` means "I concluded", `NOTES.md` means "here is what I have so
  far". Notes stream back as checkpoints during the run (the VM is destroyed
  at the deadline, so nothing collected at the end survives) and land in
  `scout_notes` — one row per session, no `Spec`, no queue entry, no review
  path. Their only consumer is the next attempt's prompt, where they are
  quoted as explicitly unverified leads. Reporting a partial spec *as* a spec
  would be worse than losing the run, because a half-explored spec in the
  review queue looks finished. Promoting notes into a spec stays a human act.
  Which is also why the Scout prompt asks for the spec to be **drafted into
  `NOTES.md`** rather than into an early `SPEC.md`: a Scout that finished the
  work and was killed six minutes later lost all of it, because `SPEC.md` is
  written last and nothing reads it until the run ends (#1046), while
  `NOTES.md` is already streaming. Drafting under the `SPEC.md` name instead is
  the fix that looks equivalent and is not — an agent that ends its turn early
  would then hand a draft to `report_outcome`, which reads a clean exit as a
  spec, and the half-explored spec would reach the review queue looking
  finished. The draft is salvage until the run concludes; promotion stays where
  it was. The prompt also says, in both agents' words, that **a backgrounded
  command dies with the turn** — an agent parked on a poll loop over a cold
  build returned its turn expecting to be woken (#962), which the orchestrator's
  own prompt has warned about for a while and the agents that most need it were
  never told — and asks for verification **in proportion to what was changed**,
  which is where the incentive to background came from. It also **names the
  clock**: an agent knew its budget as a number only if the prompt happened to
  carry one, so the two rational-looking mistakes available to it were to wait
  for a result nothing will collect and to start what cannot finish — a Scout
  ran `cargo clippy` with four minutes left against a command another run had
  measured at forty (#982). The minutes are rendered from the budget the run
  was *given* and never from the configured constant, because a reattached
  run's budget is the remainder and a prompt naming the hour would be lying to
  it. And a run that stops on its own terms says so in its own
  `exit_reason`: `unspent_budget_clause` appends one clause when half the
  budget or more is left, so `SPEC.md not found` at eighteen minutes stops
  reading identically to the same sentence at the deadline. Half, and
  deliberately *not* `WAIVED_BUDGET_SHARE` — that quarter answers whether a run
  was given its budget, this half answers whether it chose to stop, and one
  number serving two questions is #944. It changes no decision:
  `FailureClass` is stamped off a field and never off reason text, and this
  clause is addressed to a human.
  A human may also skip the Scout outright — `POST /tasks/{id}/build-now`
  writes a spec by hand for a task whose issue body already is one, and its
  `specs.session_id IS NULL` is the tell. It changes nothing below: the spec is
  text, so the Builder cannot distinguish it. What it skips is not only the
  scouting but the **review**, since there is no independent artifact to rule
  on — hence a single `author_spec` decision row rather than an `approve`,
  which would claim a second opinion that does not exist. It is human-only and
  refuses the orchestrator outright rather than being charter-gated: authoring,
  approving and dispatching one's own work with no second opinion anywhere in
  the loop is a different autonomy from `dispatch_builds`, and if it is ever
  granted it wants its own named capability.
- **Never persist a GitHub-owned fact** (PR mergeable/SHA/CI, issue
  open-closed, labels). Query at decision time. Persist only Tasks-owned
  state plus append-only decisions keyed to immutable SHAs. GitHub writes go
  through the server, never through agents.
- **`done` means shipped, and it is written in exactly one place.** A build
  that opens a PR has made a claim, not a delivery, so it parks its batch in
  `awaiting_merge`; the only thing that writes `done` is closure-derived
  retirement, so `done` always means "the issue is closed upstream". Each poll
  reads the unresolved PR (`watch_merges`) and either closes the issue as
  completed — through the server, under `retire_work`, with the merge commit
  as evidence — or unwinds the batch back to `ready_to_build` with a build
  attempt charged. Unwinding restores the *option* to rebuild; nothing
  dispatches a build by itself, which is what keeps that safe. The cost is one
  REST call per open Builder PR per poll, forever, and that is the right
  price: the moment it is cached in a `last_checked` column, the thing being
  cached is a GitHub-owned fact with a timestamp on it.
- **A PR's `merged` means "reached its base", never "shipped" — the pipeline
  stacks builds routinely.** A build based on another build's branch reads
  `merged: true` the instant that branch takes it, whether or not the branch
  ever lands, so `watch_merges` resolves `awaiting_merge` on whether the merge
  commit is an **ancestor of the trunk** (`SCOUT_BASE_BRANCH`) and never on
  `merged`. `run::shipped` is that predicate: `base_ref == trunk`
  short-circuits, so the ordinary unstacked case costs no extra call, and only
  a stacked PR spends a `GET /compare/{trunk}...{sha}` — which reads *head
  relative to base*, so reachable is `identical` or `behind`, and reversing the
  operands inverts the verdict. Every unreadable answer is `false`, because the
  two mistakes are not symmetric: staying parked costs one call on the next
  poll, while concluding wrongly writes `done` over work that shipped nothing
  and no pass revisits `done`. This is not hypothetical — **PR #863 was
  `merged: true` and its work never reached `main`**, which is the failure
  above, one level up. A batch that has merged but not reached the trunk
  therefore **stays parked rather than unwinding**, and that is what makes both
  manual merge orders safe: merge the base first and the dependent's commit is
  already reachable, or merge the dependent first and a later poll finds it
  once the base lands. Reachability is monotone, so polling can never un-ship
  something it concluded had landed. Nothing auto-unwinds a stranded batch;
  `ObligationKind::LandBatch` makes it loud instead, and it is the first
  obligation whose subject is a **build** id rather than a spec id.
- **The merge *method* decides whether a landed base stays addressable, and the
  default is a merge commit.** The rule above rests on reachability, and a
  squash is the one merge that destroys it: it writes a new commit with no
  parent link to the head branch, so the branch becomes an ancestor of nothing
  and every build stacked on it is stranded — not mergeable (its diff is
  already in the trunk under different SHAs, so GitHub reports a conflict or an
  empty change), not retargetable (replaying at the trunk replays the base's own
  commits), recoverable only by a rebase or a rebuild, neither of which anything
  in this pipeline can perform. This pipeline **stacks builds routinely**, so
  `POST /pull-requests/{n}/merge` defaults to `merge` rather than `squash` and
  refuses a `squash` whose head branch still has open pull requests based on
  it — naming them, and naming `merge` as the way through, so the refusal is a
  redirection and not a dead end. A squash with nothing stacked on it is
  untouched: the guard is about stranding dependents, not about squashing. The
  asymmetry is deliberate and **inverts the standing "unknown never blocks"
  rule**: a GitHub that will not say what is based on the branch refuses the
  squash, because refusing a safe one costs one retry with a method that is
  always correct, while allowing an unsafe one strands work with no cheap
  recovery at all. The check runs **after `authorize`** — a capability that is
  `off` answers 403 before any complaint about the method, on the same ordering
  argument that put the rationale check there — and before any effect, so a
  refusal is a no-op.
- **A build owns its batch's state only until a later build carries the same
  specs.** Both readers of "which build is still waiting on a pull request"
  found one by joining `builds → build_specs → specs → tasks` and filtering
  `t.state = 'awaiting_merge'` — which identifies a **parking**, not the build
  that caused it. A task carries no memory of which build put it there, so the
  instant a rebuild re-parks a spec, *every* earlier succeeded build carrying
  it matches again, forever. `carried_by_a_later_build` is one shared SQL
  predicate applied wherever that join appears, and `Store::build_superseded`
  is rewired onto it so there is one notion of supersession rather than two
  that can drift — two hand-written versions of this question is exactly how
  the readers came to disagree. It needs **no GitHub read**: supersession is a
  Tasks-owned fact (`build_specs`, `builds.status`, `rowid`), while "the PR is
  closed" is GitHub's, and a `builds.pr_resolution` column recording the latter
  would be persisting a GitHub-owned fact that GitHub can *retract* — a PR can
  be reopened. **The poller half is the destructive one, and reading it as
  cosmetic is the mistake**: fixing only the obligation leaves
  `list_builds_awaiting_merge` selecting the dead build, whose PR answers
  closed-unmerged on every poll forever, so `watch_merges` re-runs
  `unwind_unmerged_build` against it every pass — charging the **live** build's
  specs a build attempt each time until they cap and `blocked` themselves, and
  dragging their tasks out of `awaiting_merge` while the new PR is still open
  (#938 sat `ready_to_build` behind an open PR #952). The obligation half is
  #956, a `land_batch` naming a PR nobody can merge with no act that discharges
  it; the poller half is #959. Three rules hold the predicate's shape.
  **Keyed on the spec, never the task** — task-keyed would retire the
  obligation for a PR that genuinely never landed whenever some *other* spec of
  the same task was built later, and the errors are not symmetric: a stale
  obligation is noise, a dropped one loses the only thing that notices
  stranding at all. **`rowid` and not `created_at`**, or two builds stamped
  inside the same second let a build supersede itself. And `succeeded` only: a
  later build still queued or running has not taken the work over, and one that
  failed gave it back. `unwind_unmerged_build` filters **per spec** rather than
  refusing the whole call, so a batch a rebuild only partly re-carried still
  returns the half nothing took over — belt-and-braces beside the filtered
  list, and deliberate, because this is the half that writes. The predicate is
  falsifiable in two lines: stub it to `"0"` and the three tests that pin this
  fail. What is deliberately **not** fixed: with `retire_work` `off` or
  `shadow`, nothing unwinds a PR that closed unmerged, so its batch stays
  parked and keeps raising `land_batch` — the announced cost of the kill
  switch, whose discharge is turning the capability back on.
- **A GitHub write records its intent before it happens, and the window
  between is a state on the row rather than a gap in the ledger.** #957 closed
  the half of the attribution gap that can be *refused* — `require_rationale`
  moved into `authorize`, ahead of every effect. This is the half that cannot
  be: all ten sites that write to GitHub ran the effect and *then* the
  `record_decision` explaining it, so a SQLite error, a panic or a SIGKILL in
  between left a real artifact upstream that nothing accounts for (#964).
  Recording first stays **refused** — a row claiming an effect a failed call
  never had makes every row suspect, where a missing row leaves one artifact
  unexplained — so `decisions` grows a `state` (`pending` → `applied` |
  `annulled`), and `server::ledgered` takes the effect **as a closure**, which
  is the point: a handler nobody has written yet cannot reach GitHub without
  its intent already on record, the same property that made `authorize` the
  right home for the rationale check. One row with a state column and not an
  intent row plus a confirmation row, because every existing aggregate over
  `decisions` would double-count under two and each would need an "and not the
  intent one" clause the next query would forget; `DEFAULT 'applied'` leaves
  history and every *store-only* decision alone by construction, since those
  commit in the same transaction as the state they authorize and have no window
  to represent. The three outcomes are decided **structurally off
  `GhError::is_unavailable`, never off message text** — returned → `applied`;
  refused with an *answer* → `annulled`; **never answered → stays pending**,
  because we do not know and saying so is the whole point. A settle that itself
  fails is logged and **not** propagated: the effect happened, and a 500 there
  sends a well-behaved caller into the retry that files a second issue, which
  is #957 one level up. What chases the residue is
  `ObligationKind::ReconcileDecision`, and it is dischargeable only because the
  **server** holds the lookup: `GET /decisions/{seq}/reconcile` asks GitHub with
  the server's own credential and returns what it found, then
  `POST /decisions/{seq}/settle` writes that down. Leaving the lookup to the
  recipient is what fails — the default `ORCHESTRATOR_CMD` is
  `--allowedTools Bash(curl:*)` with no `GITHUB_TOKEN`, so its only moves would
  be to guess (writing `applied` on no evidence into an append-only ledger,
  worse than the missing row) or to refile the issue. A settle is **never
  charter-gated**, including for a capability since demoted: `shadow`/`off`
  exist for demotion and demotion is likeliest exactly when pending rows exist,
  so gating it would raise a nag its recipient is forbidden to discharge.
  Settling is not the action — the effect already happened, and refusing to
  record it only keeps the ledger wrong. The guard is structural rather than a
  list: `DecisionAction` is macro-generated so `ALL` is complete by
  construction, and `no_write_route_reaches_github_without_recording_first`
  drives it through an exhaustive match against a GitHub that never answers —
  a fake that reads the ledger *as the call arrives*, so the assertion is about
  ordering and not about the end state. The residue that remains is deliberate
  and one level smaller: a settle that fails after a successful effect leaves
  the row `pending`, which is the honest description.
- **An open PR is chased like every other stage, and the default is to land
  it.** The pipeline used to dead-end at PR open: the `land_batch` bullet ended
  "landing it is the human's" while the charter shipped `land_builds` **live**,
  so every parked batch was reported and none was merged. That sentence is now
  *generated* from the charter row (`orchestrator::landing_section`), the way
  the authority and workdir sections are — the fix for a prompt contradicting
  the charter is never a better sentence, it is one source. What sends a batch
  back to a human is **unverifiability, not risk**, because "hand it over when
  in doubt" is what the old sentence effectively said and doubt is unbounded.
  So there are exactly three carve-outs, named as the whole list: GitHub would
  refuse the merge **or reports a check that has not gone green**, the build
  reported no passing test run of its own, or nothing runnable here could have
  checked it. The first carve-out is where #1015 landed, and it is worth being
  precise about what moved. It used to be refusal alone, because **no workflow
  here produced a pull-request check**, so `mergeable_state` could only ever be
  `clean` or `dirty` and GitHub's verdict was structurally incapable of
  objecting to a change that does not work. `.github/workflows/ci.yml` now runs
  `cargo fmt`, clippy and the whole suite **on every push** — and a Builder
  branch is a push, so its check runs attach to the commit the pull request
  points at. Three consequences and none is "GitHub is the gate now". The
  checks are **not required** (there is no branch protection), so `blocked`
  stays unreachable, a red branch reads `unstable`, and the merge endpoint
  still takes it — which is why the widening had to happen in the *prompt*
  rather than being left to GitHub. `unstable` does **not** distinguish a check
  that failed from one that has not finished, and GitHub will not say which, so
  `Landing::Unstable::describe()` says both possibilities out loud instead of
  filing it under non-required noise, which is what it said before CI existed
  and would now read as permission to merge red work. And `Landing::Clear` now
  carries evidence rather than being a statement about git — what it still does
  not settle is the composition, which is the sentence the test pins, alongside
  the unchanged absence of "ready to merge". The mechanical enforcement moved
  with the premise: `no_workflow_produces_a_pull_request_check` is gone, and
  `ci_runs_the_suite_on_every_push` (`crates/tasks/tests/site.rs`) replaces it,
  failing the suite if CI stops existing or grows a `branches:`/`paths:` filter
  — the quiet falsification, because an unfiltered-out branch that nothing ran
  on reads `clean` for having no checks rather than for passing them, which
  fails *upward*. Beside it `no_workflow_runs_fork_code_with_our_secrets`
  refuses `pull_request_target` in any workflow, permanently. Prose was the
  wrong home for any of this — the next person to add a filter would have
  falsified six doc comments and this bullet with nothing going red. Read
  `mergeable_state` and never `mergeable` alone: `false` there means a conflict
  and nothing else, so a red PR reads ready. The only evidence that a change works is therefore a run of
  the project's own suite, and the bullet below is how that run is now
  obtained — by the Builder **supervisor**, as a check, where this bullet used
  to describe a `Verification:` trailer the Builder *agent* wrote into
  `SUMMARY.md` and the host grepped. Every batch parked before that changed
  carries no verification at all, which reads as "no run on record" and goes to
  a human, which is the direction a mistake here has to fall. All of this
  lands on the *brief* rather than in the obligation: refining an obligation's
  **kind** after `Store::obligations` returns would lose its
  `(kind, subject_id)` reminder row and nag every tick instead of every thirty
  minutes, and it would cost a GitHub read per parked PR per tick rather than
  one per obligation actually surfaced. Mergeability is never cached — that is
  persisting a GitHub-owned fact with a timestamp on it.
- **The one arm where the default is not to merge has a verb of its own,
  because merging there ships nothing and is irreversible.** This pipeline
  stacks builds, so a build is routinely opened against another build's branch;
  when that base lands **first**, the dependent is left pointing at a branch
  nothing will pick up. `Brief::live_landing_facts` has diagnosed exactly that
  since it started reading `base_ref` — the `(false, true)` arm of
  `merge_reached_trunk` — and the diagnosis had **no act behind it**:
  `github.rs`'s entire pull-request surface was create, merge, close. So the
  only move available to an agent following the standing "merge it this turn"
  instruction was the one that cannot be walked back, and GitHub will not edit a
  *merged* pull request — the merge deletes its own fix, and the work reaches the
  trunk never (#1027). Both halves ship together and neither is sufficient.
  `POST /pull-requests/{number}/retarget` is the verb: REST `PATCH
  /repos/{o}/{r}/pulls/{n}`, ledgered like every other GitHub write, under
  `land_builds` rather than a capability of its own — it is the same judgment
  about the same artifact as the merge it exists to make possible, and it is the
  **reversible** half, since calling it again points the pull request somewhere
  else where nothing here can un-merge one. It reports the base GitHub read
  back rather than the one that was asked for, because a caller told what it
  asked for has learned nothing. And `merge_pull_request` **refuses** that arm
  rather than warning about it, on the #1044 precedent one shelf over: the check
  runs after `authorize` and before the effect, and **every unreadable answer
  refuses** — the deliberate inversion of the standing "unknown never blocks"
  rule, because refusing a good merge costs one retry while allowing this one
  costs the work *and* the retarget together, in the same instant. A missing
  `base_ref` refuses for the same reason: absence is not "unstacked". What is
  deliberately *not* refused is the other arm — a base that has **not** reached
  the trunk yet is ordinary stacking and merges normally, which is how a queue
  drains at all — and that pair is what the tests pin. The cost is one
  `pull_request_state` per merge and no compare at all when `base_ref` is
  already the trunk; the squash check now shares that read instead of making its
  own. The prompt half is load-bearing too: a refusal that does not name the act
  is a dead end, so the obligation header names the retarget and the brief's own
  arm text names it with the number filled in — the brief and not the header,
  because which arm holds is a per-pull-request fact the header cannot know.

- **A Builder cannot return untested work, because the supervisor runs the
  suite and the agent does not — and a green run covers the branch against its
  own base, never the composition.** `SUMMARY.md`'s
  `Verification: PASSED|FAILED|NOT RUN` trailer was agent-authored prose,
  grepped by the host, gating a write to GitHub: a decision resting on text
  written by the party being graded, which is the defect `FailureClass` already
  forbids one level up. `builder-supervisor` now runs the project's own suite
  itself, inside the VM, against the **swept** tree the bundle carries, and
  stamps a structured `Verification { status, detail }` on
  `BuildEvent::Completed` — the deliberate sibling of `class: FailureClass`,
  read off the field and never off `detail`, which is prose. The larger prize is
  not where verification happens but that **a red suite never reaches GitHub**:
  it fails the build inside the VM as a `Verdict`, where before it opened a pull
  request, parked a batch in `awaiting_merge` and spent a reviewer's attention.
  `VerificationStatus` is `Passed | Undeclared | Unavailable | TimedOut` and
  there is deliberately **no `Failed`**, so "shipped and red" is unrepresentable
  rather than merely avoided; `is_green()` is the only way anything asks, so a
  variant added later cannot become green by omission, and the VM wire's
  forgiving `Deserialize` decays an unknown status to `Unavailable`, never
  toward green (a terminal event that will not decode costs the run its
  outcome, which is why it decodes at all). The suite is declared by the
  repository at `.tasks/verify` and read out of the build's **base** commit —
  the agent has write access to the tree, so a tip-resolved gate is the same
  forgery one level down with `exit 0` in place of `PASSED`. The issue's
  argument for the tip ("a pull request that changes how the project is tested
  changes its own gate") is a GitHub-Actions property and **inverts here**,
  because this gate decides whether a pull request is opened *at all*, so the
  reviewer only ever sees the diff after it has ruled. Base-resolution alone is
  not enough either: this pipeline **stacks builds routinely**, so a script
  build A weakened is already in build B's base, B's own diff changes nothing,
  and nothing would notice. So `detail` always names the blob SHA of the script
  that ran — always, including when it matches, because a field that appears
  only on disagreement is one nobody learns to read — and compares it against
  the **trunk's** copy, reporting `declaration_changed` on a difference rather
  than refusing (changing how a project is tested is ordinary work; the reviewer
  needs to know which script ruled, not to be blocked). A trunk not reachable in
  the clone is reported as an *unmade comparison*, never as agreement. A project
  that declares nothing dispatches ungated and reports `Undeclared`, on the
  standing "absence of evidence never holds" rule — never green, so it routes to
  a human exactly as it did before, which makes the whole change strictly
  additive. **Red is a verdict reached; everything else is the absence of one.**
  A red suite buys exactly one bounded repair round — `--resume` on the same
  conversation and the same worktree, so the agent repairs rather than rebuilds
  — and that round shares `BUILDER_MAX_RESUMES`'s *mechanism* while never
  sharing its counter, because the two bound different questions (how often a
  dropped connection may be picked up, versus how often the agent may be told
  its own tests are red). Once a round has returned red, **no later status may
  ship the build**: round two ends green or the build fails, because shipping on
  "we do not know" when the last thing actually known was red is the one
  direction this exists to prevent — and because red being terminal while a
  hanging suite is not would be an incentive gradient pointing at making the
  suite slow. The observed status stays honest in `detail` and the *decision* to
  fail is separate from it; a lie there would be a lie to a human. A **first**
  round that times out or cannot run is different and **ships**: a suite that
  never finished is not evidence about the work, the implementation may be
  perfect, and discarding it because a cold `target/` compiled slowly is the
  failure #929 and #884 were filed about. The inner suite budget is sized to
  expire *first* (remaining run budget minus a 120s packaging reserve, floored
  at 60s), which is what keeps the outer `BUILDER_TIMEOUT_SECS` expiry
  defensible as a `Verdict` rather than a coin toss about which clock fired.
  `run_script` must **not** await its output collector on the timeout path —
  killing `sh` does not close the pipes its children inherited, so the readers
  never see EOF and the supervisor hangs forever, worse than the timeout it was
  reporting; `abort()` there, and a bounded grace on the normal path for the
  same hazard one size down. What a green run does **not** say is the half worth
  writing down, because the request behind #1020 read as "a green supervisor run
  should end the orchestrator's own": it tested the branch against **its own
  base**, and the trunk moves under a queue. Two branches can each be green
  against their own base and red composed, and no supervisor run can see that,
  because the thing it would have to test does not exist until merge time. So
  carve-out (b) below is genuinely discharged and (c) is untouched, but "the
  orchestrator's own run is stronger evidence than the Builder's trailer" is now
  false in its stated reason (claim versus check — both are checks) while
  staying true for a different one (branch versus composition), and
  `landing_section`/`verification_line` say so in those words. The trailer, its
  parser, the app's second parser and the prompt instruction that produced it
  are **deleted rather than kept as a fallback**: deleting the parser while
  leaving the instruction is the worst of the three options, since the agent
  would go on writing `Verification: PASSED` into what *is* the PR body, putting
  a claim beside a field holding a check with the prose one in front of the
  human. `pr_text` appends a **host-authored** sentence generated from the field
  instead, and nothing parses it back. **This reaches nothing until `make
  images` runs** — until the Builder image is rebuilt every build reports
  `verification: None`, which renders as "no run on record", is never green, and
  routes everything to a human; that is `#[serde(default)]` doing its job, and
  the degradation is safe by construction rather than by luck.
- **A build is a success only if the agent said so and accounted for it —
  commits alone are not a deliverable.** The supervisor emitted the agent's
  exit code as `ImplementationFinished` and then ignored it, and the only
  artifact check downstream is `tip != base`, which a half-finished batch
  passes trivially because the sweep commits whatever is on disk. So a build
  whose agent exited 1 on `blocking_limit` carrying four specs was recorded
  `succeeded`, opened a pull request and parked four tasks in
  `awaiting_merge` — two of the four never implemented, and `builds.summary`
  zero bytes (#1008). Merging is the dangerous act there: `watch_merges` closes
  **every** task in the batch as completed with the merge commit as evidence,
  and no pass revisits `done`. Two rules now stand between that and GitHub, and
  they are separate because they fail differently. **A non-zero exit fails the
  build**, after the resume loop (so a run picked back up reports the last
  attempt's code) and *before* the sweep, because the exit is the cause and "no
  commits" is at best a symptom of it — reached through `run.failure_class()`,
  so a dropped connection is still `Transport`, charges no strike and returns
  the specs to `approved`. This is the Scout's rule one crate over (`a clean
  exit is a spec, whatever the file looks like; only a messy exit is read
  sceptically`), and the difference is what there is to be sceptical *about*: a
  Scout has `SPEC.md` to inspect, a Builder's deliverable is a branch nothing
  in the VM can judge, so there is no partial-credit reading and the honest
  answer is to fail. It costs a rebuild, which is the cheap half of the trade.
  **A missing `SUMMARY.md` fails the build** too, and that is not a style rule:
  the summary *is* the pull request body and carries the `## Review feedback`
  accounting, so without one `pr_text` falls back to a list of spec titles
  under one `Implements #NNN` per task — a claim about work, written by the
  pipeline rather than by the party that did it, in front of the human who has
  to rule on it. The fallback stays in `pr_text` because builds recorded before
  this still have to render; what changed is that no new build can reach it.
  The hardest case is the one a test pins: commits **plus** a summary plus a
  non-zero exit is still a failure, because nothing in the VM can tell a branch
  that finished from one that stopped.
- **The orchestrator can now produce the run it used to only ask for, and what
  made that possible was a warm build directory, not a bigger budget.** Carve-out
  (b) above rested on "nothing re-runs its tests for you", and that was true for
  a reason that had nothing to do with the suite: warm, the whole workspace is
  ~565 tests in ~21s. It was **compilation**. Verifying that N pull requests
  compose means checking them out somewhere, a `git worktree` gets its own empty
  `target/`, and a cold workspace debug build is minutes before a single test
  runs — so a typecheck was the ceiling on what a merge decision could rest on.
  Two more things compounded it: a 600s turn against Claude Code's own 600s
  per-command ceiling (a command could eat the whole turn and leave nothing to
  report in — the observed "killed before writing output"), and, when the agent
  avoided the worktree, contention with rust-analyzer for the live checkout's
  build-directory lock. The fix is three variables on the **child process only**
  — `CARGO_TARGET_DIR` at a shared long-lived directory
  (`ORCHESTRATOR_TARGET_DIR`), and both bash timeouts derived as **half** the
  turn (`command_budget`), half being the statable guarantee: whatever a command
  spent, at least that much turn is left to report it. Derived and not
  configured, because a second knob is a second thing to get wrong and the
  invariant is a *relationship* between two numbers. `<data dir>/.env` is the
  wrong home for any of it — every `tasks` invocation reads that file, so a
  `CARGO_TARGET_DIR` there would be inherited by `tasks reload`'s own build of
  the server and would silently redirect the Makefile's `TEST_BIN_DIR`. **The
  prompt half is the load-bearing half**: the directory alone would leave the
  fix inert, since the agent would have somewhere warm to build and a standing
  instruction saying the run will not happen. `verification_section` and
  `landing_section` are both generated from one computed `can_verify`
  (`workdir_is_checkout && target_dir.is_some()`), so they cannot disagree about
  what this host can do, and the directory is created **once per boot** rather
  than per turn so the prompt can never name one the agent will find missing.
  `brief::verification_line` says "no automated check" rather than "nothing
  downstream" for the same one-source reason: what the *pipeline* does not do
  and what its *reader* cannot do are different facts. This widens `land_builds`
  autonomy on purpose — the charter's own principle is that what sends a batch
  back is unverifiability. The *reason* has since narrowed and the bullet above
  states it: both runs are checks now, so what makes this one worth making is
  not that it is stronger in kind but that it is the only one that can test the
  **composition** — the branches merged onto a trunk that moved under them while
  they queued, which is the run nothing upstream can make. Carve-out (c) is
  untouched and still routes to a human. Verifying a composition stays under
  the orchestrator's own **judgment** and does **not** become a Builder-shaped
  VM run: that would need its own answer to the Scout/Builder barrier, all to
  deliver what a worktree plus a warm directory deliver in seconds. The
  "revisit if compositions outgrow the turn" clause fired (#1053) — for
  *availability* rather than duration, a turn spent on a suite run being a
  turn the human waits behind — and what moved was the run, never the
  judgment: it runs on the worker lane (next bullet), a host process under
  the orchestrator's own instructions, which is the alternative this bullet
  was written to defend rather than the VM run it was written against.
- **The conversation lane is for judgment; labor runs on the worker lane
  (#1053).** The orchestrator was three jobs multiplexed onto one serial turn
  lane — judge, laborer, front desk — and the laborer half is what made the
  front desk unreachable: under load, turns chain nearly back-to-back, so a
  human message waited out a turn that might spend twelve minutes in cargo
  before reaching it. `POST /workers {job, prompt, rationale}` (charter:
  `dispatch_workers`, live, ledgered **store-only** — `applied` by
  construction, the `enroll_agent` shape) queues a **worker**: a fresh,
  disposable headless Claude Code session the server spawns **on the host**,
  one serial lane (`run::worker_loop`), no `--resume` and no context carried
  between jobs, whose result text returns as a server-written
  `[worker <job>]` event turn the next tick answers. The generated prompt
  sections flip to always-delegate with the threshold stated in them —
  **delegate anything that compiles or runs a suite; keep anything that
  answers in seconds** — and the delegation text is gated on the charter row
  being `Live` exactly, because a shadowed dispatch runs nothing and a prompt
  telling the agent to lean on one strands every verification on a report
  that never comes. **A worker is a voice, not an authority, and the
  enforcement is the allowlist, never the prompt**: a local process with no
  `X-Tasks-Actor` header is attributed as the *human*, whom the charter never
  gates, so a worker that could reach the API would hand the orchestrator a
  route around every refusal it has — `build-now` included — by putting the
  instruction in a job prompt. `DEFAULT_WORKER_CMD` therefore carries **no
  `curl` and no `git push`**, spelled in verbs and quoted through
  `split_command` (#976); the worker's own server-written prompt carries the
  verification discipline (fixed worktree, warm directory, budgets), so the
  dispatcher's job prompt is only the job. **Output streams and every ending
  becomes a report**: stdout persists line-by-line to `transcript_lines`
  (third owner arc — the Scout's NOTES.md argument, #1046), and success,
  failure, timeout, cancel, shutdown and boot-orphaning each land a turn
  naming how it ended and carrying the tail of what it streamed — never
  silence, and **never a strike anywhere**; a failed worker is information
  and redispatch is the orchestrator's call. Budgets invert deliberately:
  `WORKER_TIMEOUT_SECS` (3600) is four orchestrator turns, because the whole
  point is that the suite no longer has to fit inside the turn the human is
  waiting behind; the suspend rules apply unchanged (a napped host reads
  `abandoned`, not `timed out`). Two structural consequences. The warm build
  directory's consumer is now the worker lane, which breaks the "only one
  loop starts a process in there" argument the reclaim rested on — so
  `VerifyDir` grew a lane lock: runs hold the read half for their duration,
  the reclaim takes `try_write` and **skips rather than waits**. And a worker
  is a local child like the orchestrator's turn, with the same rules: it does
  not count toward `is_destructible`, a shutdown concludes its row and
  reports the loss immediately, and a boot writes off whatever a dead process
  left (`report_orphaned_workers`) as report turns rather than silence.
- **The warm build directory grew because a fresh worktree is a fresh metadata
  hash, not because anything was kept warm — so what bounds it is a prompt
  sentence first and a reclaim second.** `ORCHESTRATOR_TARGET_DIR` was
  documented as "expect ~7.5 GB and nothing prunes it", was found at 39 GB by a
  human hunting for disk (#1010) and measured at **51 GB** on 2026-08-20,
  growing ~2 GB per verification on a filesystem with 74 GiB free. The cause is
  not the sharing: cargo keys an artifact on a metadata hash that **includes the
  source path**, and the section above told the agent a `git worktree` "costs
  you no extra compilation" — true of the registry dependencies, whose hash does
  not include the workspace path, and false of every workspace crate, so each
  new worktree path added a complete fresh set and the previous set was kept
  forever. The measured breakdown, recorded here so nobody re-measures: `deps/`
  46.79 GB, of which **35.24 GB is 208,468 codegen-unit `.o` files** (macOS's
  default `split-debuginfo = "unpacked"` for a workspace that declares no
  `[profile]` section — the debuginfo sits *beside* the binaries, not inside
  them), executables 6.14 GB, `.rlib` + `.rmeta` 5.2 GB; and `incremental/`
  **24.24 GB**. Two of those numbers kill an idea each. An eviction tier that
  removed only the linked executables — the obvious "test binaries are the
  biggest artifacts" refinement — frees 13% and leaves every byte it was aimed
  at; it is measured, wrong, and deliberately **not** recorded as a future
  refinement, because a deferred idea in a doc gets picked up by someone who
  does not know it was falsified. And **mtime eviction is backwards here**:
  registry artifacts are built once at the start and never touched again, so
  they are always the *oldest* files and `cargo-sweep --time` deletes precisely
  the warmth while keeping the per-worktree garbage (the stamp-file variant does
  not rescue it — a no-op `cargo build` touches nothing, so "older than the
  stamp" means everything). So the fix is three parts in descending order of how
  much they matter. **Report the size** wherever a human already looks, and
  **unlike the three dispatch holds this row prints whenever there is a
  reading** — a hold is an exception, this is a quantity that grows silently,
  and a row appearing only once it was over its ceiling would reproduce #1010
  exactly. **Stop the bleeding**: one reused worktree path named in the prompt
  (`<data dir>/verify-worktree`, derived and not a knob), with the
  `reset --hard` + `clean -fd` + `checkout --detach` sequence spelled out,
  because verifying a pull request means merging the trunk into its head and the
  worktree arrives carrying last time's merge commit — a bare `git checkout`
  refuses, and a wedged worktree means *no* verification at all, which routes
  every batch to a human; plus `CARGO_INCREMENTAL=0` and
  `CARGO_PROFILE_{DEV,TEST}_DEBUG=line-tables-only` on the child. Those cargo
  settings are set **in both places or neither** — toggling either invalidates
  every workspace artifact (a registry dependency is untouched, verified
  empirically), so a `make verify-warm` that disagreed with the child would
  rebuild the whole workspace on every alternation, costing far more than the
  disk it saves; `VERIFICATION_ENV` is the one list and
  `verification_env_matches_the_makefile` fails when they drift. The debuginfo
  level was **measured rather than assumed**, which is what the change was
  required to do: `cargo test --workspace --no-run` is **6.26 GB** at the
  default and **3.16 GB** at `line-tables-only`, a 49.6% saving, with a
  deliberately failing test producing a byte-identical backtrace naming a file
  and a line in every frame. Not `debug = 0`, because a failing test's backtrace
  is how the failure gets diagnosed — in a turn, with the worktree about to be
  reset. **Bound what is left** with a graduated reclaim past
  `ORCHESTRATOR_TARGET_BUDGET_GB` (20, a judgement rather than a measurement):
  tier 1 removes every `<profile>/incremental`, which is keyed to one worktree
  path and therefore costs no warmth at all, and only if that leaves it over
  does tier 2 empty the directory — **keeping the directory itself**, since it
  is created once per boot precisely so the prompt cannot name a missing one.
  Each tier re-walks, so every number reported is measured rather than
  estimated. The reclaim is permissible here in a way it is not for a rejected
  bundle because everything in this directory is reproducible from the checkout:
  a deletion costs time, never work. The cost that must not be paid quietly is
  that the wholesale tier makes the next verification **cold**, which leaves
  carve-out (b) undischarged and sends that batch to a human — so it is a `Note`
  on the feed and stays on `/status` for the rest of the boot. A `Note` and
  **not an obligation**, for the reason `ObligationKind::StaleImage` does not
  exist, one notch sharper: obligations go to the orchestrator, which is the one
  actor that must not be asked to manage a directory it builds in. It runs from
  `orchestrator_loop` **before** each `tick()`, which is the whole safety
  argument — that loop is the only thing that starts a process in there, so a
  deletion cannot race a compile by construction rather than by a lock; while a
  human has the session checked out it measures and reports and reclaims
  nothing, because that is the one case the argument does not cover. The
  reading is in memory and never a table (a fact about a filesystem with a
  timestamp on it), refreshed on a 15-minute cadence rather than at read time
  because the walk is hundreds of thousands of files, with hardlinks counted
  once so the number agrees with the `du -sh` that found the problem, and
  `measure_due` claiming on the **attempt** rather than on success so an
  unreadable directory is not re-walked every tick with a `warn!` each time. One
  thing is measured and undecided: the directory also holds `gpui` artifacts in
  two hash variants, and `app-gpui` is not a workspace member, so part of what
  is retained is a dependency tree `make test` never builds — the steady-state
  size therefore depends on whether app builds share this directory, which
  nobody has decided.
- **Bulk intake never auto-dispatches, and queue membership is explicit.**
  `tasks.manual_rank` is set only via the API; the GitHub poller must never
  write it. Ingested issues land in `backlog` and are never dispatched — only
  explicitly queued tasks (`POST /tasks/{id}/queue` or `/scout`) reach a
  Scout, and picked-up work stays picked up (failures and `needs_revision`
  return to `queued`, not `backlog`). The invariant is that **bulk intake must
  not become bulk work**: adding a repo with 11,000 issues must not turn into
  11,000 Scout runs and 11,000 PRs nobody chose. It is not a human-judgment
  gate on any individual task, so deliberate per-task queueing by an
  accountable actor is fine — the orchestrator may do it when `queue_tasks` is
  live in the charter. The invariant is upheld by the pipeline's shape, not by
  rate limits: backlog never dispatches, `SCOUT_MAX_CONCURRENT` bounds scouts,
  and builds are serial. `POST /tasks/{id}/build-now` sits inside that shape
  rather than beside it: it is per-task, human-only, and one call is one build.
  `Rejected` is a **second door in**, and it is narrow on purpose (#1028): the
  state names two unrelated outcomes — a *verdict* (someone decided not to do
  this; the issue is closed) and *attrition* (three scouts failed to produce a
  spec; the issue is still open and the work is still wanted) — and
  `Store::queue_task` admits only the second, gated on `gh_state` being open, so
  reopening an issue is the deliberate act that makes a verdict-rejected task
  eligible again. It clears `dispatch_attempts` as it goes, and that half is not
  a courtesy: `dispatch_gate` skips anything at or past the cap, so a task
  returned still carrying 3 of 3 would sit in the queue looking like it was
  waiting its turn and never dispatch — which is worse than the stranding it
  fixes, because it is invisible. Before this the decision surface
  `list_active_tasks` deliberately keeps on screen ("close the issue or
  re-queue?") offered a decision with one arm missing: nothing, for anyone,
  could re-queue a rejected task, and the only route back was to close the issue
  and file a new one, losing its number, comments, labels and history for work
  whose only failure was infrastructural.
- **Builds dispatch when the lane frees, and the lane-free turn is where
  batching lives.** A build sent into a busy lane gains nothing by existing
  early — builds are strictly serial — and it costs the two things batching
  needs: it freezes its batch's composition while the pool is still growing,
  and it removes its spec from the only text that ever commanded batching
  (`format_obligations` renders "batch where that is sensible" on two-plus
  unbuilt specs, a count eager per-approval dispatch held at ≤1 forever —
  which is why spec batching was observed roughly twice, both times with the
  pipeline degraded; #1055). So the policy is Nagle's: lane idle → dispatch
  on approval, batched with whatever else is pooled; lane busy → approved
  specs pool as *specs*, never as single-spec queue entries; and the
  `BuildCompleted` nudge is the dispatch moment, listing the pool with its
  file facts — charter-gated like the landing text, because the paragraph
  claims an authority and claiming one the server will refuse is worse than
  silence. The `dispatch_build` obligation goes quiet while the lane is busy
  (pooling is the healthy state, and an obligation raised against deliberate
  policy is how a signal gets trained out of use) and holds for one grace
  after the last build concludes, so the nudge keeps first crack. A lane
  freed by a *cancel* raises no nudge (the echo rule): that same grace is
  what gives a cancelled batch its documented pause before anything
  reconsiders the work. Nothing server-side dispatches anything, exactly as
  before; what changed is when the prompt says to act, when the obligation
  nags, and that "carried" and "lane busy" each have one SQL statement
  (`SPEC_UNCARRIED_SQL`, `BUILD_LANE_BUSY_SQL`) so no two readers can drift.
- **Mode is global; what is per-repo is the subtraction. And there is no
  project delete.** `projects.status` ∈ `active` | `paused` | `archived` gates
  the three places that select work — scout dispatch, the build claim, and the
  *upsert* half of the poll — and nothing else. It is not a second `/mode`:
  mode gates a dispatcher whose real constraints are server-wide (one
  `SCOUT_MAX_CONCURRENT`, one strictly serial build lane, one vm-pool), so a
  per-repo `play` could not run while another repo's build held the lane, and
  the setting would promise what the architecture cannot deliver
  (`store::tests::one_repos_running_build_holds_the_lane_against_every_other_repo`
  pins that in code). One ordered column rather than two booleans, because
  archived already implies not dispatching. Three consequences are load-bearing.
  Archiving must **not** stop the resolution half of the poll: closure is only
  ever learned from *absence in the open set*, so a repo that stopped being
  fetched would leave every task it already has at `gh_state = open` forever
  and strand an `awaiting_merge` batch with nothing to make it loud — one
  fetch per archived project per poll is the right price. The build's check
  lives **inside** `claim_next_queued_build`'s transaction, because
  claim-then-release would flip a build `queued → running → queued` every tick
  and drag its batch's tasks to `building` each time; and it is a `WHERE`
  rather than a test on the head of the queue, so a paused repo is walked past
  rather than stopped at (`next_dispatchable` `continue`s for the same reason).
  **Removal is archive, never delete**: `decisions` is append-only and keyed to
  a project's tasks, and `tasks.project_id` is `ON DELETE CASCADE`, so deleting
  a project takes the audit trail the whole charter rests on with it — there is
  no delete endpoint and there should not be one. `resolve_project` stops
  counting archived projects (or archiving one repo breaks `POST /issues` for
  the one that is left), while naming one explicitly still resolves it, because
  commenting on its open PR and closing its issue are exactly the work
  archiving does not abandon. `POST /projects` and
  `POST /projects/{id}/status` are **human-only and not charter-gated**, on the
  `build-now` precedent: they decide what the pipeline is pointed at rather
  than doing a unit of work inside it. Finally, `UNIQUE(repo_owner, repo_name)`
  is case-*sensitive*, so both add paths (the handler and `tasks add-project`)
  check `find_project_by_repo` case-insensitively first — not a unique index,
  because a `CREATE UNIQUE INDEX` migration can *fail* on a database that
  already holds the duplicate, and a failed migration is a boot failure in a
  process that has already taken the port.
- **What the orchestrator may do lives in `orchestrator_charter`, never in a
  prompt.** Ten independently switchable capabilities (`capture_work`,
  `curate_work`, `comment_on_work`, `retire_work`, `queue_tasks`,
  `dispatch_builds`, `cancel_runs`, `auto_review_specs`, `land_builds`,
  `enroll_agents`), each
  `off` | `shadow` | `live`, human-writable only. The system prompt's
  authority section is *generated* from those rows every turn and the server
  enforces the same rows on the endpoints — one statement of authority, and
  not one a long conversation can talk itself out of. Adding a capability is
  two edits, not one: the enum variant alone grants nothing, because
  `Store::charter_entry` reads a missing row as `off` — the migration's
  `INSERT` is what makes it real, and without it the refusal looks like a bug
  in `authorize`. **All ten ship `live`
  and uncapped** — the charter is a kill switch, not a promotion ladder, and
  the point of the system is that work moves without being asked. What makes
  that safe is the `decisions` ledger under every write: audit and recourse
  after the fact, never pre-approval. `shadow` (server behaviour, not an
  instruction: the call is accepted, the decision is recorded with
  `enforced = 0`, nothing is applied) exists only for **demotion** — a
  capability caught misbehaving, whose reasoning is still worth reading. It is
  never a probation period on the way to `live`; that costs the human more
  attention than just letting the thing act, and attention is the scarce
  resource here. The human is never gated — this governs autonomy, not the
  owner.
- **The charter only binds what the server can attribute, so attribution must
  work under the *tightest* agent permissions.** A write the server cannot
  attribute is recorded as the human's — and the human is never gated, so a
  broken credential does not fail closed, it *escalates*. The orchestrator's
  token therefore reaches it as a server-written `curl -K` config file (0600,
  under the data dir), which is a statically verifiable command under
  `--allowedTools Bash(curl:*)`. Never move that credential into argv (`ps`),
  the prompt (persisted), the environment (an agent under a static allowlist
  cannot expand `$VAR`), or the agent's workdir (a repo checkout it commits
  from). An `X-Tasks-Actor` that is present but does not verify is a 403, not
  a demotion to human.
- **An external agent gets a voice by enrollment, and a failed claim is
  refused, never demoted to the human.** The orchestrator conversation has
  three speakers, and headers decide which one a `POST /orchestrator/messages`
  is: no `X-Tasks-Agent` is the human (never gated, as ever), a *valid* code
  in it is that agent — the turn lands as `ChatRole::Event` under a
  server-written `[agent <name>]` heading, beside `[pipeline]` and
  unmistakable from both — and an invalid, expired or revoked one is a 403
  with the message **discarded**, the `X-Tasks-Actor` rule again, because
  quietly becoming the human is the one direction attribution must never
  fail. The code is the device-code flow one level up from the broker lease
  and holds its custody: `POST /agents` mints 256 random bits returned
  exactly once, `agent_enrollments` keeps only the SHA-256, and the row —
  never deleted, it is the audit trail for turns already spoken — carries the
  name, the expiry (default 4h, bounds refused rather than clamped: a
  credential silently granted a different lifetime is a different grant) and
  `revoked_at`. The **name is chosen by the minter, never the agent**, is
  validated in the store (`require_rationale`'s backstop argument), and can
  never be another speaker — reserved words refused, one active enrollment
  per name — because the name is what the words are attributed to. What an
  enrollment conveys is a **voice, not authority**: an agent turn is input
  the orchestrator is prompted to read as a peer's unverified leads, never a
  gated write, which is why `enroll_agents` (the tenth capability) can ship
  `live` — the convenient flow is asking the orchestrator in chat for a code,
  and its mints and revokes are ledgered with a required rationale
  (`enroll_agent` / `revoke_agent`, both store-only, `applied` by
  construction). The prompt tells the orchestrator to relay a minted code to
  the human verbatim and nowhere else — a persisted transcript holding a
  short-lived message code is the accepted cost of having no other channel to
  the human, bounded by the TTL and by what the code can do. The coverage
  claim, stated exactly: enrollment does not authenticate the human — any
  local process can still omit the header and speak as the human, and the
  loopback bind plus who runs on the machine remain that boundary. What it
  buys is the cooperative case: an honest label the orchestrator can weigh,
  and a bounded, revocable credential instead of ambient human standing.
- **A bind address is not access control against a browser, so the API refuses
  the two shapes only a browser sends.** The other half of the attribution
  rule: a request with no `X-Tasks-Actor` is read as the human's, and the human
  is never gated — so before #985 any page you had open could drive the
  pipeline. Two ways, and they are *not* one: a CORS-**simple** `POST` (no
  body, no `Content-Type`, hence no preflight) whose opaque response does not
  matter because `POST /tasks/{id}/build-now` has already dispatched a VM that
  writes code and opens pull requests; and **DNS rebinding**, where a name the
  attacker controls resolving to `127.0.0.1` makes their page genuinely
  same-origin and lifts the simple-request restriction entirely — `/tasks`,
  `/decisions`, the transcripts, `POST /pull-requests/{n}/merge`. So
  `crate::loopback` is one middleware over the whole router enforcing two rules
  that are **not interchangeable, because each is blind to the other's path**:
  every authority the request states must name this machine's loopback, and an
  `Origin` header — *any* value, `null` and a loopback one included — is a
  refusal. The rebind arrives with an ordinary `Origin` naming the attacker's
  own site *and* a `Host` naming it too; the simple `POST` arrives with a
  loopback `Host` the first rule has no quarrel with. Both apply to reads as
  well as writes, because deciding it per method means re-deciding it for every
  route added later — the shape `authorize` exists not to have. The **route
  list is a separate private `fn routes`** and the layer wraps it, rather than
  a `.layer()` chained onto the end of a 120-line list that a later route can
  be appended *after*, silently unguarded; the property is pinned on an
  **unrouted** path answering **403 rather than 404**, which is what makes it
  hold for routes nobody has written yet. The port is parsed and never
  compared — a browser fills `Host` from the URL's *hostname*, so a rebind
  fails on the host part alone, and comparing `:4800` would refuse every test
  in the tree, which binds `:0` — but the `u16` parse is what stops
  `127.0.0.1:80.evil.example`; `get_all` for `Host`, because "the first one is
  loopback" is the reading a smuggled second one is built to get; and the URI
  authority is checked too, since **HTTP/2 carries no `Host` header** and an
  absolute-form request line must not name a host the header would have
  refused. **The coverage claim is stated exactly, because a security change
  that overstates it is worse than one that states a gap** — the gap stops
  being anybody's job. `GET` is covered against *rebinding* by the authority
  rule and is **not** covered against a direct-to-loopback cross-site
  subresource load: browsers send no `Origin` on `<img src>`/`<script
  src>`/`<iframe>`, so `<img src="http://127.0.0.1:4800/…">` passes both rules.
  That residual is bounded to routes whose responses the attacker cannot read
  (this API sends no CORS headers) and whose only effect is server-side, and
  today exactly two routes are not nil, and both spend the server's own GitHub
  credential outbound. **`GET /decisions/{seq}/reconcile`** is **accepted**
  rather than moved to `POST` — idempotent, locally mutating nothing, answering
  only for a `pending` decision, and named as a `GET` by the obligation loop
  and the orchestrator's own `curl` — so what it costs is one GitHub read per
  pending decision, a rate-limit lever and not the `build-now` hole; closing it
  wants `Sec-Fetch-Site`, which is its own decision. **`GET /viewer`** is the
  second, and it is the cheaper of the two *because of the cache it needed
  anyway*: the app asks on every SSE event, so the answer is held for 30
  minutes (5 on a failure), which bounds a forced read to one GitHub call per
  failure TTL however hard the page hammers it. The same `Sec-Fetch-Site`
  decision covers both. There is **no knob**, and
  the argument is no longer the one that fits on a line: a disable switch whose
  only user bypasses the fix was the whole reason until a *legitimate*
  deployment shape turned out to be refused. Through an SSH `-L` tunnel the
  client connects to `localhost:PORT` and the `Host` arrives loopback, so that
  shape is untouched; through an **HTTP reverse proxy** (`tailscale serve`,
  nginx) the proxy forwards `Host: mini.tail….ts.net` and every request 403s —
  which the menubar's `TASKS_MENUBAR_MACHINES` can name. That is **accepted
  breakage with the tunnel as the answer**, and an `X-Forwarded-Host`-aware
  allowance is rejected rather than unconsidered: it would trust a header any
  client can send, on a listener with no way to tell a proxy from a page, which
  is the guard deleting itself. A trusted-authority list is the honest shape
  and belongs with the bind — this guard **assumes the loopback bind**
  (`server::bind` takes `Ipv4Addr::LOCALHOST`, no knob), so if Tasks ever binds
  beyond loopback the allow-list widens *and* something real goes in front of
  the port, deliberately, rather than a switch being flipped. The one
  deliberate exclusion is the **broker** (port 4801): a second listener on
  purpose, reachable from the VM subnet, where every route already demands a
  live lease — it builds its own router and must not get this layer.
- **A refusal is a no-op, so everything refusable runs before the effect — and
  the rationale check lives at `authorize`, not in the handlers.** The other
  end of the attribution rule: a write the server *does* attribute still has to
  be explained, and the rule that says so used to live only inside the store
  call that writes the ledger row. On every enforced path that call runs
  **after** the GitHub write it explains, so a rationale-less `POST /issues`
  filed the issue upstream and then returned 400 from the bookkeeping — and an
  agent doing the obviously correct thing with a 4xx filed one issue per retry,
  each with no `decisions` row behind it (#957). `close_task` was the worse
  case: its 400 also closed the issue, invisibly in the ledger *and* in the task
  state, since closure is only ever learned from the poller. So
  `server::authorize` takes the whole `DecisionInput` and applies
  `store::require_rationale` itself, at the one call every gated handler already
  makes before its effect — which is why a handler nobody has written yet
  inherits the ordering. The alternative, a per-handler `if
  rationale.is_empty()`, is not a partial fix but the demonstration that the
  shape cannot hold: three of the nine write routes had one, six did not, and
  nothing made them. A required parameter cannot be forgotten, because it does
  not compile. Two orderings inside `authorize` are deliberate. `Off` → 403
  answers **before** the rationale 400, because a rationale cannot rescue a
  capability that was never going to act and telling a caller to write one
  sends it to fix the wrong thing. The rationale 400 answers **before**
  `Shadow`, because a shadow row *is* recorded and one with an empty rationale
  is exactly the unreviewable artifact the rule exists to prevent. (Shadow was
  never the leaky half — it records first and reaches GitHub never — so this
  only moves *when* it refuses.) The store keeps its six call sites as a
  backstop for callers that never went through a handler, and both ends call
  the same `pub fn` so the two sentences cannot drift. **The three bespoke
  pre-checks in `merge_pull_request`, `abandon_pull_request` and `edit_issue`
  stay, deliberately** — they look like three redundant copies of a rule that
  has just been centralised, and they are not: each names what *kind* of
  rationale its route wants ("an autonomous merge must say why it is safe to
  land"), which the generic sentence cannot, and they now fire strictly earlier
  than it. Deleting one loses a better message rather than removing a
  duplicate. What is deliberately **not** fixed is the other side: a GitHub
  write that succeeds and then fails to record still leaves an unattributed
  artifact. Recording first would leave a decision claiming an effect a failed
  call never had, which is worse in the direction the ledger has to be
  trustworthy; closing it properly is an intent-then-confirm record and its own
  change. Everything that can *refuse* refuses first, which is what makes a 4xx
  safe to retry.
- **No VM ever holds a raw `ANTHROPIC_API_KEY` or `GITHUB_TOKEN`, and the
  server holds them only in guarded memory** (#971, after #923/#970 proved
  that anything injected into a VM's environment eventually reaches a log).
  What a run receives at dispatch is a **lease** — 256 random bits, stored
  only as a SHA-256 hash, bound to its session/build and its repo, expiring
  at the run budget plus slack — plus env and a clone URL that point every
  credentialed operation at the in-process **broker**
  (`crates/tasks/src/broker.rs`, `TASKS_BROKER_PORT` 4801): a second
  listener, deliberately not the loopback-only API, reachable from the VM
  subnet at `TASKS_BROKER_ADVERTISE` (bridge gateway `192.168.64.1`). The
  broker validates the lease per request and streams to the real upstream
  with the real credential injected host-side over TLS, so the keys never
  cross the vmnet in either direction — and the env var is still named
  `ANTHROPIC_API_KEY` on purpose, so Claude Code needs no image change and
  #970's name-based redaction masks even the lease. Scopes are enforcement,
  not description: agent leases are `anthropic` + `git-read`, so **a Scout or
  Builder cannot push whatever its prompt talks it into**; the push
  credential exists only as the server's own ~10-minute `land` lease, minted
  per landing on loopback, so even host-side `git` argv never carries the
  PAT. Leases are **rows** because the process that mints one need not be the
  one serving it — a reattach extends by subject without ever knowing the
  token, conclusion revokes best-effort, and expiry is the backstop nothing
  can forget. At rest the keys live ChaCha20-Poly1305-sealed under
  `<data dir>/secrets/` with the unseal key in the **Keychain** (or
  `TASKS_SECRETS_KEY_FILE`); neither artifact alone decrypts anything, a
  sealed store that exists but cannot open **refuses to boot** rather than
  silently falling back to the environment, and `tasks secrets set` rotates a
  *running* server off the file's mtime — no restart. **That unseal key is
  read and written through the `keyring` crate's native backends
  (Security.framework, Credential Manager, Secret Service) rather than
  `/usr/bin/security`, and for an existing install that buys nothing yet**
  (#1003). Three things compound: `set_password` on macOS is
  find-then-modify-*in-place*, so an item the CLI created keeps the CLI's
  access list through any number of native writes; the `security` read stays on
  the read path as the **default** fallback, so nothing breaks and nothing
  improves; and an unsigned dev build is a different *application* to an access
  list on every `cargo build`, which is why `TASKS_SECRETS_KEY_FILE` stays
  first-class rather than being the exotic-host path. So custody is
  **unchanged** until a human runs `tasks secrets rehome-key` — a
  delete-then-add, the only thing that moves an access list, spanning the
  window with a 0600 rescue file **outside the data dir** (`~/.tasks/`, the
  #1012 service home) because a rescue copy beside `sealed.json` would put both
  halves of the two-artifact property in one `tar`; nothing forces that
  command, a `warn!` is the only prompt, and the real benefit arrives with a
  signed application identity (#988, undecided). `keychain_read` and
  `keychain_write` are the whole custody boundary and a second key-store path
  is never the answer — an API route that auto-initialises a store calls them. The same three acts are on the loopback API —
  `GET /secrets` (names, `set_at`, the key-source line, and what is *currently*
  serving each name), `POST /secrets/{name}` (write-only: no type in the wire
  vocabulary can carry a value **outbound**, which makes that structural rather
  than a rule somebody remembers, so there is no read-one route and never a
  value in a response) and `DELETE /secrets/{name}` — with no second
  implementation of custody behind them: auto-init goes through `secrets::init`
  itself, so a paste-created store cannot pick a different key source than a
  terminal-created one. All three are **human-only on the `build-now`
  precedent** rather than charter-gated: the charter governs units of work
  *inside* the pipeline, and these change what the pipeline **authenticates
  as**. What enforces that is the **worker allowlist**, not the check — a
  worker is a local child with no `X-Tasks-Actor`, so it is attributed as the
  human and is never gated, and `DEFAULT_WORKER_CMD` carrying no `Bash(curl:*)`
  is the whole of what stands between the orchestrator and a route that writes
  a **credential**. Anyone who ever adds `curl` to a worker command is adding
  that, not a convenience. Two answers are shaped by the machinery rather than
  by taste: `keychain_write` is `set_password` and therefore
  find-then-modify-*in-place*, so a data dir with no store on a host that still
  holds a `tasks-v2-secrets` item would auto-init and strand *that* store —
  `unseal_key_present()` refuses it instead; and `refresh_if_changed`'s late
  unlock is one-shot per process, so a write this process cannot read back
  answers **200 with a named outcome and never a 5xx** — the ciphertext *is* on
  disk, and a 5xx renders as "your paste did not work", whose obvious response
  is to paste again, forever. Raw values cross module
  boundaries only as `redact::Secret` (no `Display` at all — interpolating
  one is a compile error, not a silent `<redacted>`; constant-time equality;
  zeroized on drop). Two carve-outs are named: a non-http(s)
  `GITHUB_CLONE_URL_BASE` (a `file://` mirror — the integration tests) is
  structurally unproxyable by a git smart-HTTP passthrough, so it clones
  `Direct` while its Anthropic credit still leases; and the broker dying with
  the server means a restart severs in-VM agent connections mid-response —
  which is exactly the transport death the supervisors already resume from
  (#845), and `reload --when-idle` avoids entirely. Env vars keep working as
  boot-captured fallbacks, warned at startup; the sealed store is where
  production keys live. Full design:
  `docs/plans/2026-08-18-credential-custody.md`.
- **Dependency direction:** `crates/vm-pool/*` are pure infrastructure and
  must never depend on tasks crates. App vocabulary enters vm-pool only
  through the `AppProtocol` generic (see `crates/tasks-protocol`). vm-pool
  stays independently publishable.
- **The daemon is the product, and its stable home is never a build cache —
  or an app bundle.** A `cargo clean` once deleted the serving binary out
  from under a live pipeline, because every runnable artifact lived in
  `target/`; embedding the server in Tasks.app and letting the app drive it
  would be the same defect one level up (an app update deletes the serving
  binary, and a headless Mac cannot run the system at all). The shape
  (docs/plans/2026-08-18-end-user-distribution.md) is OrbStack/Tailscale's:
  the server's home is `~/.tasks/bin/tasks`, **launchd owns its lifecycle**
  (`tasks service install|uninstall|start|stop|restart|status` — one
  LaunchAgent, `RunAtLoad` + `KeepAlive`, `TASKS_DATA_DIR` pinned in the
  plist), and every client is just a client. The app is one: `make dist`
  packs a **seed** copy at `Tasks.app/Contents/Helpers/tasks` — never
  beside the app binary, whose `MacOS/Tasks` is the same case-insensitive
  directory entry as `MacOS/tasks` — and when resolution falls through to
  the seed (nothing serving, nothing installed), the app's restart ops run
  `tasks service install`: the one-button install, after which deleting the
  app changes nothing about the running service. Because `KeepAlive` turns
  a plain SIGTERM into a restart, `tasks reload`/`tasks stop` **delegate to
  launchctl when the server is managed** — a reload installs its own binary
  into the home and kickstarts (so `make restart` means "make the service
  serve this build"), a stop unloads the agent — gated three ways, and the
  middle gate is the load-bearing one: the plist's pinned `TASKS_DATA_DIR`
  must equal the data dir in hand, so every test tempdir and second
  deployment is a *different* server and never touches the operator's real
  launchd session. There is deliberately no second LaunchAgent for vm-pool:
  the server supervises its own pool via autospawn
  (`TASKS_VM_POOL_AUTOSPAWN`), whose unset default is **derived from the
  binary's surroundings** (no `crates/tasks/Cargo.toml` above
  `current_exe()` → installed → on; checkout artifact → off, because a
  developer's deliberate pool restart is a race an eager server would lose
  politely but lose) — the same probe that derives the app's `--no-build`.
  Safe with no leader election only because the pool refuses an occupied
  socket. Boot mode stays the quiet default; `tasks service install
  --default-mode play` is the explicit, crash-restart-inclusive opt-in.
- **A release is one number, cut by a human, and its changelog is generated
  from the commits rather than written per merge.** The number is
  `0.1.<commit count>` — `build-stamp`'s identity, borrowed rather than a second
  scheme — so the annotated tag, the DMG name, the CLI zip, the `CHANGELOG.md`
  heading and `GET /version` all say the same thing;
  `[workspace.package] version` is declared **inert** in `Cargo.toml`, because
  nothing here publishes to crates.io and a second number is a second thing to
  keep in step. `make publish` is the whole act and it is **human-only with no
  API route at all** — the `build-now` / `POST /projects` category, deciding
  what the project publishes rather than doing a unit of work inside the
  pipeline. Three things hold its shape. **Nothing public happens until the
  artifacts are verified**: build → sign → notarize → staple → verify, and only
  then tag → push → upload, so a failed notarization retries with nothing to
  un-tag; and `push` sends both refs with `git push --atomic`, so a `main`
  rejected because someone merged under us leaves no orphaned tag. **The
  version is `count + 1`, written in exactly one place** — the changelog commit
  is inside its own release, so `scripts/changelog.sh --next-version` owns that
  arithmetic and the Makefile calls it rather than repeating it, since two
  copies of an off-by-one is how one of them gets fixed alone; every stage is a
  **sub-make**, because `BUILD_VERSION :=` is expanded at parse time and would
  be stale the moment `changelog` commits, and `tag` **refuses unless
  `CHANGELOG.md`'s newest `## v` heading is the stamp it is about to tag**,
  which catches that mistake in both directions rather than documenting it. And
  **the walk is not `--first-parent`**: a build merged into another build's
  branch rather than into the trunk is reachable from `main` and off its
  first-parent chain, so a first-parent-only log drops it silently — this
  pipeline stacks builds routinely, and `28c879e` (the Mac app) is the proof on
  this repository's own history. The walk keeps a commit that is on the trunk
  *or* is itself a pull-request merge by subject shape, which is also what puts
  the housekeeping denylist to work; the denylist is **stated, never a
  heuristic**, so a new kind of noise shows up in a section and gets added
  deliberately instead of a cleverness quietly eating a real entry. The PR title
  comes from the merge commit's **body**, where GitHub already put it, with `gh`
  as the bounded fallback — free, offline, deterministic. What
  `check-publish` cannot answer is stated rather than papered over: it reads
  check runs for HEAD and `changelog` then commits on top, so the tagged commit
  is one CI never saw. There is no fix — check runs for an unpushed commit are
  unconditionally absent, so a re-read would refuse every release — and what
  bounds it is that the commit is changelog-only and `make release` builds every
  artifact locally from it. Its dirty-tree refusal is a **second copy** of
  `release`'s `check-clean-tree` on purpose: this one has to refuse before the
  changelog commit. Design: `docs/plans/2026-08-20-release-flow.md`.
- **What never leaves this machine may be unsigned; what is downloaded is
  signed, notarized and stapled — and the chain refuses rather than
  degrades.** `make app` and `make dist` stay unsigned, and that is not an
  oversight to fix later: they install to `$HOME/Applications` on the machine
  that built them, where Gatekeeper has nothing to assess. A download does,
  and it arrives carrying `com.apple.quarantine`. So there is a second chain
  (`make release`, staged in `dist/`, full reasoning in
  `docs/plans/2026-08-19-signing-and-notarization.md`) and **every target in it
  errors on a missing credential rather than emitting an unsigned artifact**,
  because the failure worth preventing is the quiet one: the `.dmg` that built
  fine and reached a link unsigned, which nobody discovers until a stranger
  double-clicks it. Four things hold its shape. **Sign inside-out and never
  `--deep`** — `--deep` re-signs nested code with the *outer* bundle's
  arguments, and the documented failure is a signature that verifies locally
  and comes back Invalid from the notary service minutes later; the standalone
  CLI is then a **copy of the signed seed** rather than a second signing act,
  so the binary in the release and the one at `Contents/Helpers/tasks` are byte
  identical. **`stapler` is the gate, not `notarytool --wait`**, since a ticket
  exists only for an Accepted submission — and `dmg` refuses unless the bundle
  already validates, which is what stops the silent version of the ordering
  mistake, a DMG built before notarization that looks identical and ships an
  app whose first launch needs Apple reachable. **The release bundle is
  assembled by the same two recipes as the dev bundle**, with `APP_BUNDLE`
  overridden on the sub-make command line (a command-line variable beats the
  `:=` and propagates), so the thing that is downloaded cannot quietly differ
  from the thing that was tested; `app-install`'s two guards — the unset-`HOME`
  refusal and the "Tasks is running" note — are gated on *one* comparison
  against the default path, since neither has anything to say about an absolute
  override. And **a bare Mach-O cannot be stapled**, so the CLI archive is
  notarized-but-not-stapled and Gatekeeper fetches its ticket online; that is
  acceptable **for a reason specific to this system rather than in general** —
  nothing here works offline, so a machine that cannot reach Apple cannot run
  what it just unzipped either. The stapleable alternative is a `.pkg`, which
  needs a *Developer ID Installer* certificate, a different class from the
  Application one; deferred and named. One measured fact is a **checklist gate
  and not a pitfall**: `std::fs::copy` on macOS clones extended attributes, so
  a downloaded, quarantined bundle installs a `~/.tasks/bin/tasks` that carries
  `com.apple.quarantine` and makes launchd's exec a Gatekeeper assessment —
  confirmed on a Mac, not suspected — and that is the one-button install on a
  fresh machine, the path with nobody at a terminal. `install_binary` is
  deliberately **unchanged** here, because a fix wants a real signed artifact
  to test against; when it is made it clears the attribute *after* verifying
  the copy's own signature, never before. Finally, `dist/` is this chain's
  staging directory and `make dist` does not write there — `release-clean`
  empties only the former.
- **Agent engine is Claude Code / the Agent SDK — never a home-rolled agentic
  loop.** The server consumes Claude Code's typed output (stream-json, hooks,
  MCP tools, structured outputs); it does not reimplement the loop.
- **A read-only agent is spelled in verbs, and the quoting is what makes that
  expressible.** The only worked example this repository had was
  `BRIEFING_CMD`, deleted whole with `crates/tasks/src/briefing.rs` in #933,
  and the shape is worth keeping because it is not derivable from anything
  left in the tree: `claude --print --allowedTools
  "Bash(gh:*),Bash(curl:*),Bash(git log:*),Bash(git diff:*)"`.
  `--allowedTools` is default-deny and **prefix-matched**, so the list is
  written in verbs rather than tools — `Bash(git:*)` would hand the agent
  `git push` and `git commit` along with the log, which is why the example
  spelled `git log` and `git diff` out separately instead of saying `git`.
  And for an agent whose whole point is that it cannot write,
  `--dangerously-skip-permissions` is never the way to give it more access: it
  *discards* the allowlist rather than widening it, so what comes up is not a
  more capable read-only agent but an unrestricted one. The flag is not
  forbidden in general — an orchestrator deliberately pointed at the checkout
  to run as a full dev agent takes it, and the `ORCHESTRATOR_WORKDIR` row
  below says so; it is forbidden where the restriction *is* the design.
  **The quoting is the load-bearing half**: `Bash(git log:*)` contains a space,
  so a command string split on whitespace shatters it into `Bash(git` and
  `log:*)`, and the agent comes up holding two permissions that match nothing
  while default-deny refuses every call they were meant to allow. A restrictive
  multi-tool allowlist therefore needs a quoting-aware split, and
  `orchestrator.rs` now **is** the template — `split_command` lives there and
  `invoke` spawns through it (#976). It did not until 2026-08-20, and the way
  that stayed invisible is the part worth keeping: the orchestrator's *default*
  command (`DEFAULT_ORCHESTRATOR_CMD`, `crates/tasks/src/run.rs`) has an
  allowlist of one space-free token, `Bash(curl:*)`, so `split_whitespace` was
  never observably wrong there — and the one shape the env table invites an
  operator to write was the one shape the variable could not carry. It fails
  **closed**, the fragments matching nothing, which is why this was a bug and
  not an incident, and also why it would have been met the confusing way: by
  someone tightening permissions and watching the agent lose a capability
  instead of gaining one. `an_unquoted_command_splits_exactly_as_whitespace_did`
  pins that the change is inert for every command without quotes in it. The
  splitter is deliberately not a shell — grouping only, no escapes and no
  expansion, since an agent under a static allowlist cannot expand `$VAR`
  anyway. The worked example the tree lost with `crates/tasks/src/briefing.rs`
  (#933) is still only in git: `git show 63a1fb6^:crates/tasks/src/run.rs` holds
  `DEFAULT_BRIEFING_CMD` and the doc comment giving its reasons.
- **A dead API connection is resumed in the supervisor, never re-dispatched
  from the host.** Agent processes die intermittently at ~380s elapsed (#845)
  when the connection drops mid-response — below the agent, in the VM's network
  path, so nothing here can prevent it. What the supervisor can do is re-invoke
  the agent with `--resume <session_id>`, read out of the stream-json it is
  already forwarding: same conversation, same worktree, same `NOTES.md`. A
  host-side retry would get a new VM and a fresh clone and keep none of the
  three — and for a Builder that worktree *is* the implementation. The
  classifier and every guard live in `crates/tasks-protocol/src/agent_run.rs`,
  and **the guards are the load-bearing part**, because the failures you must
  not retry look superficially like the one you must: an OOM kill meets the
  same limit with a larger conversation, a missing terminal record means the
  host is deallocating this VM right now, and a command that already selects a
  session belongs to the operator. Two other rules hold that shape: read the
  session id, never inject one (`--session-id` would overwrite the operator's
  command), and never restate the task in the resume prompt — the task is above
  it in the conversation, and re-sending it is how a resume silently becomes a
  restart. A transport death also names itself in the terminal reason; "SPEC.md
  not found" or "no commits" alone reads as a verdict on work that was never
  judged — and it is no longer charged for one either, which is the rule below.
- **The context gauge's denominator is transcribed from the agent, never
  derived from the model name.** The orchestrator is one long-lived Claude Code
  session, so "how full is it" is a real operational question — and the
  tempting way to answer it is a `model -> window` table in our code, which is
  a fact owned elsewhere that goes stale the next time a model ships. It is not
  needed: the stream-json `result` record carries `modelUsage`, keyed by wire
  id, and every entry states its own `contextWindow`. Three things follow. The
  entry is selected by matching the **last main-chain assistant record's**
  model, because sub-agents routinely run on a smaller one and reporting their
  window against our reading would scale it wrongly; with nothing to match and
  more than one candidate the window is `None`, since a wrong denominator is
  worse than none and the token count still shows either way. `context_tokens`
  and its three parts come off that same assistant record, so the parts sum to
  the whole a client draws beside them — and never off `result.usage`, which
  aggregates every internal turn and is a *bill* (`tick_tokens`, routinely
  several times the window; never a segment in the bar). And compaction is
  **counted, not inferred from the gauge dropping**: it happens inside the
  agent and keeps the session id, so the only honest signal is the
  `system`/`status` record's `compact_result: "ok"`. A zero count means "none
  counted since the column existed", which is why the app shows the row only
  when there is one — rendering it as "never" would claim history the counter
  never had.
- **A strike is charged for a verdict, and for nothing else.**
  `dispatch_attempts` and `build_attempts` exist so that work which genuinely
  cannot be done stops consuming the pipeline after three tries; a run that died
  of something unrelated to the work has learned nothing, so charging it
  identically means three infrastructure deaths reject a good task or `blocked`
  a good spec (#884, and #825 where five scout attempts burned in one night
  without a verdict among them). `FailureClass` (`Verdict` | `Transport` |
  `Cancelled` | `Orphaned`) is stamped by the **supervisor** — the only thing
  that knows how the agent died, from `AgentEnding::is_transport()` — and read
  by the host **off the field, never off the reason text**: a reason is prose
  written for a human, and a decision that greps it changes meaning the next
  time someone improves a sentence. One decision point per dispatcher
  (`ScoutError::failure_class` / `BuilderError::failure_class` into
  `Strike::for_class`), so the restart-orphan exclusion is a *class* rather than
  a second mechanism beside it. Not every class comes off a terminal event,
  though: the failures where there *is* no terminal event are classified by the
  **host**, in those same two functions — `Egress`, because the agent finished
  and the push is what failed; `StreamClosed`, because vm-pool going away means
  the host stopped being able to observe the run at all; and `Suspended`,
  because a budget the machine slept through was never offered to the run (see
  *Budgets and a host that sleeps*). vm-pool is a
  separate daemon this document says to restart *ahead* of the server, so the
  second one is routine maintenance rather than a judgement, and it used to
  charge the whole batch. Wire skew runs both ways and only one way is
  obvious: `#[serde(default)]` covers an older supervisor omitting the field,
  while a hand-written `Deserialize` decays an *unknown* class to `Verdict`,
  because a lost terminal event does not cost a strike — it costs the run its
  outcome and hangs it until the deadline. What stays charged is deliberate and
  is what the negative tests pin: an agent that ran to completion and produced
  nothing usable, a `Timeout` that had all but a small share of the budget
  **awake** (a nap the run worked through is still a timeout — #944), an OOM kill (a memory
  limit is a real property of the work in that VM, and #828 exists to make that
  death legible as itself), and every pre-agent setup failure — a clone against
  a base branch that is gone fails identically every time, and waiving it would
  retry forever with nothing to stop it. Every waived strike appends a `Note`
  naming the class and the unchanged count, because an attempt that was not
  spent is otherwise indistinguishable from a cap that has been switched off.
- **A GitHub outage holds dispatch, because that failure is the one in its
  family that is *preventable*.** A Scout clones and a Builder clones, so work
  dispatched while GitHub is down dies at its first step — a pre-agent setup
  failure, which the rule above deliberately keeps charged, so one outage spent
  a three-spec batch three build attempts of three for something no spec did
  (#939). **Do not relax the strike rule to fix this**: waiving pre-agent setup
  failures is the fix that looks equivalent and is not, since a clone against a
  base branch that is gone fails identically forever and waiving it retries
  forever with nothing to stop it. The poller already knew and told nobody, so
  `github_health::GitHubHealth` is an **in-memory** record (never a table — that
  is a GitHub-owned fact with a timestamp on it, and the vm-pool precondition it
  mirrors is in-memory too) written from *every* GitHub call a pass makes and
  read by both dispatchers as a precondition. It cannot be read off `poll_once`'s
  return value: a failed fetch is logged and skipped so one repo cannot stall
  intake for the others, so the pass returns `Ok` through an outage that failed
  every call in it. What counts as an outage is decided **off
  `GhError::is_unavailable`, structurally** — 5xx, or a request that never got a
  response — and never off the message text, the same rule `FailureClass`
  follows; `429` and every other `4xx` are excluded, because they are GitHub
  *answering* and a hold on one would clear from nowhere. Three rules keep a
  hold from becoming a silent stall, and each is a way this could go permanently
  wrong: **absence of evidence never holds** (a tokenless server observes
  nothing at all and would otherwise never dispatch again), **only a fresh
  success clears one** (a 404 on one PR is not GitHub coming back), and **a hold
  nobody is refreshing expires** — generously, at 10 × `TASKS_POLL_INTERVAL`
  floored at 10 minutes, because during an outage the poller's own requests are
  the slow kind and a tight window would expire the hold *during* the outage it
  was set for. The window is bound at construction rather than at each read, so
  the scout loop, the build lane and `/status` cannot disagree about whether a
  hold is in force. The mistakes stay asymmetric on purpose: one failed
  observation is enough, since a false hold costs at most one poll interval of
  latency and loses nothing, while a false dispatch costs one of three attempts
  on work that did nothing wrong. Holding is safe here in a way it usually is
  not — `POST /builds` still records the request, queued work stays queued,
  nothing is charged, and the batch takes the lane on the tick *after* GitHub
  answers — which is also why the build lane's check sits in the match guard
  **ahead of `claim_next_queued_build`**: claiming would flip the build
  `queued → running` and drag its batch to `building` on every tick of the
  outage. It is announced once per edge as a `Note` and reported for as long as
  it lasts on `/status`, `tasks status` and the Server window, so an idle
  pipeline can always say why it is idle. **No obligation and no orchestrator
  prompt section, but a brief line** — the orchestrator cannot fix GitHub, and
  an undischargeable signal raised every pass is how a signal gets trained out
  of use; a brief line is neither, and this is the precedent `images.rs`
  already set (reported on `/status`, `tasks status`, the Server window **and**
  the brief, with no `ObligationKind` behind it). #939 adopted three quarters
  of that and dropped the quarter that matters most here, because during an
  outage the orchestrator is not a bystander: it is the process still pushing
  merges, comments, closes and issue edits *through the server* over the same
  API returning 503, and it does not read `/status` unprompted. So
  `Brief::github_hold_facts` is silent on a healthy pipeline, reads
  `GitHubHealth::hold` rather than deciding freshness a second time, and is
  worded as what the hold is — a hold on **dispatch**, which says nothing about
  whether a given write will succeed. The reading it has to produce is "expect
  your writes to fail and stop retrying", never "you are blocked": the
  orchestrator has plenty it can still do, and a line that reads as a stop
  order costs a turn. It reaches
  the same place `ObligationKind::StaleImage` does and by a different road:
  there the means are present and the *decision* is not the orchestrator's,
  here the decision would be its to make and the means do not exist. The one
  remaining gap is deliberate: the record is in memory, so a restart mid-outage
  can dispatch once, and closing it would mean persisting a GitHub-owned fact or
  blocking dispatch on a signal that may never arrive. It is narrower than it
  looks, since a boot takes `TASKS_DEFAULT_MODE` (`pause`) and the path that
  carries `play` over is `tasks reload`, a deliberate human act.
- **A broker outage is the same failure one layer down, and it is the one every
  other check is blind to.** Every credentialed operation inside a VM is
  redeemed against the broker — the Anthropic traffic and the **git clone**
  both — so a broker that stops answering fails every scout and every build at
  the clone. That is a pre-agent setup failure, which the rule above charges
  deliberately, so an outage of one minute does not *delay* work, it
  **destroys** it: on 2026-08-18 19:35–19:36 UTC #996 burned two attempts in 12
  seconds and #982 burned all three in 27, and both were `rejected` — terminal —
  for a fault neither task had anything to do with, with ten more queued tasks
  seconds behind (#1006). Nothing that existed covered it, and the near misses
  are instructive: `GitHubHealth` is written from calls the *poller* makes, and
  the poller talks to `api.github.com` directly with the server's own
  credential, so GitHub read perfectly healthy throughout; `PoolHealth` has the
  wrong subject, since a pool with free slots and a dead broker allocates
  happily and then dies at the clone. #939 already settled where the fix goes —
  **in dispatch, not in classification** — so `broker_health::BrokerHealth` is
  the fifth standing hold, in memory beside the other three and at the same
  gates. **The evidence has to be a probe, and that is the one thing genuinely
  harder here than for `github_health`**: the broker's own successful
  request-serving would be the honest passive signal, but during an outage
  there are *no* requests to observe — the runs that would generate them are
  exactly the runs that are failing, so a passive record is blank at precisely
  the moment it is needed and its only clearing signal is the thing the outage
  prevents. It also settles the other half: the clone failure is reported by
  the supervisor, inside the VM, as prose (`clone: … Empty reply from server`),
  and deciding a hold off message text is what `FailureClass` and
  `GhError::is_unavailable` forbid — a host-side probe needs no protocol change
  and no image rebuild, and it observes the fault *before* a VM is spent on it,
  which an in-VM signal arriving after the allocation and the teardown
  structurally cannot. It reuses `doctor::probe_broker_within` rather than
  growing a second opinion (one implementation with a timeout parameter — three
  seconds on the dispatch path where a gate awaiting it stalls the tick, ten for
  a human running a diagnostic), so both of that probe's load-bearing properties
  carry over: it goes to the **advertised** address and never loopback, because
  during the firewall outage that produced the check loopback answered a correct
  401 while the bridge gateway returned zero bytes; and **an unauthenticated 401
  is the success condition**, since every broker route demands a lease first.
  Which answers hold is where the three rules bite, and the first one bites
  harder here than anywhere else in the tree. **`Unreachable` never holds** —
  apple/container's bridge gateway does not exist until the first container has
  started, so on a cold machine the advertised address is unreachable as a
  matter of course, and a hold on it would prevent the container that creates
  the gateway: the gateway would never appear and the hold would never clear, a
  gate only the gate itself keeps closed, which is the shape the update watch
  refuses for a pre-boot image observation. **Only a fresh 401 clears one**, so
  an unreachable address does not release a hold a silent listener set. **A hold
  nobody refreshes expires.** And a listener that **spoke HTTP** is deliberately
  not an outage, whatever it answered — that is the line
  `GhError::is_unavailable` draws when it excludes every `4xx`, and a hold set
  on a thing that answers has no clearing signal of its own. `BrokerHealth::unprobed`
  is `pub` on the `Secrets::for_tests` precedent and is **structural rather than
  a pre-claimed probe**: claiming one would go quiet for the probe interval and
  then start probing, so a test that happened to run past it would fail
  intermittently for a reason it is not about. The strike rule is **not**
  relaxed, exactly as #939 said. What is deliberately left alone is the
  aftermath: `rejected` is terminal and there is no endpoint that returns a task
  from it, so the two tasks that died that night are still there. Whether
  `rejected` should be reversible, and by whom, is its own decision.
- **The substrate under all of it gets a hold too, and the probe is the tool
  rather than the refusal.** On 2026-08-19 the container runtime was not
  running — `apiserver is not running and not registered with launchd`, so it
  had not survived a reboot — and dispatch resumed anyway: in one play window
  **3 builds failed** on `allocate failed: runtime error: transport closed
  before Ready` and **12 tasks were charged one of their three dispatch
  attempts** and left stranded (#1017). vm-pool itself was healthy and current;
  it accepted every allocate and only then discovered it could not start a
  container. At twelve a window that is three windows from rejecting the whole
  queue, and the asymmetry the GitHub hold rests on applies unchanged: a false
  hold costs a tick of latency, a false dispatch costs one of three attempts on
  work that did nothing wrong. `pool_health` is the closest existing shape and
  the **wrong subject** — it asks whether vm-pool has a *slot*, and a pool with
  every slot free answers cheerfully while nothing can be started at all. So
  `runtime_health::RuntimeHealth` is the sixth standing hold, and the decision
  worth writing down is that it is **not** a widening of #930's strike waiver.
  #930 waives `Capacity` alone and keeps `Runtime` charged, on the argument
  that "a reference that does not resolve refuses identically forever" — right
  about `Image`, and beside the point here, because with the probe in place the
  failing allocate **never happens**. Today's outage would have cost *zero*
  attempts rather than twelve, which no strike waiver could deliver: a waived
  strike still spends an allocation, a pool slot and a teardown per task,
  forever. `FailureClass::for_service_error` is therefore untouched. **The
  evidence is `container system status` and deliberately not the refused
  allocation** the dispatchers already collect: a refusal-driven record's
  natural clearing signal is a *successful allocation*, which is the one thing a
  hold prevents — the same circle that keeps `pool_health` on a `status` round
  trip. Asking the tool has two consequences and the second is the prize: it
  needs **no protocol change and no vm-pool restart** (a field on `pool_status`
  was the other honest design and is inert until the pool reporting it is
  restarted, which during an outage a *reboot* caused is exactly when nobody
  has restarted anything), and it observes the fault **before the first
  allocate**, so an outage costs a log line rather than a strike — a record
  written from refusals can only ever be one task late. It reuses
  `doctor::probe_within`, the same one-implementation-with-a-parameter move as
  the broker's. The three rules again, and the first is doing real work:
  **absence of evidence never holds** — a host with no `container` on `PATH` is
  not a broken host, since vm-pool can be built on `SupervisorRuntime`, the test
  harnesses are, and a Linux checkout has no apple/container at all, so
  `Probe::Missing` touches nothing; a probe that **timed out** touches nothing
  either, because it is not an answer and `doctor` reads the same outcome as a
  `Skip`. **Only a zero exit clears one**, and **a hold nobody refreshes
  expires**. It is ordered **last** among the six, because it is the most
  expensive question asked — a subprocess rather than a socket round trip — so
  everything cheaper answers first. The other half of #1017, the 12 tasks
  stranded in `scouting` because `Scout::dispatch` claims before it allocates,
  is #967's unwind and already landed; the two compose exactly as that issue
  warned they must, since without a hold the fix that returns tasks to `Queued`
  is a retry loop at `DISPATCH_TICK`.
- **The scout dispatcher asks *what is next* and *may I start it* in one call,
  because a call site is not pinnable by a test of its predicate.** `top_up`
  reads the six dispatch holds twice — once before its loop for cost, and once
  **per scout**, since each iteration starts a VM and a pause landing mid-pass
  must stop the next one rather than merely the next pass (#948). That
  per-scout read was correct and invisible: deleting it left the whole suite
  green (#973), and `dispatch_held_answers_from_live_state_every_time` could
  not see it, because a test of a predicate never observes a caller that
  stopped calling it. **The obvious repair is not available, and this is
  measured rather than argued**: an integration test that races a pause against
  a second dispatch fails identically against the correct code and against the
  mutant, since nothing is awaited between the hold read and `in_flight.spawn`
  — the very fact that makes the fix correct is what leaves no window to
  schedule against, and widening one artificially (thousands of skipped rows,
  so the scan is slow) pins the test to `next_dispatchable` staying a full
  table walk, so the first `WHERE`-clause optimisation makes it flaky. So the
  rule is pinned **structurally**, on the `server::ledgered` precedent:
  `crates/tasks/src/dispatch_gate.rs` answers both questions in `next_scout`,
  and the only thing `top_up` can dispatch is a `Cleared` whose fields are
  private to that module. **The load-bearing half is that `next_dispatchable`
  is private, not that the enum is new** — had `next_scout` been added *beside*
  it, a later refactor could call the old one and go green again; with it
  private there is no route from `run.rs` to a `(Task, Project)` at all, so a
  pass that starts a VM without re-reading the holds cannot be written rather
  than being written and caught. The ordering inside `next_scout` is scan
  **then** holds, and that is not tidiness: a human pauses and *then* queues
  work, so anything the scan can see was committed after the pause was, and a
  read that follows the scan cannot miss it — read first, the window reopens.
  `NextScout::Held` and `Drained` are distinct because the caller's `debug!`
  says which kind of idle a stopped pass was, which is the question `/status`,
  `tasks status` and the feed already answer elsewhere; an `Option<Cleared>`
  would also leave the ordering unpinned, since a hold read first answers
  `None` for an empty queue and nothing could tell. Two unit tests hold it, one
  mutation each and neither catching the other's: deleting the in-`next_scout`
  read fails only `a_hold_that_lands_after_the_pass_began_stops_the_next_scout`
  (which takes the first `Cleared` of a pass, reserves it in `skip` exactly as
  `top_up` does, and commits the hold between the two turns — one leg per
  reason, releases included), and hoisting it above the scan fails only
  `the_holds_are_read_after_the_scan_not_before_it` (every hold live, the one
  task `Backlog`: the answer must be `Drained`). `dispatch_held_answers_from_live_state_every_time`
  stays deliberately — it is the narrower statement and the one that fails
  first if the predicate itself breaks rather than its call site. **The serial
  build lane calls `dispatch_held` too**, since #965. It kept four inline
  copies for a while, on an argument that was correct as far as it went — the
  lane claims at most one build per pass, so it already re-read every gate for
  every container it started and never had `top_up`'s bug, while sharing meant
  restructuring the match guard the copies lived in, a guard being exactly what
  cannot `await`. What the argument does not cover is the property the function
  exists for: with two implementations a *fifth* reason reaches scouts and not
  builds unless somebody remembers both, and the guard is gone, so it does not
  have to be remembered. `pool_hold` is **private again** — it was `pub(crate)`
  for those copies — which is the `Cleared` argument one level down: leaving it
  reachable would leave a route by which a caller assembles three of the four
  checks and goes green. `a_full_pool_never_claims_a_build` is what that buys,
  and it is the test that goes red if the lane is ever swapped back to a
  partial copy.
- **A full pool is a property of the moment, so it costs no strike and starts
  no retry loop — and those are two mechanisms, not one.** A Scout refused a VM
  used to be charged a dispatch attempt *and* left in `Scouting`: `dispatch`
  claims the task before it allocates, the `?` on `allocate` returns before any
  session row exists, so `finalize_failed` — the only path back to `Queued` —
  never ran, and `next_dispatchable` (which reads only `Queued`) could not see
  the task again until the next boot's `reconcile_orphaned_work`. Three busy
  moments therefore rejected a task nothing had judged (#930), and a single one
  cost a task a restart (#967); on the morning of 2026-08-19 a host whose
  container runtime was down charged twelve tasks an attempt each and stranded
  every one of them. The **strike** half reads a *field*: vm-pool's
  `ServiceEvent::Error` now carries a `ServiceErrorKind` (protocol revision 2,
  `#[serde(default)]`, unknown values decaying rather than failing the decode —
  an undecodable error response is never delivered, which turns a refusal into
  a hang), `ClientError::Service` carries it, and
  `FailureClass::for_service_error` states the reading once for both
  dispatchers. Only `Capacity` is waived, because the line is **whether the
  condition clears by itself**; `Image` and every other kind stays charged, and
  so does `Unspecified` — which is what a vm-pool older than the field says, and
  it is the routine case, so a waiver there would silently spare every permanent
  misconfiguration on every old daemon. That the fix is inert until the pool is
  restarted is said out loud once per connect (`run::report_error_kinds`) rather
  than discovered in a rejected task, and `ERROR_KIND_PROTOCOL_VERSION`
  deliberately **gates nothing**: an added *field* needs no gate (its absence is
  an answer every reader handles), where an added *command* does, because an old
  service rejects the line at decode time. The **stranding** half is
  `Scout::start` as the boundary — one `match` in `dispatch` undoes the
  `Scouting` claim on every pre-session failure, reading the state back so it
  can only undo its own change, best-effort so a bookkeeping error cannot
  replace the error being reported, and writing no `Note` because
  `record_outcome` already writes one. Neither half works alone, and the
  asymmetry is the point: waiving the strike *removes the backstop* that
  bounded the retry, so `pool_health::PoolHealth` — the third dispatch hold,
  beside `github_hold` and `UpdateWatch`, at the same two gates — is **mandatory
  rather than preferable**, or the requeue becomes a 500 ms loop against a pool
  that stays full. Its evidence is a `status` round trip and **never** a
  classified refusal: `available` is the exact quantity `Pool::allocate` checks,
  this codebase does not decide on reason text, and a refusal-driven record
  would want a successful allocation as its clearing signal — which is the one
  thing a hold prevents. `probe_due` claims the slot, so the two gates share one
  round trip *and* exactly one of two racing callers writes the edge `Note`;
  announcing off the `hold` predicate instead would be a `Note` per loop per
  tick, which is the flood the whole change exists to prevent one level up.
  Nothing observed never holds, an unreadable `status` touches nothing, and an
  unrefreshed record expires. `0 of N` is reported rather than "full", because
  `0 of 0` is a `VM_POOL_MAX_VMS` that can never dispatch and `0 of 6` is work
  or a leak holding every slot. The gate makes the unwind rare, not
  unnecessary — a slot can go between the probe and the allocation — which is
  why both are tested, and disabling either alone produces a different named
  failure.
- **A build is whichever tip the reconciliation chooses, and the head is read
  out of the bundle — in that order.** `git rev-parse HEAD` and
  `refs/heads/<branch>` are the same commit only while HEAD stays symbolically
  attached to the host-chosen branch; a rebase, a `git checkout <sha>` or a
  branch of the agent's own detaches it, and the branch ref silently stops
  tracking the work. That is #891, where a finished build was discarded for
  `bundle tip … does not match the reported head …`. Both halves of the fix are
  load-bearing: `reconcile_checkout` decides which tip the build *is* (by
  ancestry, with a rebase guard ahead of it, since git parks HEAD on a partial
  replay while the branch still holds the complete history), and `package_bundle`
  then reads `head_sha` back out of the bundle, so there is **one** value where
  there were two that had to agree. Reading from the bundle alone makes the
  error disappear while shipping a stale tip; reconciling alone leaves the
  two-values problem. And the reconciliation runs **before the sweep**, because
  the sweep lands on whatever HEAD is: on a stranded checkout, sweep-first
  manufactures a divergence no ancestry rule can undo, and the PR ships the
  sweep with none of the implementation. Whatever the decision chooses against
  rides the bundle as `refs/abandoned/<branch>` and is never pushed, so no arm
  of it can lose a commit. The server's tip check stays, and now measures
  transport integrity rather than racing the VM — and it no longer costs the
  build, because the VM is deallocated before egress runs, so a rejected bundle
  is written to `<scratch_root>/rejected/` (deliberately never swept) with the
  `git fetch` that recovers it named in the failure reason.
- **A preserved bundle is deleted only once its work has been reproduced —
  never by age, never by disk usage.** A build whose branch cannot be pushed
  holds the only copy of a finished implementation, because the VM is
  deallocated before egress runs; `<scratch_root>/rejected/<build_id>.bundle`
  is that copy. So the retention policy (`run::reclaim_bundles`, driven by
  `Store::build_superseded`) deletes one only when **every spec in its batch
  was carried by a later build that `succeeded` and every task in it is
  `done`**. Both halves are load-bearing and neither is a proxy for the other:
  a later build that only opened a PR is not evidence, since `watch_merges`
  can still find that PR closed unmerged and unwind the batch back to
  `ready_to_build`, at which point the bundle is the head start again — and
  `done` means the issue closed upstream, which is exactly "the merge landed".
  One unreproduced spec keeps the whole bundle, because a bundle is one file
  over a whole batch and there is no half-bundle to keep. "Later" is `rowid`
  and not `created_at`: two builds stamped inside the same second would let a
  build supersede *itself*. A bundle nobody ever rebuilt is therefore kept
  forever, and that is the behaviour rather than the leak. The **filesystem is
  the only record** — `crates/tasks/src/bundles.rs` reads the directory on
  every request, and there is no table, no migration and no cached size,
  because that directory is one a human works in and a row asserting a file
  exists goes stale the moment somebody `rm`s one. Two consequences: a router
  without the bundle service answers **503 and never `[]`** ("nothing was
  preserved" is the one wrong answer to give about work that exists in exactly
  one place), and `DELETE /builds/{id}/bundle` is **human-only**, refused to
  the orchestrator outright like `build-now` — what survives the policy is by
  construction work nobody reproduced, and there is no undo. Recovery is a
  `git fetch` printed in full rather than a button, because the file is on the
  server host and the fetch runs in the human's own checkout; the bundle is
  thin, so `base_sha` is reported beside the command rather than left implied.
- **A cancel interrupts the dispatcher's drain; it never just removes the VM.**
  `POST /sessions/{id}/cancel` and `POST /builds/{id}/cancel` write a durable
  `cancellations` row, and `crate::cancel::bounded` — the `tokio::select!` the
  wall-clock deadline already used, with a third arm — is what wakes the drain,
  which then tears the VM down through the path it always used. Killing the
  container by hand (or calling `deallocate` and nothing else) is precisely the
  bug (#876): the drain is parked on a vm-pool stream that will never produce
  another event, so the row stays `running`, the serial build lane stays
  occupied, and nothing tells the operator the cancel took. The request is a
  store row rather than a channel because the process taking the request need
  not be the one following the run — a run picked back up by
  `resume_in_flight` was never subscribed to anything. A cancelled run is
  `cancelled`, never `failed`: `exit_reason` names the actor and the rationale
  (the only thing that later tells a deliberate stop from a crash), no dispatch
  attempt or build strike is charged, and the work returns — a build's specs to
  `approved`, and a scout's task to **`backlog`**, the one exception to
  "picked-up work stays picked up", because leaving it `queued` has the
  dispatch loop start a replacement scout within the tick. A cancelled scout
  keeps its salvage, with the cancel's rationale stamped onto the notes'
  `reason`. The `biased` ordering in `bounded` is load-bearing: an outcome
  already in hand is never discarded for a cancel that arrived in the same
  poll, so cancelling a run that finishes in the same breath is honest rather
  than destructive.
- **`directions` tell an agent what to do; `rationale` tells a human why. They
  are never copied into each other.** A rationale explains a judgment to
  whoever reads the `decisions` ledger afterwards and reaches no VM ever; put
  an instruction there and the agent never sees it. `Directions { text, author
  }` is the other channel: it reaches a Scout or a Builder as its **own
  labelled section** of the prompt — after the field notes for a Scout, after
  the specs for a Builder, and immediately before `## Your job` in both — and
  is persisted against the *run* that carried it. It carries its **author**
  because the prompt introduces it by name, which is also what lets a Builder
  see that what it is reading is not a Scout: that is the barrier carve-out,
  and the argument for it lives in `builder::render_prompt`'s doc comment
  rather than being re-litigated. The barrier forbids *Scout-run-derived*
  material, and no path runs from a Scout run to that field. Both sections
  demand every direction be **accounted for** in the run's own artifact
  (`SPEC.md`'s `### Notes`, `SUMMARY.md`), declines included, because a
  direction silently dropped is indistinguishable from one never read — and
  both say a genuine conflict resolves in the directions' favour *but must be
  stated*, since the reviewer reads the issue or the spec and cannot see this
  section. An **undirected prompt grows no heading at all**: an always-present
  empty `## Directions` is what teaches an agent to skim past the one that
  matters. Scout directions are **staged on the task and sticky, never
  consumed** — a VM death or a `needs_revision` return would otherwise leave
  the retry unaimed with nobody noticing — which is why "absent" cannot mean
  "clear": a second `POST /scout` with no body must not unaim the run.
  `parse_directions`' doubled `Option` is that three-way distinction, and
  over the limit is a **400, not a truncation**, because an instruction cut
  off halfway is a different instruction. The run's copy is a *copy*: re-aiming
  a task tomorrow must not rewrite what a run that already happened was told.
- **A review's `feedback` is agent-facing on *both* verdicts, so an approval
  can carry required changes.** It is the third channel beside `directions` and
  `rationale`, and it belongs with the first: on `needs_revision` the re-scout
  quotes it under `## Previous attempt`, and on `approved` the Builder receives
  it as its own `## Review feedback on these specs` section — attributed per
  spec, because a batch is exactly where unattributed feedback becomes a guess
  about which spec it belongs to. Until #935 the approved half reached nothing
  at all: feedback lives on `spec_queue.feedback`, the build prompt was
  assembled from `specs` and `tasks`, and `Spec` has no such column, so every
  required item that was not *also* spec content was dropped by construction —
  which is why the dropped ones were uniformly documentation, naming and
  framing. `Builder::load_batch` reads the queue entry at **prompt time**, not
  when the batch was created, so a batch a `watch_merges` unwind sent back to
  `ready_to_build` is rebuilt under the same requirements. The section demands
  each item be accounted for in `SUMMARY.md` under a `## Review feedback`
  heading, declines included — and since the summary *is* the PR body, that
  accounting is in front of the reviewer without anything being fetched.
  `summary_accounts_for_review_feedback` is a **presence check** and is
  reported as the build's own claim — the last thing that reads agent prose,
  now that the `Verification:` trailer beside it is gone, and deliberately so:
  what it reports is a fact for a reviewer, never a gate on a write. It is a
  brief fact and **not a fourth `landing_section` carve-out**, since those three
  are about whether a change can be *verified* and a fact that reads like a veto
  is a second source of truth about who decides. `rationale` must never follow
  this path — `review_spec` takes both, and a decision record addressed to the
  ledger has no business in a VM.
- **The images are rebuilt by hand, and the gap is what has to be visible.** A
  merge does not rebuild images and should not: nothing inside the pipeline can
  reach the cross toolchain, the `container` CLI or the checkout a rebuild
  needs, and a host-exec capability would be far larger than anything in the
  charter. The failure was never that the rebuild was manual — it was that
  nobody could see it had not happened, so #888's fix sat on `main` for ten
  hours while that exact failure killed a scout inside an older image and the
  old supervisor, having no idea it was old, charged a dispatch strike for it.
  So both supervisors are stamped by `build-stamp` exactly as the server is
  (one implementation, which is the only reason the numbers are comparable),
  each states its identity on the `Started` event of its protocol — the only
  moment there is to ask, since a VM exists only while a run is inside it — and
  `crates/tasks/src/images.rs` records it per image and reports it from
  `/status`, `tasks status`, the Server window and the brief. Three rules hold
  it together. The field is `#[serde(default)]` because images are upgraded by
  hand, so the host is routinely newer than the supervisor talking to it —
  that skew *is* the bug — and **absence is the loudest reading, not the
  quietest**: `Unstamped`, never `Unknown`, because an image that reports no
  identity predates reporting one and is staler than any version it could have
  named. The **verdict is never stored**, only computed at read time against
  the running server's build, since the server is replaced far more often than
  the images are. And **nothing observed is not a clean bill of health** — no
  poll exists, so an empty list means no run has started in an image yet, and
  every renderer says "none observed yet" rather than "current". There is no
  `ObligationKind::StaleImage`, and the reason is **what a rebuild decides, not
  what the orchestrator can reach** — a capability claim would be checkable and
  wrong: `ORCHESTRATOR_WORKDIR` is routinely the checkout, `ORCHESTRATOR_CMD`
  routinely carries `--dangerously-skip-permissions`, and the `container` CLI
  and the cross toolchain are on this host, so the orchestrator can run `make
  images` today. It must not, because a rebuild is a **deployment**: it changes
  what every future run executes, with no review in front of it and no revert
  but another rebuild. That is the `build-now` category — `POST /projects`,
  `POST /projects/{id}/status`, `DELETE /builds/{id}/bundle` — all human-only
  for what they decide rather than for any lack of means. The obligation
  argument then follows rather than carrying the weight: the decision is a
  human's however the host is configured, so an obligation raised every pass
  would be undischargeable *by the party it is addressed to*, which is how a
  signal gets trained out of use. Writing it the other way round is the trap
  this document already names one section down — the first person to run `which
  container` finds the stated objection evaporated and enables the thing without
  re-deriving whether it is safe. `make images-check` covers the one window observation
  cannot — right after a rebuild, before anything has run — and `make images`
  ends by invoking it.
- **While an update is pending, new containers wait — observing a gap and
  walking into it must not be the same behaviour.** An upgrade is three acts
  (`cargo build`, `make images`, `tasks reload`) and the gaps between them are
  where work dispatches into the stale half. `crate::updates::UpdateWatch`
  holds *new* dispatch — the scout top-up and the build claim, the only two
  places a container starts — when either of the two skews observable from
  inside the process is present: a server binary at `current_exe` with an
  mtime after boot (discharge: `make restart`), or an image identity observed
  **since this process booted** whose `ImageFreshness::needs_rebuild()` holds
  against this server's stamp (discharge: `make images`, then `make restart`).
  Both halves of "observed" are load-bearing, and both are the no-wedge rule:
  **absence of evidence never holds** — the run that would observe a rebuilt
  image is the run the hold would prevent, so "none observed yet" dispatches —
  and a *pre-boot* observation is stale data, not evidence, because every
  image reads `behind` the moment a newer server starts, the record only moves
  when a run moves it, and a rebuild does not touch it; holding on it would be
  a gate only the gate itself keeps closed. The bounded cost is that after an
  upgrade one run may start in a genuinely stale image — it reports the fact
  and closes the gate behind itself. In-flight
  work runs on, queued work stays queued, nothing is charged an attempt; the
  transition is announced once per edge by the watch — in the log **and as an
  `EventPayload::Note` on the event feed**, the same shape `GitHubWatch::observe`
  uses, since a hold announced only to a terminal that has scrolled away is one
  nobody arriving later can see — while `/status`/`tasks status`
  carry the standing answer with each reason naming its own discharge. A `Note`
  and not an obligation, for the reason `ObligationKind::StaleImage` does not
  exist: discharging this one means a rebuild or a restart, and both are
  deployments a human decides — not because the orchestrator could not run
  them.
  `TASKS_UPDATE_HOLD=off` keeps the report and drops the gate; anything else
  non-`on` refuses to boot. The hold sits beside `github_hold` at the same two
  gates, ahead of the claim, for the same claim-then-refuse reason.
- **A run in flight is not a reason to refuse host work.** The three ways this
  pipeline gets upgraded were gated as though they were one act, and they are
  not. `tasks reload` re-attaches to every live VM (`resume_in_flight`), so a
  swap costs at most one `Orphaned` write-off, which charges no attempt — it
  now **reports** what is in flight and swaps, with `--when-idle` kept as the
  opt-in for someone who would rather not spend even that, and `--force`
  demoted to its *other* job (a server that is alive and will not answer
  `/status`). What `make images` can actually spoil is narrower still: a run
  **dispatched into it** starts in the old image — the #909 staleness
  `UpdateWatch` structurally cannot see, since the identity it reads comes only
  from a run that has already started — while a run that started earlier is
  simply not that case. So the rebuild is wrapped rather than gated:
  `tasks hold [--label TEXT] -- <command>` pauses dispatch, runs the command as
  **its own child**, and puts the mode back the instant that child exits —
  success, failure or signal — exiting with the child's status, so a recipe is
  unchanged by the wrapper. It waits for nothing and cancels nothing. What a
  `container build` does to an already-running container is **not established
  here and the argument deliberately does not rest on it**: if it does disturb
  one, that run dies `Transport`/`Orphaned`, charges no attempt and is
  re-dispatched, which is an outcome this already accepts. Do not "improve"
  this by asserting the container is safe — the point is that it does not need
  to be. Four things hold the hold's shape. **It is a parent process and not
  two recipe lines**: a `tasks hold` before and a `tasks resume` after would
  reintroduce exactly the failure being removed, since a `make` that dies in
  between leaves the pipeline paused with nothing left running that knows to
  undo it — which is why `images-rebuild` exists as its own target, one command
  for `hold` to be the parent of. **A SIGINT or SIGTERM of the hold itself
  restores too**, and forwards the signal on so the rebuild actually stops;
  Ctrl-C during a multi-minute rebuild is ordinary behaviour, not an edge case.
  A **SIGKILL** of it is the one case that strands a pause, named rather than
  papered over, with `tasks resume` as the undo. **The restore is gated on
  whether *this* call installed the pause** (`pause_dispatch` answers that), so
  a pipeline already `pause`d or `stop`ped is never *promoted* to `play` in the
  name of having held something — `Stop` is tighter than `Pause`. And if a
  human moves the mode while the command runs, the restore **re-reads and
  leaves it as found**: the window is the command's own duration and the human
  wins it, which is the same direction of error the pause end already refuses.
  The one honest arm of the old refusal is **inverted rather than deleted**: a
  live server that will not answer `/status` used to refuse, because "quiesced"
  about a server you cannot see into is the wrong direction to be wrong in —
  right while the promise was quiescence, and not the promise now. It runs the
  command unheld and says so, the cost being at most one run starting in the
  old image (which reports itself through `ImageFreshness` the moment it does)
  against a rebuild that does not happen at all. `stop --when-idle`'s pause
  turns out **never to have been a debt**: `apply_startup_mode` overwrites the
  stored mode from `TASKS_DEFAULT_MODE` before the next listener binds, and
  between the SIGTERM and that boot nothing reads the column — so the fix there
  was the sentence, not the behaviour. It still cannot be put back *before* the
  SIGTERM (that hands the dispatcher a window for one last scout) and nothing
  in `reload.rs` may open the store to do it after. `tasks drain` / `tasks
  resume` stay, narrowed to the one host act with **no** recovery: restarting
  vm-pool on the same socket, where the successor stops its predecessor's
  containers off the orphan ledger. Mode `pause` is still the hold — no fourth
  thing to keep in step beside `github_hold` and `update_hold` — and
  `drain --check` survives **demoted from a gate to a diagnostic**, with
  nothing in the repo refusing on it. `tasks hold` is deliberately general
  (`-- <any command>`) rather than an `images`-shaped flag: the same shape is
  the honest answer for any future host act that can only be spoiled by a
  *new* dispatch. If a seventh server-side dispatch hold is ever wanted (a real
  `maintenance_hold` with a TTL beside `github_hold`/`update_hold`), **this is
  the change to revisit** — it would remove the mode juggling entirely, and it
  was rejected here for the reason above: mode `pause` *is* the hold, and a
  parallel one is a fourth thing to keep in step.

- **A diagnostic reports and never fixes, and the one check worth writing is
  the one every other check is blind to.** `tasks doctor` asks every
  precondition for a scout at once — the container CLI, vm-pool's socket and
  its *two* ledgers, the images, custody, the broker, GitHub, the
  orchestrator's surroundings — in the order the preconditions bite, because a
  missing container CLI explains the vm-pool failure below it which explains
  the dispatch failure below that; **do not sort by severity**, a reader who
  sees the first cause first does not have to work out which of six complaints
  is the root. There is no `--fix` flag and there should not be one: a
  diagnostic that changes state cannot be run when you are unsure, and the fix
  it would most want to perform (`make images`) cannot be reached from inside
  this pipeline at all. Four rules hold its shape. **The fix is a required
  parameter, not a convention** — `Check::fail` and `Check::warn` take it by
  value, because every earlier version of "name the fix beside the complaint"
  here (`make check-toolchain`, `ImageFreshness`, the update-hold reasons) does
  it by convention and a convention is what the next check quietly skips;
  `Check::note` is the *named* escape hatch for the two warnings with genuinely
  no command, so "there is nothing to run" cannot be mistaken for "somebody
  forgot". **It never opens the store**, because `Store::open` migrates and a
  diagnostic that moved the schema is worse than none — which is why mode,
  projects and the observed image identities come from the running server's
  API, and why a host with no server reports "not serving" rather than reaching
  past it. **A `Skip` never sets the exit code**: every skip has a failure above
  it that caused it, and a skip that failed too would report one broken thing as
  two. And the single write — one uniquely-named file under the data dir — is
  the write probe, because writability is only answerable by writing (mode bits
  lie under ACLs, a read-only mount, a full disk). The **broker check** is the
  one that justifies the command: on 2026-08-19 every host-side signal on this
  machine was green — pool healthy with slack, socket live, images present,
  token valid, server serving — and no scout could have run, because the macOS
  application firewall was severing the broker's non-loopback listener after a
  `cargo clean` removed the binary its verdict was attached to. So the probe
  goes to **`TASKS_BROKER_ADVERTISE:TASKS_BROKER_PORT`, never loopback**, and
  **an unauthenticated 401 is the success condition** — during that outage
  loopback answered a correct `a lease is required` while the bridge gateway
  accepted the connection and returned zero bytes, so a `127.0.0.1` probe reads
  as a pass at exactly the moment the thing is broken. Anything that fails
  *after* the connect is `Silent` and a `Fail`, never `Unreachable`: the connect
  already proved the address is reachable, and demoting that to a `Skip` sets no
  exit code, which is the false negative the check exists to prevent. A gateway
  that cannot be reached at all *is* a `Skip`, because apple/container's bridge
  does not exist until the first container has started — a cold machine has not
  been shown to be broken. Finally, severity is **read, never re-decided**:
  `Capacity::level`/`describe`/`fix` are what both the connect-time log line and
  the checklist use, and doctor reads `ImageFreshness::needs_rebuild` rather
  than judging freshness a second time — two hand-written versions of one
  question is exactly how two readers come to disagree.

## Project structure

- `crates/tasks/` — the server binary: SQLite store, event log, GitHub
  polling (read-only intake), scout dispatcher, HTTP API + SSE
- `crates/tasks-api/` — wire types for the HTTP API (models, events,
  request/response bodies), shared by the server and native clients.
  Dependency-light (serde/chrono/uuid) on purpose; enums are strict —
  clients ship from this repo, so skew is a build error, not a runtime
  fallback
- `crates/tasks-protocol/` — ScoutCommand/ScoutEvent, the `AppProtocol` impl
  shared between server and Scout VMs
- `crates/build-stamp/` — `build.rs` helper that stamps a build identity
  (`0.1.<commit count>` + short SHA, env-overridable) into a binary. Used by
  the server, `tasks-client` and `app-gpui`; one implementation on purpose,
  since `GET /version` compares those numbers across processes
- `crates/scout-supervisor/` — PID 1 inside Scout VMs: clone, branch, run the
  agent, report the spec back
- `crates/vm-pool/` — vendored VM infrastructure (protocol, pool, service,
  client, supervisor). Has its own CLAUDE.md and TODO.md; conventions there
  apply within it (notably: no mocks, real processes in tests)
- `images/` — container image definitions. `base`, `agent` and `automation`
  are vm-pool's own (it stays independently publishable); `scout` and
  `builder` are Tasks'. A tool a Tasks crate needs goes in the latter two,
  duplicated, rather than once in `agent` — app vocabulary does not enter
  vm-pool's images any more than it enters its code
- `site/` — the landing page published at `nate.rip/tasks/` (#995): one
  hand-written `index.html`, one stylesheet, three screenshots. It lives here
  rather than in `docs/` because a `/docs` Pages source would publish the
  design docs too, and in this repo rather than its own so the README, CLAUDE.md
  and the diagram it must stay in step with are one checkout away. **No build
  step**, and that is more than register: `.gitignore`'s `dist/` and
  `node_modules/` patterns are *unanchored*, so a generator emitting into
  `site/dist/` would commit nothing and fail silently. `make site-check` is the
  whole publish gate (`.github/workflows/pages.yml` runs the same line); the
  disclaimer-drift half of it is *also* a workspace test, deliberately, because
  the script only runs at deploy time and this pipeline merges its own PRs
- `docs/plans/` — implementation plans; `docs/vm-pool.md` — vm-pool spec
- `spec` for the platform: issue #744 + docs/plans/2026-08-09-v2-resume.md

## Conventions

- Tests use real processes and real SQLite (in-memory or tempfile). No mocks.
  HTTP tests bind real servers on `127.0.0.1:0`.
- **Tests exec binaries; they never build them.** A `cargo build` inside a
  test blocks on the build-directory lock, so a background `cargo check`
  (rust-analyzer, another terminal) stalls the whole suite. For a binary in
  the test's own package use `env!("CARGO_BIN_EXE_<name>")`; for one from
  another package use `common::workspace_bin(name)` in `crates/tasks/tests`,
  which reads `TASKS_TEST_BIN_DIR` (exported by `make test`) and only builds
  as a memoized fallback. vm-pool has its own copy of this —
  `vm_pool_test_support::supervisor_binary()`, reading `VM_POOL_TEST_BIN_DIR`
  — deliberately, so vendored infrastructure stays independently testable;
  don't merge the two.
- **A new migration is named for a UTC instant, never for the next free
  number.** `make migration NAME=build_transcripts` writes
  `crates/tasks/migrations/20260815030411_build_transcripts.sql`
  (`YYYYMMDDHHMMSS`, UTC, **digits only**) — don't hand-roll one by copying
  the file next to it and adding one to the number. That number is read off a
  tree that cannot see its sibling branches, so two of them pick `0024`, and
  the collision exists only after the merge, where it surfaces as a boot
  failure in a process that has already taken the port. Three facts make the
  switch additive: 0001–0023 keep their versions and checksums (sqlx records
  both, so an applied migration can never be renamed), a 14-digit stamp sorts
  after any four-digit sequence number, and sqlx parses the text before the
  first `_` as an `i64` — so `20260815T030411_…` is a hard compile error, and
  a name it cannot split at all is silently skipped and simply never runs.
  `crates/tasks/src/migrations.rs` owns `MIGRATOR`, documents the rule, and
  holds the guard tests that make a violation red in your branch.
- Errors: `thiserror` enums per module. Logging: `tracing`.
- Rust edition 2024, `cargo fmt` + `cargo clippy --workspace --all-targets`
  clean before committing.

## Running

```sh
make serve                             # build, take over, log to this terminal
make restart                           # build, take over, background it
make restart RELOAD=--when-idle        # ...but wait out in-flight scouts first
make status / make stop
make stop STOP=--when-idle             # ...but wait out in-flight scouts first
make drain                             # quiesce the pipeline and HOLD it, for
                                       #   the one host act with no recovery
                                       #   (restarting vm-pool on the same
                                       #   socket); undo with make resume
make drain DRAIN=--cancel-scouts       # ...stopping running scouts rather than
                                       #   waiting them out
make resume                            # release that hold
cargo run -p tasks -- add-project owner/repo
tasks secrets init                     # create the sealed credential store
tasks secrets set github-token         # seal a key (value on stdin, never argv);
                                       #   a running server picks it up live
tasks auth login                       # or skip minting a PAT: GitHub device
                                       #   flow — shows a code to enter at
                                       #   github.com/login/device and seals
                                       #   the token as github-token (#1002)
make dist                              # Tasks.app with a release `tasks` seed
                                       #   at Contents/Helpers/tasks; first
                                       #   launch installs the service from it
                                       #   — installs to ~/Applications, NOT
                                       #   to dist/
make release SIGN_IDENTITY='Developer ID Application: … (TEAM)'
                                       # the download: signed, notarized,
                                       #   stapled .app + .dmg + CLI zip,
                                       #   staged in dist/. Refuses without an
                                       #   identity rather than shipping
                                       #   unsigned; macOS + Apple Developer
                                       #   enrollment only
make release-clean                     # rm -rf dist/ — the release staging
                                       #   directory only, never what `make
                                       #   dist` installed
make publish HEADLINE="…"              # cut a release: refuse unless HEAD is a
                                       #   green origin/main, generate the
                                       #   CHANGELOG section, run `make
                                       #   release`, then tag, push both refs
                                       #   atomically, upload and re-download
                                       #   the two assets. Human-only, and
                                       #   there is no API route
bash scripts/changelog.sh <from> <to>  # that section on its own, to stdout;
                                       #   `--next-version` is the one place
                                       #   0.1.<count + 1> is written
tasks service install                  # THIS binary -> ~/.tasks/bin, one
                                       #   LaunchAgent (login + crash restart);
                                       #   idempotent, and also the upgrade
tasks service status                   # agent / binary / launchd / serving
tasks doctor                           # every precondition for a scout as one
                                       #   checklist, each failure naming its
                                       #   fix; 0 clean, 1 on a failure (or on
                                       #   any warning under --strict), 2 usage
tasks doctor --probe-images            # ...and boot each image to read its
                                       #   --version, as `make images-check` does
make migration NAME=lower_snake_case   # new migration, stamped with the UTC now
make images                            # rebuild the Scout/Builder VM images
                                       #   (wrapped in `tasks hold`, which
                                       #   pauses dispatch for exactly as long
                                       #   as the rebuild runs)
tasks hold [--label T] -- CMD          # that wrapper on its own: pause, run
                                       #   CMD as a child, restore on its exit
make images-check                      # boot each image, read `--version` back
make site-check                        # the landing page's publish gate: the
                                       #   disclaimer matches the README's and
                                       #   every relative link resolves
make verify-warm                       # prime the orchestrator's build directory
sh .tasks/verify                       # what a Builder VM runs before it packages
                                       #   anything — a red run fails the build
                                       #   inside the VM and opens no PR
make test                              # see Tests below
```

`make images` is the whole deployment step for anything inside a VM — a
supervisor fix reaches nothing until someone runs it on a Mac with
apple/container and the cross toolchain. It asks nobody's permission: the
rebuild runs inside `tasks hold`, which pauses dispatch for exactly as long as
the rebuild takes and restores the mode when it exits, however it exits. A
scout dispatched into the middle of a rebuild would start in the *old* image,
and that — not a run already in flight — is what the hold prevents. `images-check` (which `images` ends by
invoking) is the only reading available in the window between a rebuild and the
first run in the new image; everywhere else, the identity is observed from the
runs themselves. Until the images are rebuilt, `unstamped` / "PREDATES
STAMPING" in the app and `tasks status` is the correct answer, not a bug —
it is the feature reporting the state #909 was filed about.

`serve` runs the Diamond 1 loop (`crates/tasks/src/run.rs`): GitHub intake,
scout dispatch bounded by `SCOUT_MAX_CONCURRENT`, and the HTTP API. Mode gates
*new* work only — `Pause`/`Stop` never interrupt a scout already in flight.
Both dependencies degrade rather than crash: no `GITHUB_TOKEN` disables
polling, an unreachable vm-pool disables dispatch and reconnects periodically,
a GitHub that stops answering *holds* dispatch until it does (see *A GitHub
outage holds dispatch* below), and the API stays up either way.

**A boot does not resume the mode; it takes `TASKS_DEFAULT_MODE` (default
`pause`) and overwrites the stored one** (`apply_startup_mode`, before the
listener binds). Starting a server is therefore never the same act as resuming
dispatch: a crash, a `launchd` `KeepAlive` or an infrastructure problem brings
the pipeline back quiet, and `pause` rather than `stop` keeps intake and the
API alive while it is. The one exception is a deliberate upgrade — see below.
If you want a host to come back dispatching, the honest way to say so is
`TASKS_DEFAULT_MODE=play` on that host, not re-reading the column. The column
itself stays: it is still the live mode for the rest of the process's life
(`GET /mode`, `POST /mode`, and the three loops that read it every tick), it
has only stopped being consulted at boot — **do not delete it** without first
moving mode into process memory. Two details are load-bearing and cheap to
"clean up" by accident: the boot breadcrumb is a `Note` and not `ModeChanged`,
because `ModeChanged` is nudge-worthy and would spend an orchestrator turn on
every restart; and the transition happens *before* `server::bind`, so no
client — and no `reload` verifying a swap — can observe the previous run's
mode.

### Budgets and a host that sleeps

**Every run budget is measured on two clocks, and the gap between them at
expiry is the suspend.** A `tokio::time::sleep(budget)` is `Instant`-based and
an `Instant` does not advance while the machine is asleep, so on the laptop this
pipeline runs on a build dispatched at 03:44 against a lid closed from 04:22 and
opened at 12:34 fired three and a half minutes later as `build timed out after
3600s` — true in monotonic terms and wrong in every term a human uses. It held
the serial build lane for nearly nine hours and charged three specs a build
attempt each for a closed lid (#929), which the strike rule above forbids: a
`Timeout` is charged precisely because it "had the entire budget", and this one
had 38 minutes of it. `crate::deadline::Deadline` anchors on **both** a
monotonic and a wall-clock reading and expires on whichever runs out first;
because both anchors are kept, the time the host was not running is a
**measured fact rather than an inference**, and it gets its own error variant
(`{Scout,Builder,Orchestrator}Error::Suspended`), its own `exit_reason` sentence
and `FailureClass::Transport`.

**Two thresholds, because "was the host asleep" and "was the run given its
budget" are different questions.** One constant answering both is #944: the
deadline fires on `max(wall, awake)`, so a run whose host napped *at all* can
never reach its budget awake, and a single 61-second nap anywhere inside an hour
waived the strike for a run that spent 59 of its 60 minutes working.
`WAKE_KILL_FLOOR` (5 minutes) is **availability** and gates the wall-clock arm of
the expiry itself — below it the wall arm is disarmed and the run drains its
monotonic budget as it did before the module existed, which is right because the
in-VM supervisor already re-invokes an agent whose connection dropped
(`{SCOUT,BUILDER}_MAX_RESUMES`, the failure a short nap causes) and killing the
run throws that recovery away. It is cumulative, so three four-minute naps trip
it. And what it bounds is a run's **awake execution past the point wall elapsed
reached the budget, never wall-clock elapsed** (#955). The sentence that stood
here claimed the latter, and its reason — "the wall arm is disarmed below it" —
was right about the regime it named and was then generalised past it: while the
arm is disarmed the whole suspend is under the floor, so the wall overshoot is
under it too, but a single nap at or past the floor *arms* the arm, and that nap
is itself the overshoot. So wall-clock elapsed has no bound and costs nothing —
`Expiry::remaining` answers `None` once `awake` reaches the budget whatever the
suspend is, and nothing caps a suspend, so a lid closed for three hours during a
disarmed run's last tick fires three hours past the wall budget and the run was
not running for any of it. Awake execution *is* bounded, strictly **under** the
floor and not by the floor plus a tick, which double-counts: neither arm of
`remaining` ever answers with more than the monotonic remainder (`elapsed >=
awake`) and the poll sleeps `remaining.min(tick)`, so `awake` never passes
`budget` and at most the suspend accumulated at that point is left to spend —
under the floor if the arm is disarmed there, and at most one tick if it is
armed, a tick being *less* than the floor rather than an addend to it. The tick
is not a term in that bound at all; the question it does answer is the one in
the paragraph below. `WAIVED_BUDGET_SHARE` (a quarter) is **accountability**, read as how much
of the budget went *unspent awake* (`budget − awake`) — a fraction and not a flat
ten minutes because every budget it reads against is configurable and shorter
ones (the reattach remainder, floored at 30s) would make a flat floor
unreachable. `Expiry::starved_by_suspend` is that predicate, and it — not "did
the host sleep" — is what picks `Suspended` over `Timeout` at all three
consumers. It needs no second "was that really a suspend" test: an expiry only
happens with `elapsed >= budget`, so `unspent <= suspended` always, and two
clocks read microseconds apart leave nothing unspent. The split creates a
**middle state** with its own sentence: a long nap arriving late is killed at the
wake *and* charged, because a run that had fifty minutes of its hour and produced
nothing is a verdict — the same twenty minutes arriving early leaves forty and is
waived.

Four more things are load-bearing. The **monotonic reading is the floor**
(`wall.elapsed().unwrap_or(awake).max(awake)`), and the two directions of a clock
adjustment are **not** symmetric: a step *backwards* is fully neutralised and
degrades to the monotonic behaviour that shipped before, while a step *forwards*
is taken by `max()` and adds elapsed time the run never had, so a large enough
one retires a run early and reports a suspend that never happened. Nothing can
tell that from a lid, because the measurement *is* the disagreement between the
clocks; that is accepted rather than solved, and what bounds the bill is
`WAKE_KILL_FLOOR` — a forward step under it costs nothing at all. The deadline
**polls** on a 30s tick rather than sleeping the remainder,
because that is what makes it fire on the *wake* instead of once the leftover
monotonic budget finally drains; the tick must stay well under any budget anyone
would configure, since it bounds how long after a wake a doomed run stays parked
holding the serial lane. `Timeout` keeps reporting the **configured** `secs` and
not the expiry's, because a resumed run's effective budget is the remainder and
the integration tests pin specific numbers — and the suspend sentence must never
contain "timed out", or the distinction goes straight back. And a suspended run
is **killed at the wake, not extended**: no agent's API connection survives an
eight-hour suspend, so handing the budget back would only hold the lane longer
for a run that is already dead. `caffeinate -s` stays the operational answer;
this makes a sleeping host legible and free, not harmless.

### Pool capacity

There are **two ledgers, and they are not the same one**. The *slot* ledger is
`VM_POOL_MAX_VMS` (default 6): a slot is a VM the pool allocated, and this
server asks for `SCOUT_MAX_CONCURRENT` of them for scouts plus exactly **one**
for the serial build lane — nothing multiplies that one, because builds are
strictly serial. `buildkit` is **not** on this ledger: the container runtime
starts it to service `container build`, as an ordinary host process the pool
never allocated and never counts. The *memory* ledger is the one that bites a
small machine first, and buildkit is on it: at the default VM shapes, scouts
(`SCOUT_MAX_CONCURRENT` × `SCOUT_VM_MEMORY_MB`) plus a Builder plus buildkit
reserve ≈22 GB.

So the recommended ceiling against the default pool is **`SCOUT_MAX_CONCURRENT
= 3`** — 4 of 6 slots, two spare. 4 scouts is 5 of 6 and 5 is 6 of 6, where a
single leaked VM (one whose owner died between allocate and deallocate, held
until its own event stream ends) exhausts the pool and dispatch waits (see the
capacity-hold rule above: it is a wait rather than a refusal, and it costs no
attempt — but nothing dispatches for as long as the leak lasts). To go higher, raise `VM_POOL_MAX_VMS`, restart the *pool*, and check
the memory ledger first.

**Two leaks eat slots, and they need two mechanisms — neither of which is the
sweep the docs used to point at.** `run::sweep_leaked_vms` asks a *store*
question ("what does my database still point at for work that has concluded"),
and it is single-shot: the row is cleared whether or not the deallocate landed.
It cannot see either leak below, and all three stay. A **slot leak** is a VM
that died while the pool still counted it — the pool used to go on counting it
until `vm_timeout`, two hours — and is now reclaimed *event-driven*, at the
instant of death, from the end of that VM's event stream. Its owner's
`deallocate` still succeeds and still runs the **full** teardown, because a
supervisor that died inside a container that is still running looks identical
from there; the acknowledgement is consumed, so a second `deallocate` is
`VmNotFound` again and `VmNotFound` keeps its one honest meaning. An **orphan
leak** is a VM whose whole daemon went away (`container run` outlives the
process that spawned it), and it is stopped by the *next* daemon on that
socket, from a write-ahead `VmLedger` under `ServiceConfig::state_dir` (the
data dir, deliberately not `/tmp`, which a reboot may clear) keyed by socket
path. The ledger is read and discharged strictly **between the bind and the
accept loop** — never at construction, where a second pool started against a
live one would kill that pool's in-flight scouts and Builder and then exit on
`AlreadyRunning`. Two limits are stated rather than implied, and only
one of them has moved. A stop that is **refused** is now retried across boots:
`VmRuntime::stop` answers a question — `Ok(())` claims the VM is not running,
anything else declines to claim it — and both `reclaim_carried_over` and
`deallocate` forget an id only on the claim, so an unconfirmed stop is carried
to the next daemon and asked again (#950; before it, `ContainerRuntime::stop`
returned `Ok(())` whether or not `container stop` worked, so the `Err` branch
was unreachable in production and recovery was single-shot). A stop that
**succeeds** is still the CLI's word: nothing verifies the container died, so
the honest sentence there remains "the successor asked the runtime to stop it".
The cost of the retry is one stop and one warn per stuck id per boot, forever,
which is the behaviour rather than a leak — the alternative is dropping the
only record that a VM exists. What *is* recoverable on every runtime, untouched
by any of this, is an **interrupted** reclaim, because the ledger seeds its
in-memory set at read time and persists the remainder after each stop — so a
daemon that dies partway through hands the rest to the next one.

`VM_POOL_MAX_VMS` is read by **`tasks vm-pool`, not by the server** — both
entry points honour it (`max_vms_from_env` is public and separate from
`ServiceConfig::from_env` for exactly that reason), but a pool is sized when it
starts, so changing the variable means restarting the pool and not the server.
A value that is not a positive integer refuses to start rather than falling
back: `0` binds the socket, answers `status` cheerfully and fails *every*
allocate, which is precisely the failure the knob exists to make configurable.
What the server does is **report** the arithmetic on every vm-pool connect, off
the `status` round trip the connect path already makes (`run::Capacity`): too
small, or an exact fit with no slack, is a `warn!` naming the variable and the
fix. A report and not a gate — nothing here can resize a pool in another
process, and refusing to dispatch would turn a survivable misconfiguration into
an outage.

### Upgrading a running server

`tasks reload` (alias `restart`, `crates/tasks/src/reload.rs`) is the upgrade
loop the make targets drive: **build, report, gate, drain, swap, verify**, in
that order. A failed build costs nothing because nothing has been signalled
yet; "did it come up?" and "did the schema move?" are answered by `GET /status`
on the *new* pid rather than assumed. It refuses by default when a scout or a
build is in flight (`--when-idle` waits for a drain point and pauses dispatch
for the wait, `--force` swaps anyway); an owed orchestrator turn is reported
but never blocks, since the obligation loop keeps producing input and the
answered watermark means a restart mid-turn only costs one turn. Exit codes: 3
busy, 4 drain timed out, 5 the swap did not land.

**An upgrade is the one path that carries the mode over, and it carries it in
the child's environment.** `ModeHandover` snapshots the running server's mode
*before* the drain (the pause `--when-idle` installs is the tool, not the
intent), passes it to the new process as `TASKS_DEFAULT_MODE` — for the spawned
and the `--foreground`-exec'd server alike — and then verifies against the new
pid's `/status` that it came up in it, reporting the `curl` that fixes it if
not. It is not a `POST /mode` after the boot, for three independent reasons: a
POST leaves a window in which the new server runs in its configured default
(with `TASKS_DEFAULT_MODE=play` against a paused old server, that window
*dispatches*), `--foreground` execs so there is no "later" to restore anything
in, and the real environment outranks every `.env`. A cold start, and a
`--force` swap of a server too wedged to answer `/status`, carry nothing —
unknown resolves to quiet, never to dispatching. `reload` resolves
`TASKS_DEFAULT_MODE` as step **0**, before the build: an unusable value is a
hard `serve` startup error, and discovering that after the SIGTERM turns a typo
into an outage.

**`tasks stop --when-idle` waits on the same predicate, and differs in exactly
one lasting way.** Idle is `InFlight::is_destructible()` and there is one of it,
so Restart When Idle and Stop When Idle cannot disagree about what they are
waiting for; `--drain-timeout SECS` and exit codes 3 and 4 mean what they mean
in `reload` (5 has no meaning for a stop and is never returned). What differs is
the mode: **a stop leaves dispatch paused**, and says so in the help, in the
drain output, on the way out and in the app. Not taste — the only slot in which
a stop could write the mode back is *before* the SIGTERM, and unpausing a server
that is still running hands the dispatcher a window to launch one last scout,
which is precisely the unattended VM the flag exists to prevent (nothing here
may open the store to do it afterwards, for the reason below, and after the
SIGTERM there is no server to `POST /mode`). The drain *timeout* still restores
the mode, because nothing was stopped and a no-op must not have side effects; an
idle server is never paused at all, for the same reason. `ModeAfterDrain` is the
single place that asymmetry is written down, so a third caller of `drain` has to
answer the question rather than inherit an answer. Plain `tasks stop` is
unchanged — immediate and ungated, because it is the counterpart of
`reload --force`, the thing `make stop` and the reload path already rely on, and
the documented way through both new refusals. The GUI is where the missing
question lives: an immediate **Stop** with work in flight raises a three-way
prompt (wait / stop anyway / cancel) off the Server window's existing `/status`
poll — up to 5s stale, so it is a courtesy and never a lock.

Nothing in `reload` opens the store — `Store::open` runs migrations, so a
supervisor that opened the database would apply the new schema before the new
binary booted, masking the failure it exists to catch. `<data dir>/tasks.pid`
is a discovery record, not a lock: liveness is re-derived from the OS
(`ps`, where a `Z` state is dead), so a killed server leaves nothing to clean
up by hand. This is not a service manager — no supervision, no
restart-on-crash; point `launchd`/`systemd` at `tasks serve` if you want one.

### Restarts and work in flight

**A restart does not cost the work in flight.** Scouts and builds run under
their own supervisors inside VMs that vm-pool (a separate daemon) keeps alive,
so the only thing a restart loses is the event stream. Boot is `resume_in_flight`
— attach to every still-`running` session/build that names a live VM
(`ServiceCommand::Attach`, bounded replay, see `crates/tasks/src/reattach.rs`)
— and only then `reconcile_startup`, which writes off what is genuinely gone.
A reattach *always concludes its row*, including when it cannot resume;
reconciliation skips rows it owns, so one that returned leaving a row `running`
would strand it. The orchestrator's turn is a local child and cannot be
reattached: shutdown waits it out instead, and an interrupted one is reported
in the feed at the next boot. Shutdown holds the HTTP port through the whole
drain (so a restart is a hand-over, not an outage) and releases it last, which
means a successor waits for this process to exit before it can bind.

**Which is why the last thing a shutdown waits on is bounded too: the clients.**
A graceful shutdown that waits for open connections never returns here, because
half this API is streams — `/events/stream`, `/orchestrator/stream` and the two
transcript tails end when the *client* hangs up, and the app holds them for as
long as it is open. So "wait for connections to close" reads as "wait for the
user to quit the app", and the observed behaviour was a **75s SIGKILL on every
single restart**, with `drain complete` in the log a millisecond after the
SIGTERM. The 75s is not merely the cost: `reload`'s grace is 75s and the drain's
own budget is 70s, so a process that always burns the full grace leaves 5s of
margin, and a drain that genuinely needs its budget is severed mid-teardown by
the kill that should never have been reached — plus the pidfile cleanup after
`serve_on` that a SIGKILL skips. `CONNECTION_GRACE` (2s, the remainder of that
same arithmetic) is the exit condition, not a tuning knob. It lives in
`server::serve_on` rather than in the four stream handlers deliberately: a
handler taught to stop is one endpoint's fix and the next long-lived route
brings the hang back, whereas the property *a shutdown terminates* holds for
routes nobody has written yet. Severing a tail is safe in the way waiting is
not — an SSE client resumes from `?since=`, so it costs a reconnect to a
successor that is already binding. The `biased` select is load-bearing for the
same reason it is in `cancel::bounded`: a server that closed its connections
inside the grace reports its own result, so a genuine accept-loop error is
never overwritten with the timer arm's `Ok(())`.

**The drain is bounded at every stage, and it names whatever it walked away
from.** `poll`, `nudge` and `obligations` used to be awaited *unbounded* and
unnamed, while scouts/builds and the orchestrator turn already had
`SHUTDOWN_GRACE` leashes. That asymmetry is what turned a one-line bug (#883:
`orchestrator_nudge_loop` never observing the shutdown flag, because
`watch::Receiver::changed()` marks a value seen when it *returns*, so a
shutdown consumed by an inner `select!` parks the outer loop forever) into a
75s SIGKILL with nothing in `serve.log` — and cost a scout run to diagnose.
`run::drain_background` now awaits those three under one shared
`BACKGROUND_GRACE` (10s) deadline and returns the names that did not finish,
one `warn!` each. Shared, not per-task, so the whole drain is **10 + 30 + 30 =
70s and still fits inside the 75s** `reload` allows before it SIGKILLs; and
loops after the deadline are still asked, so the log names the loop that is
stuck rather than whichever handle happened to be awaited first. Abandoning
these three is safe *by construction*, which is why they and not scouts get the
short leash: a poll is idempotent, obligations are recomputed from state every
pass, and a nudge is a latency optimization the answered watermark makes good.
The `warn!` wording is deliberate — these loops return on a flag, so there is
no legitimate reason for one to still be running at 10s, and a reader who takes
the message for ordinary shutdown noise is the failure mode that put #883 in
front of a scout.

**vm-pool is upgraded separately, and it goes first.** It is a long-lived
daemon that a server restart does not restart, so a freshly built server
routinely talks to the binary vm-pool was started with. `serde(default)` makes
an added *field* survive that skew (`seq` on `vm_app`, `protocol_version` on
`pool_status`); it cannot make an added *command* survive it, because an old
service rejects the whole line at decode time and the client sees an ordinary
`ClientError::Service` — indistinguishable, without matching serde's message
text, from a real failure to attach to a VM that does exist. So `attach` is
gated: `reattach::attach_support` asks `status` once per boot and reads the
`protocol_version` it reports (absent ⇒ `PRE_VERSIONING`), and
`resume_in_flight` returns empty **before claiming any row** if the answer is
too old, unanswerable, or unreachable — `ResumedWork` membership is a promise
to conclude the row, so a claim made against a pool that cannot decode `attach`
would fail the run rather than lose the stream. The remaining cost is the
one-time restart itself, and it is what `tasks drain` is for: whatever vm-pool
is holding is lost once (the event log is in memory), and the VMs it leaves
running are stopped by the pool that takes the socket next, off the ledger the
old one wrote — that is the orphan
half of *Pool capacity* above, and it is why the restarted pool is the thing
that cleans up rather than the server's own sweep, which never sees them.
`dispatch_loop` logs the skew on every connect, because the bill
otherwise only arrives at the next restart, by which point the work it costs is
already in flight.

### Tests

```sh
make test        # prebuild + cargo-nextest (default profile) + doctests
make test-ci     # same, --profile ci: no fail-fast, retries, quieter slow threshold
make test-cargo  # plain `cargo test --workspace`, no prerequisites
```

`make test` needs `cargo install cargo-nextest --locked`; `make test-cargo` is
the fallback if you don't have it, and is also what keeps the build-on-demand
path in `workspace_bin` honest. Both nextest targets prebuild the supervisor
binaries and export `TASKS_TEST_BIN_DIR` so no test shells out to cargo.

Two gotchas worth knowing. **nextest does not run doctests** — silently, with
no skip count in its summary — so both targets end with `cargo test --doc
--workspace`; anything else that runs the suite must too. And a handful of tests
leave a stray child holding the output pipe, so they report as LEAK; that is
expected (`leak-timeout` is `result = "pass"`), and the profile deliberately
keeps the period short rather than waiting the leak out, which would cost
seconds and hide a real leak. **The known set is listed in
`.config/nextest.toml`, beside the setting it justifies, and is not restated
here** — this sentence used to say "the scout timeout tests (three of them)"
while that file said two and the suite reported seven, and at that spread a new
leak is indistinguishable from the undocumented ones (#969). One list, in one
place, naming the tests rather than counting them, and saying why they leak so
that a test leaking for a different reason reads as new. Tuning lives in the
same file.

**`app-gpui` is not a workspace member, so none of the above touches it — and
it *can* be compiled and tested from a Linux agent VM**, which was long
assumed otherwise:

```sh
make app-check   # cargo check --all-targets, ~1 minute cold
make app-test    # the app's own unit tests
cd app-gpui && cargo test   # the same thing, when the deps below are present
```

Neither needs a display or a Mac. The build wants five packages —
`pkg-config libfontconfig-dev libxkbcommon-dev libxkbcommon-x11-dev
libxcb1-dev` — which `images/{scout,builder}/Dockerfile` install, so in a VM
off a current image all three commands above are plain cargo. Where they are
absent the make targets fall back to what they always did:
`RUST_FONTCONFIG_DLOPEN=1` makes `yeslogic-fontconfig-sys` skip its
`pkg_config` probe, and linking the *test* binary is satisfied by three empty
stub `.so`s (`-lxcb`, `-lxkbcommon`, `-lxkbcommon-x11`) that `app-stubs`
generates — the tests are pure functions over view state and never enter the
platform layer. The Makefile picks between the two with one `pkg-config
--exists`, because the fallback must not stay the default once the packages
exist: `-L` beats the system paths, so the empty stubs would shadow the real
libraries, and `RUSTFLAGS` is part of cargo's fingerprint, so a stubbed
`make app-test` and a hand-run `cargo test` would each rebuild the whole gpui
tree over the other.

The boundary is compile and test, yes; **run, no**. A green `make app-test`
says the code compiles and its logic holds, not that a pixel landed anywhere —
the title bar next to the traffic lights, icon-only rows at 240px and whether
a menu item is actually greyed are still `make app` on a Mac. But "the GUI
can't be compiled here" was costing every app-gpui change its feedback loop,
and it was not true.

Data dir: `~/.local/state/tasks-v2/` (override: `TASKS_DATA_DIR`).

**Config is read from `.env`, not just from the environment**
(`crates/tasks/src/env_file.rs`). Three files are tried, in this order, and
the first to define a variable wins — with the real environment outranking all
of them, so `GITHUB_TOKEN=… tasks serve` still overrides:

1. `<data dir>/.env` — launcher-independent, and the only one an installed
   binary outside a checkout can have
2. the nearest `.env` at or above the **cwd** — a developer's `make serve`
3. the nearest `.env` at or above the **executable** — the same repo file,
   found when the cwd is `/` because launchd started the app

The third one is not redundant. Configuration used to come from the process
environment alone, which meant it only ever applied to a server started from a
shell that had exported it: restarting from the app's Server menu — whose
ancestor is launchd — silently reverted `GITHUB_TOKEN`, `ORCHESTRATOR_CMD` and
`ORCHESTRATOR_WORKDIR` to their defaults, and the server came up healthy and
wrong. Loading happens once in `main`, before subcommand dispatch (so `serve`,
`reload` and `status` cannot disagree about `TASKS_DATA_DIR`) and before the
tokio runtime exists (`set_var` is unsafe once threads are running). It is
never done inside `Config::from_env` — tests build configs, and a suite that
read the developer's untracked `.env` would be the worse bug.

**`Command::env_remove` is the opposite of a scrub**, and a test that execs the
`tasks` binary has to know it. The real environment is the only thing a `.env`
entry loses to, so *removing* a variable from a child's environment is exactly
what promotes the file that defines it — and `.env` is gitignored, so a
maintainer with `TASKS_DEFAULT_MODE=play` in one fails a restart suite on their
machine and nowhere else. `TASKS_ENV_FILES=off` is the switch for that:
`crates/tasks/tests/reload.rs` and `crates/tasks/tests/cli.rs` are the only
files that exec the binary (so they are the whole blast radius), and any future
one needs the same settings. The
test that pins it carries a **control** — it first boots with the switch off
and asserts the `.env` really does decide the mode, then boots with it and
asserts it does not. Without that half the assertion is vacuous. A value that is
neither `on` nor `off` (including one that is not UTF-8) refuses to start rather
than being ignored: `.ok()` there would mean "load the files anyway", the one
direction this switch must not fail in.

The matching rule for the orchestrator: **anything the system prompt claims
about the environment is generated from it**, alongside the charter-generated
authority section. `workdir_is_checkout` and `github_configured` decide whether
the prompt offers a checkout it may edit and whether it warns that GitHub
writes will fail. A hardcoded "your working directory is the project checkout"
is what sent a curl-only agent reaching for `python3` and `Write`.

| var | default | |
| --- | --- | --- |

| var | default | |
| --- | --- | --- |
| `TASKS_SERVER_PORT` | 4800 | HTTP API port (also `--port`) |
| `TASKS_POLL_INTERVAL` | 60 | seconds between GitHub polls. It has a second job: the poll is the only thing that observes GitHub's reachability, so a dispatch hold expires after **10 × this**, floored at 10 minutes (`GitHubHealth::stale_after`). Raising it slows how quickly an outage is noticed *and* lengthens how long a hold survives a dead poll loop |
| `TASKS_DEFAULT_MODE` | `pause` | the mode **every** boot starts in, overwriting whatever the last process left in the store — `play`, `pause` or `stop`, and an unparseable value refuses to boot rather than being ignored. Only `tasks reload` overrides it, by passing the old server's mode to the new one |
| `TASKS_ENV_FILES` | `on` | `off` skips `.env` loading entirely — for tests that exec the `tasks` binary, where `env_remove` promotes a `.env` rather than scrubbing it. Anything that is neither `on` nor `off` refuses to boot |
| `TASKS_INTAKE_LABEL` | — | when set (e.g. `tasks`), only open issues carrying that label are ingested; matched case-insensitively. Applied after the fetch, so closure tracking still sees the complete open set. Un-labelling an issue keeps its existing task, it just stops refreshing it |
| `SCOUT_MAX_CONCURRENT` | 2 | scouts running at once. Each holds a vm-pool slot and the serial build lane holds one more, so the pool must fit `SCOUT_MAX_CONCURRENT + 1` — 3 is the recommended ceiling against the default pool of 6, and the server `warn!`s on every connect if the pool it found is short or an exact fit. See *Pool capacity* |
| `SCOUT_IMAGE` | `agent:v1` | vm-pool image scouts run in |
| `BUILDER_IMAGE` | `builder:v1` | vm-pool image builds run in. Paired with `SCOUT_IMAGE` rather than appended at the end: they are the same knob for the two halves of the pipeline, and `make images` rebuilds both, so a reader who finds one should find the other |
| `SCOUT_TIMEOUT_SECS` | 3600 | budget per scout, measured on both clocks (see *Budgets and a host that sleeps*); past it the VM is deallocated and the attempt counts as a dispatch failure — unless the host was asleep for enough of it (a quarter of the budget left unspent), which is `Suspended` and costs nothing. Keep below vm-pool's `vm_timeout` (7200) |
| `SCOUT_CHECKPOINT_INTERVAL_SECS` | 30 | how often a Scout's `NOTES.md` is **re-read** and streamed back as a checkpoint. It does not govern the *first* read: the watcher probes once a second until notes appear, because salvage matters most for the runs that end badly and early and those are exactly the runs inside the first interval (#968) — a shorter interval would trade that window for polling cost without removing it. Read *inside* the VM, so it is set in `images/scout/Dockerfile`, not here |
| `SCOUT_MAX_RESUMES` / `BUILDER_MAX_RESUMES` | 2 | times a supervisor re-invokes an agent with `--resume <session_id>` after its API connection dropped mid-response (#845). Only a transport death is retried, and the backoff rises 2s / 15s / 30s. `0` disables it. Read *inside* the VM, so both live in `images/{scout,builder}/Dockerfile` |
| `SCOUT_VM_CPUS` / `SCOUT_VM_MEMORY_MB` | 4 / 6144 | shape of a Scout VM. Multiplied by `SCOUT_MAX_CONCURRENT` on the host — lower one of the three on a small machine |
| `BUILDER_VM_CPUS` / `BUILDER_VM_MEMORY_MB` | 4 / 8192 | shape of a Builder VM. Larger than a Scout's because builds are serial (nothing multiplies it) and a killed Builder costs a whole implementation |
| `BUILDER_SUITE_BUDGET_SECS` | derived | hard cap on the in-VM test suite (`.tasks/verify`), overriding the derivation outright; `0` skips the suite and reports `Unavailable`, which is never green. Derived, it is the run budget still unspent minus a 120s packaging reserve, floored at 60s — sized to expire *before* `BUILDER_TIMEOUT_SECS` does, which is what keeps the outer expiry defensible as a `Verdict`. Read *inside* the VM, so it belongs in `images/builder/Dockerfile` rather than here |
| `BUILDER_TIMEOUT_SECS` | 3600 | budget per build, allocation included, measured on both clocks (see *Budgets and a host that sleeps*). Past it the VM is deallocated, the build fails and every spec in the batch is charged a build attempt — unless the host was asleep for enough of it (a quarter of the budget left unspent), which is `Suspended` and charges nothing (#929). Same ceiling argument as the scout's: keep it below vm-pool's `vm_timeout` (7200) |
| `SCOUT_BUILD_JOBS` / `BUILDER_BUILD_JOBS` | derived | `CARGO_BUILD_JOBS` injected per-VM. Derived from the VM's memory — `(memory_mb − 2048) / 2048`, clamped to `[1, cpus]` — because cargo defaults `-j` to the CPU count and knows nothing about the memory limit, which is how 4 CPU / 4 GB VMs got a linker OOM-killed. Set either to override the derivation |
| `VM_POOL_SOCKET` | `/tmp/vm-pool.sock` | vm-pool service socket. A start against a socket something is already listening on **refuses** rather than taking the path over — stop the running daemon first. A socket file left by a dead one is unlinked and reclaimed |
| `VM_POOL_MAX_VMS` | 6 | VMs the pool holds at once. Read by **`tasks vm-pool`** (and the stock `vm-pool` binary), never by the server, so a change takes effect on a pool restart. Anything that is not a positive integer refuses to boot — `0` binds and answers `status` while failing every allocate. See *Pool capacity* |
| `TASKS_VM_POOL_AUTOSPAWN` | derived | whether a failed vm-pool connect spawns the pool from the serving binary, detached, logging to `<data dir>/vm-pool.log` (`on`/`off`; anything else refuses to boot). Unset, the default is derived from where the binary lives: `on` for an installed binary (no workspace above `current_exe()` — a `make dist` bundle), `off` for a checkout artifact, whose developer restarts the pool deliberately. Safe because the pool refuses an occupied socket, so racing spawns resolve to one daemon. See *The daemon is the product* above |
| `GITHUB_TOKEN` | — | **fallback** for `tasks secrets set github-token`, warned at startup — the sealed store is where production keys live. Needed (either way) for polling; the broker spends it for clones and the land push |
| `GITHUB_API_URL` | api.github.com | GraphQL endpoint override |
| `GITHUB_OAUTH_URL` | `https://github.com` | where `tasks auth login` speaks the device flow. The OAuth endpoints live on github.com rather than the API host, so `GITHUB_API_URL` is deliberately not reused — override for tests only |
| `GITHUB_CLONE_URL_BASE` | `https://github.com` | clone URL prefix, and where the broker forwards git traffic. A non-http(s) base (a `file://` mirror) cannot be proxied and clones direct — see the credential-custody rule |
| `TASKS_BROKER_PORT` | 4801 | credential broker listener — where VMs redeem run leases. A second listener on purpose; the API stays loopback-only |
| `TASKS_BROKER_BIND` | `0.0.0.0` | broker bind address. All interfaces because the vmnet gateway does not exist until the first container starts; every route demands a live lease |
| `TASKS_BROKER_ADVERTISE` | `192.168.64.1` | the broker's address as VMs see it (apple/container's bridge gateway). Also what the dispatch gates probe every 15s to decide the broker hold, and what `tasks doctor` probes — never loopback, which answers correctly while the gateway is severed |
| `TASKS_BROKER_ANTHROPIC_UPSTREAM` | `https://api.anthropic.com` | where Anthropic traffic forwards — override for tests only |
| `TASKS_SECRETS_KEY_FILE` | — | unseal-key file, outranking the credential-store item the store header names. The Linux/test path — and **first-class on macOS too, not a fallback**: an access list is granted to an *application*, so an unsigned dev build is a different one on every `cargo build` and a natively-stored key re-prompts each time, which a launchd-started server has no window server to answer |
| `ORCHESTRATOR_CMD` | `claude --print … --allowedTools Bash(curl:*)` | orchestrator agent command; its permission flags decide what the orchestrator may do. Split shell-style, so quotes group — `--allowedTools "Bash(git log:*)"` survives as one argument, which is the only way a prefix-matched allowlist can be written in verbs (#976) |
| `ORCHESTRATOR_WORKDIR` | `<data dir>/orchestrator` | orchestrator cwd; point at the repo checkout (with `--dangerously-skip-permissions` in the cmd) to run it as a full dev agent |
| `ORCHESTRATOR_TIMEOUT_SECS` | 900 | budget per orchestrator tick, measured on both clocks (see *Budgets and a host that sleeps*). Claude Code's per-command ceiling is derived as **half** of it (`orchestrator::command_budget`, floor 60s) and set on the child as `BASH_DEFAULT_TIMEOUT_MS`/`BASH_MAX_TIMEOUT_MS` — so whatever a command spent, at least that much turn is left to report it in. Bounded above by `OBLIGATION_REMINDER` (30 min) |
| `ORCHESTRATOR_TARGET_DIR` | `<data dir>/verify-target` | `CARGO_TARGET_DIR` for the orchestrator's own verification, set on that child process and nowhere else, alongside `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_{DEV,TEST}_DEBUG=line-tables-only` (`orchestrator::VERIFICATION_ENV` — `make verify-warm` sets the same three, and a test fails if they drift). Shared and long-lived — the warmth is the value. Its size is on `/status`, `tasks status` and the Server window, and it is bounded by `ORCHESTRATOR_TARGET_BUDGET_GB`. `make verify-warm` primes it. There is no `off`: every value here is a path, so `ORCHESTRATOR_TARGET_DIR=<checkout>/target` is the escape hatch |
| `ORCHESTRATOR_TARGET_BUDGET_GB` | 20 | ceiling on that directory, past which the orchestrator loop reclaims it in two tiers — every `<profile>/incremental` first (no warmth lost), and only if that is not enough, the directory's contents (**the next verification is cold**, and that is announced on the feed and stays on `/status` for the boot). `0` keeps the report and drops the reclaim, the `TASKS_UPDATE_HOLD=off` shape; the *report* half is deliberately not switchable. The default is a judgement, not a measurement — see the design bullet |
| `WORKER_CMD` | `claude --print …` with a no-`curl`, no-push allowlist | worker agent command (#1053). The default's omissions are the enforcement: no `Bash(curl:*)` (an unattributed local process writes as the *human*, so API access would be a route around the charter) and no `git push`; verbs, quoted, split by `split_command` |
| `WORKER_TIMEOUT_SECS` | 3600 | budget per worker run, measured on both clocks (see *Budgets and a host that sleeps*). Four orchestrator turns on purpose — the lane exists so a suite run stops having to fit inside the turn the human is waiting behind. Per-command ceiling is derived as half, like the orchestrator's |
| `TASKS_UPDATE_HOLD` | `on` | whether new scouts and builds wait while an update is pending — a newer server binary on disk awaiting `make restart`, or a VM image observed running a build older than this server's awaiting `make images`. `off` keeps the `/status` report and drops the gate; anything else refuses to boot |
