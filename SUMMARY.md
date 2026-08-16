# Context menus on task rows

Right-clicking a row in Tasks, Queue or Activity now opens a menu of the verbs
that already existed as API calls: add to / remove from the queue, scout now,
approve a spec, open the review composer, request a build, close the issue as
completed or not planned, reopen it, and the three GitHub affordances (open,
copy number, copy URL). The menu is a pure function of row state — every row
offers the same twelve verbs in the same order, and the ones that cannot run
are greyed with the reason in the label (`Scout Now (already running)`),
because gpuikit's menu item has neither a tooltip nor a submenu to put one in.
The issue's central premise turned out to be wrong in a way that made this much
smaller than it looked: `rationale` is `Option` on every request body and is
required only of the orchestrator, so these are one-click verbs rather than a
prompt each. The one item that needs text, `Review Spec…`, reveals and focuses
the review composer that already lives in the inspector, so no modal or window
is introduced. Right-click also selects, since half the verbs act on a spec
only the inspector renders. Three safe verbs — add to queue, scout now, approve
— also get a new **Task** menu and key equivalents; nothing that closes an
issue is bound to a key anywhere.

The verb table and its greying live in `app-gpui/src/row_menu.rs` as
`entries(RowContext) -> Vec<RowEntry>`, deliberately shaped like
`menus::menus(MenuState)` so "which verbs exist, which are greyed, and why" is
testable without standing up a gpui `App`; its predicates mirror the store's
(`queue_task` wants `Backlog`, `dequeue_task` wants `Queued`,
`push_task_to_front` takes either, `create_build` takes only an approved spec).
The workspace turns that into gpuikit menu items, owns `perform_row_action`,
and re-derives legality when a verb is actually run — the menu greyed against
the state at open time, and the menu-bar path never saw a menu at all, so a
refusal is reported in the sidebar banner rather than silently swallowed.
Supporting changes: `Client::reopen_task`, which was routed and handled on the
server but had no client method (a close with no undo is the wrong shape for a
right-click menu); `close_task` / `reopen_task` / `build_spec` mutations and
`latest_queue_entry` / `github_url` projections in `state.rs`, the latter
extracted from `detail.rs` so the inspector's link and the menu's two GitHub
items cannot disagree.

Four things worth flagging for review. **The spec was written against the
pre-0.7 gpuikit**, whose `ContextMenuPopup` was public and whose menu state had
to be owned per row; 0.7 (adopted in #882) ships `context_menu(id, trigger)
.menu(builder)` instead, which owns its own state, dismissal and keyboard
navigation. Using it dissolves two of the spec's pitfalls — the deferred
`DismissEvent` that would eat a second menu, and the per-row `ContextMenuState`
— and it is why the workspace holds no popup entity. **Escape still needed
layering**, for a different reason than the spec gives: gpui dispatches key
bindings *before* key-down listeners, so `escape` reaches the workspace's
`Dismiss` handler before the menu's own escape handler exists in the path;
untouched, it would throw away the selection the menu is about and leave the
menu up. `Dismiss` now steps aside (`cx.propagate()`) while a row menu is open,
tracked by one bool that the next mouse-down anywhere clears. **⇧⌘Q is macOS's
Log Out**, so "Add to Queue" is bound to ⇧⌘U ("queue up") rather than the
mnemonic the spec proposed; ⇧⌘S and ⇧⌘A are as specified, and a test pins that
the collision cannot come back. The menu item renders its shortcut from the
keystroke `menus` binds rather than restating it, so the two cannot drift.
**Build-row actions are deliberately not delivered**: the issue asks for "open
its PR, copy the branch name, read its transcript", and there is no build row
anywhere in app-gpui to attach them to — builds surface only as Activity
sentences and as a clock on a *task* row. That is a Builds surface, and its own
issue.

Verified: `make app-test` — 72 passed / 0 failed (18 new: 13 in `row_menu`,
several exhaustive over all 224 combinations of row state; 5 in `menus`);
`cargo test -p tasks-client` — 11 + 4 passed / 0 failed (2 new, covering the
new `reopen_task` and that neither custodial write demands a rationale of the
human); `cargo test --workspace` green; clippy and fmt clean for both trees.
Not verified: nothing ran on a Mac, so no pixel has been seen — popup
positioning, the Task menu's key equivalents rendering, and the new bindings'
behaviour under the real AppKit menu bar all still need `make app`. That gap is
narrower than it was: this also lands `make app-check` / `make app-test`, which
typecheck and test the GUI on Linux with no display and no X11 dev packages
(`RUST_FONTCONFIG_DLOPEN=1` plus three empty stub `.so`s), documented in
CLAUDE.md — "the GUI can't be compiled by an agent" was costing every app-gpui
change its feedback loop, and it was not true.
