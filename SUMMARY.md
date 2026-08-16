Global key bindings only fired while a text input had focus, because gpui
resolves a keystroke against the dispatch path running upward from the
*focused* node, and at rest nothing in the workspace's frame held focus. The
root div's `key_context("Workspace")` and its whole stack of `on_action`
handlers were therefore never on that path: gpui falls back to the *window*
root when the focused handle is absent from the rendered frame, and that node
carries no context, so ⌘R, ⌘1–⌘5, ⌘B, ⌘N, ⇧⌘D and Escape had nowhere to
dispatch on a freshly opened window. (Something *was* focused — `Workspace::new`
focused the chat composer — but the composer is only drawn in `Section::Chat`
while a window opens on Home, and a focus id missing from the frame is treated
exactly like `None`, which is also why clicking the Chat nav row appeared to
revive the shortcuts at random.)

This completes the fix on top of the focus handle the palette work already
introduced. The root div now carries `.id("workspace")`, `gpui::Role::Group`
and an `aria_label` beside its `track_focus` — a focusable element with no
element id logs `note_focus_without_node` and reaches no accessibility node, so
a screen reader announced the window instead of the workspace; the name follows
#861 rather than an index, since this id sits at the root of every descendant's
id path. The rule for where focus goes on a section switch is extracted as
`Section::focus_target() -> FocusTarget` — Chat gets its composer, every other
section gets the root — so it can be asserted as a pure function over view
state, per the app's testing convention (a `#[gpui::test]` would need gpui's
`test-support` feature, which the Makefile's stub-`.so` fallback for machines
without the xkbcommon dev packages cannot link). `clear_selection` now takes a
`&mut Window` and hands focus back to the root explicitly — the inspector it
puts away may have held the focused element, the review composer — and its two
call sites (Escape's `Dismiss` handler and the inspector's ✕) are threaded
accordingly. Finally, `Workspace::new` registers `cx.on_focus_lost` as the
general backstop: it is gpui's own answer to this, it is loop-safe, and it
covers everything the explicit hand-backs cannot (an input blurring itself on
Escape, a popup closing, anything added later that forgets). The two together
mean no uncovered case and no frame without a keyboard — `on_focus_lost` fires
only after the draw that removed the element, and its guard requires a
non-empty previous focus path, so it can never fire on the first frame, which
is why startup focus stays explicit.

Verification: PASSED — `make app-check` clean, `make app-test` 148 passed
(146 before, +2 new), `cargo clippy --all-targets` and `cargo fmt --check`
clean for `app-gpui`, and on the workspace `make test` 628 passed with clippy
and fmt clean. Not
verifiable from a Linux agent VM: that the shortcuts actually fire on screen.
The mechanism was verified against gpui 1.14.2's source (`Window::draw`'s
`focus_lost` branch, `Context::on_focus_lost`, `track_focus`, `Div::id`).
Someone on a Mac should run `make app` and check, on a freshly opened window
with no click anywhere: ⌘B, ⌘R, ⌘1–⌘5, ⌘N, ⇧⌘D, and Escape clearing a
selection — then the round trip: click into the chat composer, Escape (blurs),
Escape again (clears the inspector), and ⌘1 still working after each.
