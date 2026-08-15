# app-gpui: menu bar + git-stamped About window

`app-gpui` shipped without a `cx.set_menus(…)` call, so it had no menu bar at
all — no App menu (and therefore no working `cmd-q`, `cmd-h`, or About), no
File, Edit, or Window menu, and its two real commands (the dock toggles) were
reachable only by a keystroke you had to already know. The Swift client got
all of that free from AppKit, and removing it in #814 took the `make app`
target that stamped a git version into the build along with it. This adds
`app-gpui/src/menus.rs` — four menus (App / File / Edit / Window), their
actions, global handlers, and key bindings — plus `app-gpui/src/about.rs`, a
singleton About window showing the version and commit, and `app-gpui/build.rs`,
which stamps both from git so even a bare `cargo run` is honest about what it
is. `make app` and `make run` are restored at the repo root, now assembling a
`Tasks.app` bundle by hand around the cargo-built release binary
(`Contents/MacOS/Tasks` plus `Info.plist` rendered from the new
`app-gpui/Info.plist.in`); the bundle identifier and deployment target carry
over from the Swift target, so an install replaces the old app in place.

Two decisions are worth calling out. The Edit menu dispatches gpuikit's *own*
input actions (`gpuikit::input::bindings::{Undo, Redo, Cut, Copy, Paste,
SelectAll}`) rather than declaring new ones, so the items drive exactly the
code path `cmd-c` already drove and every present and future `InputState` — the
chat composer, the spec-review composer — is covered without opting in; they
use `MenuItem::os_action` so gpui maps them to the AppKit selectors and the key
equivalents keep working inside system panels. And the menu handlers are global
(`App::on_action`) rather than hung off the workspace root, because
Minimize/Zoom/Close act on whichever window is focused — including the About
window, which has no workspace behind it — while Hide and Quit aren't a
window's business at all. A consequence worth knowing rather than filing as a
bug: menu items grey themselves out via `App::is_action_available`, so the
dock toggles correctly grey out while About is focused, and Cut/Copy/Paste grey
out with no text input focused. Ordering in `main()` is load-bearing — gpui
reads shortcuts out of the keymap once, while building the bar, so
`menus::init` (handlers and bindings) runs before `menus::set`. Alongside:
window opening moved into `open_workspace(cx)`, which keeps its handle in a
`Global` and is registered with `Application::on_reopen` so a Dock click brings
the app back after `cmd-w` instead of stranding it; and the redundant
`cmd-alt-b` binding for `ToggleRightDock` is gone, since a menu item can only
display one shortcut and the menu is now the discovery surface.

**Verification.** `menus.rs` and `about.rs` carry seven unit tests (menu
structure as pure data — the four top-level names in order, About first and
Quit last, the dock toggles present in Window, the Edit menu's six commands,
the literal `"Window"` name that gpui hands to AppKit as the windows menu, and
a non-empty build stamp); `cargo fmt --check`, `cargo clippy --all-targets`,
`cargo build`, and `cargo test` are green in `app-gpui`, and the root
`cargo test --workspace` is green at 317 passed. The built binary was checked
to actually carry the stamp (`0.1.497`, `cf08fdd-dirty`) and the action names
(`tasks::Quit`, `workspace::ToggleRightDock`, `input::SelectAll`), and the
bundle assembly was rehearsed into a temp directory with the resulting plist
parsed back. Note that `app-gpui` is not a workspace member and has its own
`target/`, so `cargo test --workspace` at the root does *not* run these tests —
they need `cargo test` inside `app-gpui/`. **This ran on aarch64 Linux against
the Linux gpui backend, which compiles the same code (nothing here is behind
`cfg(target_os)`), so the macOS behaviour — that the bar appears, that `cmd-q`
fires, that Cut/Copy/Paste reach a focused input through the responder chain,
that `make app` produces a launchable bundle — was reasoned from the gpui-macos
source, not observed. Worth one `make run` and a click through all four menus
before merging.**

Two smaller notes. The issue mentions a `workspace::NewIssue` action and a
`cmd-n` New Issue window from commit `c6d65c9`; neither exists in this repo, so
per the issue's own "no menu item for an action that doesn't exist yet" the
File menu ships with Close Window only, with a comment marking exactly where
`New Issue` goes. And two unrelated files (`sections/activity.rs`, `state.rs`)
picked up incidental `cargo fmt` hunks — they were never formatted, and both
style edition 2021 and 2024 agree on the result, so this won't ping-pong.
