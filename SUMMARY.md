# The inspector's toggle wins over selection — a dismissed right sidebar stays dismissed

The right sidebar's visibility was one `bool` written by two parties who meant
different things by it: the title-bar toggle (and ⌘B/⌘R) meant "the user is
talking about this panel", while `select_task` meant "show me this task".
Because the second runs on every row click, dismissing the inspector while a
row was selected — the common case, since the panel is usually open *because*
something is selected — lasted only until the next click, and the toggle read
as broken (#899). This keeps both writers but stops them sharing one fact:
`SidebarState` gains a private `dismissed` flag alongside `open`, and four
named verbs that say which statement is being made. `toggle()` flips the panel
and records a dismissal when it closes; `force_open()` opens regardless and
clears one; `reveal()` is content asking to be seen, honoured only if the user
hasn't dismissed the panel; `hide()` closes it without recording anything.
Selecting a row while the inspector is dismissed now changes what the panel
*holds* without forcing it visible, and the toggle brings it back with the
last-picked row already in it.

The four call sites in `workspace.rs` each become a decision about what that
path means: `toggle_sidebar` → `toggle()` (unchanged behaviour, and now the
only place a dismissal is recorded); `select_task` → `reveal()` (the fix — all
three ordinary row-click paths and the row context menu reach the inspector
through here, so they all inherit it); `begin_review` → `force_open()`, the one
selection path that overrides a dismissal, because it focuses the review
composer and a focused field inside a hidden panel eats keystrokes with nothing
on screen to explain where they went; and `clear_selection` → `hide()`, so
escape and the inspector's own ✕ let the panel follow its content out without
meaning "and don't come back". `SidebarState::open` is now private with an
`is_open()` reader, which is the load-bearing part: `workspace.rs` is a
different module, so a future `sidebar.open = true` side effect is a compile
error rather than a re-run of this bug. `dismissed` deliberately starts `false`
regardless of the `open` argument — the inspector starts closed as a default,
not as a decision the user made, so the first row click still opens it.

Eight tests in `components::sidebar::tests` cover the literal #899 sequence,
that a dismissal outlasts repeated selections, that toggling back open forgets
it, that `force_open` clears it, that `hide` is not a dismissal, and that a
panel which starts open (the left sidebar's shape) dismisses on its first
toggle; a ninth covers the pre-existing `set_width` clamp, which had no test.
The left sidebar was never affected — `left_sidebar.open` has exactly one
writer — so nothing there changes. `make app-check` and `make app-test` are
green (111 tests), as is `cargo clippy` under the Makefile's stub environment;
`app-gpui` is not a workspace member, but `make test-ci` was run anyway and is
unaffected. Not verified visually: app-gpui compiles and unit-tests in a Linux
VM but only runs on macOS, so the repro and the fix are reasoned from the code
paths.
