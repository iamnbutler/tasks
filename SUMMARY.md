# One defect read by three loops, and an intent ledger under every GitHub write

Two changes ride this batch. The first is a single shared SQL predicate,
`carried_by_a_later_build`: both readers of "which build is still waiting on a
pull request" found one by joining `builds → build_specs → specs → tasks` and
filtering `t.state = 'awaiting_merge'`, which identifies a **parking** and not
the build that caused it. A task carries no memory of which build put it there,
so the instant a rebuild re-parks a spec, every earlier succeeded build carrying
it matched again — forever. The obligation half is #956, a `land_batch` naming a
PR nobody can merge with no act that discharges it. The poller half is #959 and
is the destructive one: `list_builds_awaiting_merge` kept selecting the dead
build, whose PR answers closed-unmerged on every poll, so `watch_merges` re-ran
`unwind_unmerged_build` against it every pass — charging the **live** build's
specs a build attempt each time until they capped and `blocked` themselves, and
dragging their tasks out of `awaiting_merge` while the new PR was still open.
The predicate is applied to both queries, to all three statements inside
`unwind_unmerged_build` (per spec, so a batch a rebuild only partly re-carried
still returns the half nothing took over), and `Store::build_superseded` is
rewired onto it so there is one notion of supersession rather than two that can
drift. A third reader, `brief::is_unresolved`, was making the same
mis-attribution about which builds still claim which files, and now reads a
`Store::builds_superseded` set first. No migration, no GitHub read, no stored
GitHub-owned fact: supersession is derivable from `build_specs`,
`builds.status` and `rowid`. **Both #956 and #959 are resolved by this build.**

The second closes the other half of the attribution gap (#964). #957 made every
refusable failure a genuine no-op by moving `require_rationale` into
`authorize`, ahead of the effect; what remained was the half that cannot be
refused — all ten sites that write to GitHub ran the effect and *then* the
`record_decision` explaining it, so a SQLite error, a panic or a SIGKILL in
between left a real artifact upstream that nothing accounted for. Recording
first stays refused (a row claiming an effect a failed call never had makes
every row suspect), so the window is represented instead: `decisions` grows a
`state` (`pending` → `applied` | `annulled`), and `server::ledgered` takes the
effect **as a closure**, so a handler nobody has written yet cannot reach GitHub
without its intent already on record. The three outcomes are decided
structurally off `GhError::is_unavailable` and never off message text — and
"GitHub never answered" leaves the row `pending`, because we do not know and
saying so is the point. `ObligationKind::ReconcileDecision` chases the residue,
and `GET /decisions/{seq}/reconcile` is what makes it dischargeable: the
**server** asks GitHub with its own credential and returns what it found, then
`POST /decisions/{seq}/settle` writes that answer down. Along the way,
`create_issue` was the one write not going through `rest_ok` — it parsed the
body before checking the status, so a 5xx whose body is not JSON became a
statusless decode error that `is_unavailable()` read as GitHub *answering*, and
would have annulled a decision whose issue may well have been filed.

## Review feedback

### On spec 1 of 3 (#959)

- **"Build the #956 design, not this one."** Done. The #956 spec is what is
  implemented; the shared `carried_by_a_later_build` predicate replaces the
  `builds.pr_resolution` column entirely.
- **"Do not add the migration, the `pr_resolution` column, `record_pr_resolution`,
  or the `Build.pr_resolution` wire field."** None of the four exists. No
  migration was added for this half at all.
- **"Nothing in your spec is lost — the `brief::is_unresolved` fix falls out of
  the `list_builds_awaiting_merge` change directly."** Partly declined as
  stated, and then done properly. `is_unresolved` does *not* walk that query —
  it is a pure function over the brief's loaded `World`, so the fix did not fall
  out of the change and would have been silently dropped. It is fixed
  explicitly, with `Store::builds_superseded` (the same predicate, offered as a
  set) read in `load_world`, and the doc comment whose claim was quietly wrong
  is corrected. `Brief::pipeline`'s "parked behind PR #…" line *does* walk
  `list_builds_awaiting_merge` and is fixed by the query change, as the #956
  spec notes.
- **"Your `Unwind::{Superseded, Returned}` is what per-spec filtering achieves
  without a new type."** Agreed; no new type. `unwind_unmerged_build` keeps
  returning `Vec<TaskId>` and simply returns nothing when nothing is its to
  move.
- **"You were right about the mechanism first — say so in `SUMMARY.md`: both
  issues close on this build."** Said, in the first paragraph and here.
- **"If you find a case the predicate cannot cover that the column could, stop
  and say so rather than reintroducing the column quietly."** I found none. Four
  cases checked: (a) a build whose PR closed unmerged and whose specs were never
  rebuilt — its tasks return to `ready_to_build`, so it leaves both queries, as
  you checked; (b) a build that shipped — the issue closes and `t.gh_state =
  open` drops it, pinned by the existing test asserting `pr_reads` stops at 1;
  (c) a partial rebuild — per-spec filtering keeps the un-recarried half, which
  the column could not express at all; (d) `retire_work` `off`/`shadow`, where
  nothing unwinds and the batch keeps raising `land_batch` — the column would
  not help either, since `watch_merges` writes nothing in those modes. That last
  one is the announced cost of the kill switch and is documented as deliberately
  unfixed.

### On spec 2 of 3 (#956)

- **"No required changes."** Built as specified: the shared fragment with its
  two parameters (not collapsed into a `const &str`), `build_superseded`
  rewired onto it, both queries filtered, all three statements in
  `unwind_unmerged_build` filtered per spec on numbered binds, the subject left
  as the build id, `succeeded`-only and `rowid`-ordered.
- **"Keep the stub-to-`\"0\"` instruction in the summary."** Kept, and used:
  stubbing `carried_by_a_later_build` to return `"0"` was run, and both store
  tests fail — `a_rebuilt_batch_stops_obligating_the_build_it_was_rebuilt_past`
  with two `LandBatch` obligations where there must be one, and
  `unwinding_a_build_a_rebuild_has_passed_touches_nothing` with the live build's
  task dragged out from under its open PR. It is a two-line check that the fix
  is load-bearing rather than a query that happened to be rewritten.
- **"Say the poller-fix-is-destructive point in `CLAUDE.md`, not only in
  `SUMMARY.md`."** Done — it is the emphasised clause of the new bullet, with
  #938/#952 named.
- Three tests, as specified:
  `store::tests::a_rebuilt_batch_stops_obligating_the_build_it_was_rebuilt_past`,
  `store::tests::unwinding_a_build_a_rebuild_has_passed_touches_nothing`, and
  `merges.rs::a_rebuilt_batch_leaves_the_build_it_was_rebuilt_past_alone`. The
  fake in `merges.rs` gained the per-number `prs` overlay and `read_numbers`;
  every existing test in the file is unchanged.

### On spec 3 of 3 (#964)

1. **"`ReconcileDecision` is only dischargeable if the orchestrator can read
   GitHub, and in the default configuration it cannot."** Fixed as you asked,
   by moving the lookup to the server: `GET /decisions/{seq}/reconcile` reads
   the intent recorded on the pending row, asks GitHub with the *server's* own
   credential for the artifact that intent describes — an issue by title for a
   capture, the issue state for a close or reopen, the current text for an
   edit, the label set for a labelling, the comment list for a comment or
   review comment, the PR for a merge or abandon — and returns
   `applied | annulled | unknown` plus what it saw. `unknown` is a real verdict
   and never a licence to guess: the row stays pending. The obligation summary
   and the brief both name that call and then the settle, in that order, and
   the prompt says explicitly not to redo the write. I did not take the
   poller-auto-resolves variant: it would resolve the same set with the same
   lookups while making the residue harder to name, and a `GET` the recipient
   runs deliberately is easier to reason about than a background pass that
   settles rows nobody asked it to.
2. **"The nine-route guard does not guard against a tenth route — make the
   enumeration structural."** Done, and taken further. `DecisionAction` is now
   macro-generated, so `ALL` is complete **by construction** rather than
   hand-maintained (the macro's third column is the `capability()` mapping you
   suggested as the spine). `no_write_route_reaches_github_without_recording_first`
   drives `ALL` through an exhaustive `write_route` match, so a new variant does
   not compile until somebody says whether it reaches GitHub, and the guard then
   drives it. Your second objection — that the test only checked the end state —
   applied to the spec's version too, so the fake GitHub now reads the ledger
   *at the instant each write arrives* and reports the pending count in its
   error body: the assertion is about ordering, and it fails (verified) if
   `ledgered` records after the effect instead of before. One residue is stated
   rather than assumed: a new route reusing an *existing* action is not covered,
   because the guard drives one request per action.
3. **"A capability demoted to `off` while a row is pending makes this obligation
   permanently undischargeable."** Answered by letting the settle through:
   `POST /decisions/{seq}/settle` is **never charter-gated**, and the handler
   says why — settling is not the action, the effect already happened, and
   refusing to record it does not un-file the issue, it only keeps the ledger
   wrong. The response reports which capability the settled row came from, and
   the brief names its current level, so a reader can see it has been switched
   off. `a_settle_is_not_refused_by_a_capability_since_demoted` pins it: the
   capture route 403s while the settle for a pending capture returns 200. A
   rationale is still required of the orchestrator.
- The "not required" items were built as approved: one row with a `state`
  column, `DEFAULT 'applied'` leaving store-only decisions alone by
  construction, a pending action charged against the daily cap
  (`state <> 'annulled'`), `outcome` merged with `json_patch`, and a settle
  failure logged rather than propagated.

## Directions for this implementation

1. **"The close outcome becomes ternary and #959 must be written against
   that."** Decided explicitly and written in `watch_merges`. Since the #959
   resolution column is not being built (spec 1's feedback redirects it), there
   is no stored resolution to withhold — a PR's ending is GitHub's fact and is
   re-read every poll. So: an **annulled** close (GitHub answered no) ends the
   intent, a **pending** close (GitHub never answered) is left open because a
   write that may have landed must not be recorded as one that did not, and
   neither moves the batch. The task stays `awaiting_merge`, the next poll reads
   the same PR and retries the close under the *same* intent row — reused via
   `Store::pending_decision`, because there is only one intent and a poll every
   minute through an outage would otherwise leave a row a minute. A close is
   idempotent upstream, so retrying one that did land costs a call and changes
   nothing. `a_close_records_its_intent_before_it_reaches_github` walks all
   three states in one story.
2. **"Give the `watch_merges` close its own test that it records intent before
   the effect."** Done, and it is an ordering test rather than an end-state one:
   the fake GitHub in `merges.rs` reads the ledger when the close request
   arrives and the test asserts the intent was already there.
   `a_close_github_refuses_is_annulled_rather_than_left_pending` covers the
   other branch.
3. **"Keep #956's parked-rather-than-unwound behaviour intact."**
   `a_merged_but_unreachable_batch_stays_parked_and_records_no_close` asserts it
   directly — the task stays `awaiting_merge`, the spec stays `built`, and no
   `retire_work` row exists at all, pending or otherwise — alongside the two
   pre-existing stacked-merge tests, which are unchanged and still pass.
4. **"Run the suite with `make test` in the FOREGROUND and paste the counts."**
   Done; counts in the trailer below. `cargo-nextest` was present, so no
   substitution was needed. The 7 LEAKs are the documented scout/builder timeout
   ones. **#958 was not observed** — `a_timed_out_scout_keeps_the_checkpoint_it_had_already_streamed`
   passes here (it reports LEAK, which the profile treats as a pass).
5. **"#964's first feedback item must be resolved before building the obligation on top
   of it."** Done in that order: `GET /decisions/{seq}/reconcile` exists and the
   obligation, the brief and the prompt are all written against it rather than
   against a lookup the recipient would have had to perform itself.

## Notes

- `crates/tasks/migrations/20260818031658_decision_intent.sql` is the only
  migration, and it belongs to #964. There is **no** migration for #956/#959.
- `Decision`'s three new fields are `#[serde(default)]`, so `tasks-client` and
  `app-gpui` are unaffected by skew; `make app-check` is clean.
- Nothing here touches a supervisor, so there is no coupling to a VM image
  rebuild: it ships with an ordinary `make restart`.
- `CLAUDE.md` gains two load-bearing bullets, one per change.

Verification: PASSED — `make test` (834 tests run, 834 passed, 0 failed, 7
leaky as documented; doctests included), plus `cargo fmt --all`, `cargo clippy
--workspace --all-targets` clean and `make app-check` clean. Each guard was
additionally proven load-bearing by disabling it in isolation: stubbing
`carried_by_a_later_build` to `"0"` fails both #956 store tests, and moving
`record_intent` after the effect in `ledgered` fails
`no_write_route_reaches_github_without_recording_first`.
