Three approved specs, committed separately in the order the directions
asked for, all in `app-gpui`: the All Tasks list gets its first selection
model (#1066), the rail's Tasks header gets a Clear control and its
finished rows sort last (#1029), and the pre-release sweep removes the one
control that ships permanently disabled, gives the machine's vocabulary a
definition it cannot be rendered without, and puts the repository link in
the About window (#998). The three are one branch because all three edit
`workspace.rs`, and two of them also edit `main.rs` and
`sections/tasks.rs`; ordering #1066 first meant the later two were written
on top of the selection rather than under it.

**#1066** — `Workspace::selected_task` is derived from navigation
(`settle_after_nav` maps `MiddleView::AllTasks => None`), so it is `None`
for the whole time the catalog is on screen: the row's `is_selected`
background was unreachable in the only view that renders it and ⇧⌘U / ⇧⌘S
/ ⇧⌘A reported "select a task first" in exactly the view you would use
them from. `selection::TaskSelection` is the list's own selection — keyed
by id and never by index (rows come and go behind the archive toggle),
order never stored but taken from the list at the moment it is asked, and
a ⇧-sweep that adds and never removes with an anchor that survives it, so
overshooting costs one click. `row_menu::bulk_entries` folds the *same*
`entries` over the selection, so there is exactly one derivation of "can
this run?" and both surfaces read it; `BULK_ACTIONS` is a fixed ordered
subset that excludes the three destructive verbs, the review composer and
the browser tab, with a test asserting every offered verb is
non-destructive — read off `entries`, so a verb that *becomes* destructive
later fails there too. `perform_row_action` splits: the legality check and
the single-row report stay, the `match` moves to `dispatch_row_action`,
and the bulk path re-derives legality over the whole selection and reports
one aggregate receipt naming both counts. Every verb is a per-task POST —
there is no bulk endpoint and this deliberately does not add one — so
partial legality is the normal case. The clipboard verbs are special-cased
into one newline-joined write, because dispatched per row they would leave
the last row's value and silently drop the rest while the receipt claimed
all of them. The orchestrator hand-off sends the ticked rows as one
message that states them and commands nothing. Escape clears the ticks
first and leaves the middle column's dismissal to the second press;
pruning runs from the three places that change what is visible without
changing what is ticked (the `app_state` observer, the archive toggle, the
repo filter), because it cannot live in the render path.

**#1029** — read against the middle pane the draft is two no-ops and a
control in the wrong pane; read against the left rail, which is the only
surface with a `+` in a "Tasks" header, all three parts are real.
`band()` has never admitted `Done` or `Rejected` to the tree, and the one
band it holds whose work is finished — `AwaitingMerge`, implementation
done and a pull request open — sat above both draggable strata, with no
count ceiling on it. It now sorts at band **5**, strictly after
`ReadyToBuild`'s 4 (see the review-feedback note below), and
`clear_finished` is the rail's counterpart to `sections/tasks.rs`'s
`archive`: drops rows and never sorts them, counts what is *finished*
rather than what is hidden, and runs over `tree_rows`' output so the count
is the count for the repo on screen. The header chip sits immediately left
of the `+`, states its count on both sides of the toggle and says in its
tooltip that nothing is closed — "Clear" is the one word here a reader
could take as destructive, and the count is the receipt. The All Tasks
archive footer is untouched: different surface, different set, disjoint by
construction.

**#998** — the voice-mode microphone is gone, with the only hardcoded
gruvbox colour in the tree (its SVG stroke), the divider and the
asymmetric padding that seated it; nothing in the app now promises a
feature it does not have. `vocabulary.rs` defines the pipeline's words
over four total matches with no `_` arm, so a state added to `tasks-api`
is a compile error here rather than a title-cased wire string nobody
defined; the label stays the machine's word (a friendlier one would put a
second vocabulary between this app and `tasks status`) except `gh_state`,
which rendered raw and is now "Issue Open" / "Issue Closed".
`status_badge` takes the gloss **by value** and the old no-tooltip badge
is deleted rather than kept beside it, which is what makes the guarantee
structural. The palette's private `build_status` — five wire strings
re-spelled under a doc comment claiming `BuildStatus::as_str` did not
exist, which it does — went with it. About gains the repository link its
"README.md, under Read this first" pointer had nothing behind it in the
one window with no checkout in reach.

## Verification

`.tasks/verify` is `make test-ci`, which is `cargo nextest --workspace`,
and **`app-gpui` is not a workspace member** — so the supervisor's run
compiles none of this change. Nothing under `crates/` is touched by this
branch, so that run is a regression check rather than evidence about the
work. The evidence is these, run here:

- `make app-check` — clean. No errors; the only warnings are pre-existing
  dead code in `modal.rs` and `bin/tasks-menubar/popup.rs`.
- `make app-test` — **298 passed, 0 failed** (`tasks-gpui`) and **35
  passed, 0 failed** (`tasks-menubar`). It did **not** OOM-kill the linker
  on this 8192 MB Builder VM, so these are `make app-test`'s numbers and
  not a fallback's. 261 tests before this branch; the 37 new ones are 14
  in `selection`, 12 in `row_menu::bulk_tests`, 4 in `rail`, 5 in
  `vocabulary` and 2 in `about`.
- `cargo clippy --all-targets` for the app — no new warnings.
- `cargo fmt` — clean, with the one exception recorded below.

Not verified, and it is the boundary `CLAUDE.md` already draws: this VM
compiles and unit-tests the app but cannot run it. Every pixel claim —
where the selection bar lands, whether the 16px tick box reads as a
checkbox, whether the header chip crowds "Tasks" at a narrowed rail width,
whether `theme.accent()` is the right check colour — wants one `make app`
on a Mac. The logic beneath all of it is pure and tested.

## Review feedback

**Spec 1 (#998)**

1. *Run `make app-check` and `make app-test` yourself and report the exact
   commands and outcomes.* Done — both above, both green, `make app-test`
   rather than a fallback.
2. *Do not re-do the items already done; no placeholder icon or
   screenshots.* Done. The per-tab empty states and the Changes tab's
   linkage sentence are untouched, nothing was added to `empty_state.rs`,
   no `.icns` and no screenshots were committed. `about.rs`'s module doc
   records why the icon is absent and what landing one would take.
3. *Build from current main and re-locate rather than trusting line
   numbers.* Done — every site was re-read in the tree I was given.
   `MIC_SVG`, the `voice-mode` div, `BuildStatus::as_str` and the four
   `status_badge` call sites were all where the spec said, and the two
   `detail.rs` badges are one call site pair rather than two separate ones.
   Preserved as asked: the old no-gloss `status_badge` is **deleted**, not
   kept as a shim.

**Spec 2 (#1066)**

1. *You own `row_menu`'s set derivation and there must be exactly one; do
   not accommodate #1067's `Selection` / `fold` / `Arity` / `RowItem.scope`
   shape.* Done. None of those names is in the tree — I grepped before
   starting — and `bulk_entries` / `BulkItem` / `BULK_ACTIONS` is the only
   generalisation, folding `entries` rather than restating it.
2. *The exclusions are part of the deliverable; keep the non-destructive
   test.* Done, and doubled: `every_offered_verb_is_non_destructive` reads
   `destructive` off `entries` so it catches a verb that changes later, and
   `the_named_exclusions_stay_excluded` names the five individually.
3. *Report which command produced the numbers; fall back if the linker
   dies.* Done — `make app-test` succeeded, no fallback needed, stated
   above.
4. *Leave `sections/changes.rs`'s formatting drift alone.* Done. That file
   carries exactly two hunks: the badge call site and the now-unused
   `title_case` import. `cargo fmt` reflowed its `use
   tasks_client::api::models::{…}` line and I reverted that hunk by hand.

**Spec 3 (#1029)**

1. *The spec's band numbers are wrong; use 5 or renumber, and make the
   test fail on a tie.* Done — the spec said "2 → 4, after `Queued` 2 and
   `ReadyToBuild` 3", and the tree says `Scouting` 0, `Building` 1,
   `AwaitingMerge` 2, `Queued` 3, `ReadyToBuild` 4, so 4 would have tied
   with `ReadyToBuild` and interleaved the finished rows among it under a
   stable sort. `AwaitingMerge` is band **5**.
   `the_tree_is_attention_ordered_and_review_free` now asserts the new
   order *and* that `band(AwaitingMerge)` is strictly greater than every
   other band in the tree, so a tie fails it. **This overrides the spec**,
   as the feedback is the later word.
2. *Report which command produced your numbers.* `make app-test`, above.
   The `-j 1` fallback was not needed.

## Directions

- *Order and commit them separately: #1066, then #1029, then #998.* Done —
  three commits in that order.
- *Read #1029's own conclusion about whether it replaces the archive footer
  and do not quietly ship both.* Its conclusion is that they are different
  things over disjoint sets, and that is what shipped: the rail header's
  control governs `AwaitingMerge` in the tree, the All Tasks footer governs
  `Done` in the catalog, and the footer is unchanged. The labels are chosen
  so they cannot be confused ("3 open PRs · Clear" against "3 done · Show")
  and `clear_is_about_awaiting_merge_and_not_about_done` pins that the rail
  has never shown a done task at all.
- *`app-gpui` is not a workspace member; run `make app-check` and
  `make app-test` yourself and report both.* Done, above.
- *PR #1077 is open and touches `main.rs` and `workspace.rs`; do not try to
  reconcile with it.* I did not. This branch's `main.rs` reach is two `mod`
  lines and its `workspace.rs` changes are local to the selection, the chat
  chip and one new field; nothing was reproduced from or adjusted for that
  branch, and I hit no conflict I could not resolve locally. If #1077 lands
  first, the likely collisions are the `mod` list and the `Workspace`
  struct's field block, both of which are additive.
