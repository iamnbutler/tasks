# app-gpui: archive Done tasks behind a toggle, and simplify the title bar

Four UI changes in `app-gpui`, landed together because three of them meet in
the title bar. **Done tasks are archived out of the Tasks list by default**,
behind a footer that always states its count — `"3 done · Show"` /
`"3 done · Hide"` — also reachable from `View ▸ Show Done Tasks` (a checkmark
over live state, not an item that renames itself) and `shift-cmd-d`. The rule
is one pure function, `archive(tasks, show_done) -> (Vec<&Task>, usize)`, with
four unit tests: default hides done, the toggle restores them *in the server's
order*, the count is stable across the toggle, and rejected is not archived.
It **drops rows and never sorts them**, so whatever ordering the server ships
survives the filter untouched, and it is a client-side view filter rather than
a `?hide_done=` on `GET /tasks` — that endpoint is shared with the
orchestrator, the briefing generator and `tasks status`, and one client's view
preference does not belong in it. The count is of what is *done*, not of what
is hidden, so the number does not move when the toggle does; hiding work is
only a problem when it is silent, and a footer that says "5 done · Show" and
reverses in one click is not. Rows are now keyed by `TaskId` rather than by
index, because with rows appearing and disappearing behind a toggle, index N is
a different task before and after and gpui treats a repeated id across frames
as the same node.

The other three changes leave **every section named exactly once**. The
sidebar's nav rows lose their text and become icons carrying a tooltip
(`"Tasks (⌘2)"`) plus a real accessible name — `role(Role::Tab)`,
`aria_label`, `aria_keyshortcuts`, and the role is load-bearing, since a node
reaches gpui's a11y tree only with *both* a global element id and a non-`None`
role. Their element ids move from bare integers to names (`"nav-tasks"`),
which is the collision class #861 is about. The title bar loses its chat button
(chat is still reached from the sidebar, `⌘5` and `View ▸ Chat`), gains
`owner/name` for the current project on the left as a label rather than a
button — the server offers no way to switch projects yet, and nothing renders
there before the first snapshot — and its static "tasks" in the centre becomes
the name of the section you are looking at. That last move is what pays for the
icon-only rows: `render_center` no longer draws a section header, the way Chat
already worked. `app-gpui/README.md` documents both the archive's three
properties and the new title-bar/nav contract. Verified with `cargo check
--all-targets`, `cargo clippy --all-targets`, `cargo fmt --check` and `cargo
test` (39 passed) in `app-gpui`, plus `cargo fmt --all --check` at the root;
nothing outside `app-gpui/` is touched. What a Linux build cannot see still
wants a look on a Mac via `make app`: the repo label beside the real traffic
lights, and how the icon-only rows read at the default 240px sidebar width.
