# Say why the panes are empty — one ordered diagnosis, five placements

The app had four blank panes and one sentence between them, so "no server",
"no repositories", "400 issues sitting in backlog" and "everything shipped"
all rendered identically. Every fact needed to tell them apart already
existed and nothing joined them: the lists, the mode and the connection live
in `AppState`, while the three dispatch holds live on `GET /status`, which
until now only the Server window ever read. This adds
`app-gpui/src/empty_state.rs` — a gpui-free module of pure functions over
plain data, the `feed.rs` / `chat_log.rs` precedent — that counts the
pipeline once and walks an ordered list of layered claims, returning exactly
one `Situation` plus the headline, the sentence, the button that fixes it,
and any dispatch holds that are true but are not the headline. Fourteen
situations, enumerated from the code rather than from the issue: its own
five, plus awaiting-review (`rail::band` keeps `InReview` out of the
tree, so this is the commonest non-idle empty tree), awaiting-merge,
stopped-as-distinct-from-paused, dispatching-but-nothing-started-yet, and the
three connection states as three rather than one.

It renders in five places: the rail's empty tree, a compact *standing* line
above a rail that has rows and is not moving, the middle column before the
first snapshot, the catalog's empty list, and the chat when nothing is
reachable. Every button routes through the same action the menus already
dispatch (`menus::RestartServer`, `open_repo_window`, `navigate(AllTasks)`,
`Workspace::set_mode`), so there is no second path to starting the server and
the Server window's confirmation stays the only confirmation — and the new
Play affordance reads its caution from `disclaimer::PLAY_TOOLTIP` and
`disclaimer::PIPELINE_CAUTION` (#984, already in this base) rather than
inventing a second wording. The main window's 1s view timer now also refreshes
`ServerControl` every `server_window::POLL`; without it the two hold
situations are dead code that never fires, because nothing outside the Server
window ever read `/status`. 35 unit tests pin the walk, the counting, the
hold semantics and all fourteen sentences.

## Where the diagnosis can appear today

Rail empty tree — `Connecting`, `NoServer`, `Incompatible`, `NoProjects`,
`NoTasks`, `AwaitingReview`, `NothingQueued`, `Idle`. Rail standing line —
`Paused`, `Stopped`, `Held`. Middle column before the first snapshot, and the
chat — the three connection states. Catalog — as the rail's empty tree, minus
`AwaitingReview` and `NothingQueued`, since backlog rows *are* the catalog.
`Working`, `Dispatching` and `AwaitingMerge` are computed and have no
placement today; they are kept deliberately, because `Working` is what stops
`Idle` ("nothing owed a decision") being reported over a running pipeline by
any placement added later, and deleting the three makes the walk non-total.

## Functions touched in `workspace.rs`

Contended with #984 (landed) and #987 (queued behind), so additions are
additions and nothing neighbouring was reflowed, reordered or renamed:

- **New:** `reachability`, `pipeline`, `explanation`, `render_explanation`,
  `run_empty_state_action` — one contiguous block immediately above
  `render_center`.
- **Changed, and only as the spec names:** `Workspace::new` (the existing 1s
  view-timer loop gained a `server_control.refresh()` every
  `server_window::POLL`; nothing else in it moved); `render_center` (the
  `!loaded` branch only); `render_chat_empty_state` (return type widened to
  `gpui::AnyElement`, plus an early branch when reachability is not `Up` — the
  "Talk to the orchestrator" invitation below it is untouched, including its
  wording and layout).
- **Imports:** two `use` lines added (`crate::empty_state::…`,
  `crate::server_window`).

`render_chat_chip` is untouched, which is the region #987 rewrites; the only
overlap is that both changes are inside the chat pane.

Outside `workspace.rs`: `rail.rs` (`render_left_sidebar` — the empty tree's
single string becomes the diagnosis block, and a compact line is added under
the section header below the queue notice), `sections/tasks.rs`
(`render_tasks` — the empty catalog's `done_count == 0` arm becomes the
diagnosis with `OpenAllTasks` dropped; the `done_count > 0` arm keeps its own
"Nothing open — every task here is done."), and `main.rs` (module
registration).

## Review feedback

1. **Route the new Play affordance through the `disclaimer` module.** Done.
   `disclaimer.rs` is present in this base (#984 landed), so nothing was
   reworded: the button carries `disclaimer::PLAY_TOOLTIP` as its tooltip,
   and — having considered `PIPELINE_CAUTION` as the item invites — it also
   renders `disclaimer::PIPELINE_CAUTION` in full beneath the button, in
   every placement including the compact one. The reasoning is the item's
   own: this is the surface with the least context, and a caution only on
   hover is one the reader with the least context has to find. The cost is
   that the rail's standing line is materially taller than a one-line notice
   would be (the caution wraps to roughly four lines at 240px) — that is a
   judgement about pixels, and it is one of the things `make app` on a Mac
   should confirm or overturn. `Action::starts_the_pipeline()` is what the
   render site reads, so a future action that also dispatches inherits both
   strings rather than being remembered.
2. **Justify the `diagnose` ordering, specifically running-before-awaiting-
   review.** Done, in `diagnose`'s doc comment, and the answer is the
   structural one rather than the charter one: *the two cannot both be
   candidates in any placement that exists today.* Every placement that can
   report `AwaitingReview` requires an empty list — `rail::tree_rows` empty,
   or the catalog empty — and `rail::band` puts `Scouting` and `Building` in
   the tree while the catalog shows every non-done task, under the same repo
   filter this walk counts with. So `running > 0` implies the list has a row
   and the diagnosis is not rendered at that placement at all. The
   charter-dependent argument the feedback offers (with `auto_review_specs`
   live, awaiting-review is transient and self-clearing, so headlining it
   would be noise most of the time) is recorded in the same comment as the
   weaker, secondary reason, and the comment says a placement that can show
   both must revisit the line rather than assume it.
3. **`workspace.rs` is contended; add rather than restructure, and list the
   functions.** Done — the function list is above, and it is functions rather
   than the file. No neighbouring function was reflowed, reordered or
   renamed. On the named direct overlap: `render_chat_empty_state`'s return
   type is widened as the spec requires, and `render_chat_chip` — the region
   #987 rewrites — is not touched at all.

**One thing for the merge, not a change to make.** Acknowledged and acted on:
`make test` compiles no part of `app-gpui`, so the workspace suite says
nothing about this change. What was run is recorded below. Rendering is
**unverified**: whether the compact line sits under the right header, whether
three simultaneous placements collide on element ids, whether the catalog's
own "Nothing open" survives where it was kept, and whether the copy fits at
240px are not things any command available here can answer. The fourteen
states themselves are exercised, because `empty_state.rs` is gpui-free.

## Directions

- **Read what is actually in the base before editing.** Done.
  `app-gpui/src/disclaimer.rs` is present with `PLAY_TOOLTIP`,
  `PIPELINE_CAUTION`, `HEADLINE`, `SUMMARY` and `README_POINTER`, so #984 has
  landed and the empty state reads its constants rather than restating them.
  **#987 has *not* landed** — there is no `sync_avatar` or `avatar_source` in
  `workspace.rs`, and `render_chat_chip` is the pre-#987 version. Nothing here
  depends on it either way; it is recorded so the next rebase knows which side
  of that change this was written against.
- **Add rather than restructure in `workspace.rs`; list every function
  touched.** Done — see the section above.
- **Run `make app-check` and `make app-test`; the `Verification:` trailer
  reflects those and not the workspace suite; be explicit that rendering is
  unverified.** Done — the trailer names the app targets, and the paragraph
  above states the rendering gap in the specific terms the direction asks for.
- **Account for every review-feedback item, declines included.** Done above;
  there are no declines. Nothing in the feedback or the directions conflicted
  with the spec, so there is nothing to flag under "the later word wins".

## Deviations from the spec text

Three, all small and all in the spec's own direction:

- The spec describes `render_chat_empty_state` and `render_center`'s `!loaded`
  branch as rendering "the diagnosis". They render `explain_reachability`, a
  named function that reports the connection half of the walk and folds `Up`
  to `Connecting`. Both callers are in a state where no snapshot has landed,
  and walking the lists there would report "no repositories" off lists that
  have never been filled — the loudest possible wrong answer. The spec's own
  placement table already says only the three connection states can appear at
  either site; this is that table made into a function rather than into a
  condition at two render sites.
- The spec says "26 new tests". There are 35, for the same reasons the spec
  gives for the ones it names — the extra nine pin the hold ordering, the
  non-binding-update observation, a `None` status observing no holds,
  `Explanation::without`, `Action::label`, the standing set, a paused
  repository still counting as a repository, and the counting helpers.
- The spec's notes say "five of the fourteen carry a button". There are five
  *actions* across **six** situations: `Paused` and `Stopped` both offer
  `Action::Play`. They are the same question for the reader — "why is my
  queued work not moving?" — and withholding the button on one of them would
  make the pane's helpfulness turn on a distinction the reader did not make
  and cannot see from there. The spec's own `Action` enum has exactly five
  variants, which is the number that is load-bearing.

Verification: PASSED — `make app-check` (exit 0; the four warnings it prints are pre-existing, all in `bin/tasks-menubar/popup.rs`, which has its own module tree and does not include `empty_state`) and `CARGO_BUILD_JOBS=1 make app-test` (exit 0, 248 + 35 passing, 0 failed — 35 of the 248 are this change's). Also `cargo fmt --check` clean and `cargo clippy --all-targets` adding no new warnings (the five it reports are the same pre-existing `popup.rs` ones). `make test` is deliberately not cited: it compiles no part of `app-gpui`. **Rendering is unverified** — whether the compact line sits under the right header, whether three simultaneous placements collide on element ids, whether the catalog's own "Nothing open" survives where it was kept, and whether the copy fits at 240px all want `make app` on a Mac.
