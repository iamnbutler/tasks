# tasks-gpui

The gpui port of the Tasks app (`tasks/app`, the Swift client). Built on
[gpui-unofficial](https://github.com/iamnbutler/gpui-unofficial) 1.14.2 and
[gpuikit](https://github.com/iamnbutler/gpuikit) 0.7, talking to the tasks
server through the workspace's own crates: `tasks-api` (shared wire types)
and `tasks-client` (typed blocking client + reconnecting SSE streams).

## Architecture

Follows Zed's workspace patterns (Zed is the source of truth for how
components are built and how data moves):

- **`state.rs`** — `AppState`, the server state entity (the Swift app's
  `AppModel`). One dedicated thread runs the reconnecting event stream and
  feeds a channel; a foreground task drains it. The sync contract is
  snapshot-on-`Connected` + refetch-on-event — events are invalidation
  signals, never folded into state. Snapshots and mutations run blocking
  `tasks-client` calls on gpui's background executor (loopback is
  sub-millisecond). Mutation failures surface the server's own error
  message in the sidebar banner; the server stays the authority on which
  transitions are legal.
- **`workspace.rs`** — the root view. Owns UI state (active section,
  per-sidebar open/width, selection, resize-drag tracking), observes
  `AppState`, and registers action handlers. Actions:
  `workspace::ToggleLeftDock` (`cmd-b`), `workspace::ToggleRightDock`
  (`cmd-r`) — Zed's defaults. Title bar carries play/pause (mode gates
  *new* work only) and refresh.
- **`menus.rs`** — the menu bar (App / File / Edit / View / Server /
  Window) and its actions. Handlers are global, not the workspace's,
  because Minimize / Zoom / Close act on whichever window is focused; the
  Edit menu dispatches gpuikit's own input actions so the items drive the
  same code path `cmd-c` already drove. Bindings are registered before
  `set_menus` — gpui reads shortcuts out of the keymap once, while building
  the bar. `menus()` is a pure function of `MenuState { serving, mode,
  busy }`; `sync()` reinstalls the bar only when that state actually
  changes, because `set_menus` leaks a boxed action per item on every
  rebuild.
- **`server.rs` / `server_window.rs`** — the Server menu: the one thing
  the app cannot do over HTTP, because a server cannot gracefully swap
  itself out through its own API. It shells out to the `tasks` binary
  (`reload` / `stop`) and reads the exit code as a verdict (3 busy, 4 drain
  timed out, 5 the swap did not land). A restart is minutes of staged work,
  so it always opens the Server window, which streams the child's output
  and shows `GET /status` + `GET /version` alongside it. A refusal grows
  buttons — *Wait, then restart* / *Restart anyway* — because a GUI can ask
  where the CLI could only refuse. Nothing in the menu takes a key
  equivalent: a one-keystroke server restart is the foot-gun this is trying
  not to build.
- **`about.rs`** — a singleton About window showing the version and commit
  `build.rs` stamped from git. `0.1.0` with `commit unknown` means the
  binary was built without git in reach.
- **`sections/`** — one file per sidebar section (`impl Workspace` blocks):
  Tasks (Linear-style rows → inspector), Queue (attention-ordered groups:
  Needs you / Running / Building / Up next / Ready to build, with live
  elapsed clocks — a clock reads as working, a spinner reads as hung),
  Home (briefing slots + needs-you rows), Activity (typed event sentences —
  exhaustive match, so a new event kind is a compile error), Chat
  (orchestrator conversation, read-only so far), and `detail.rs` (the
  inspector with per-state actions: queue/dequeue/scout, approve/reject).
- **`components/`** — presentation-only chrome. Components never reach into
  workspace state; they talk back by dispatching actions (title bar toggle
  buttons) or via callbacks the workspace hands them (sidebar resize).
  - `titlebar.rs` — 28px fixed height, 1px bottom border, whole bar is a
    `WindowControlArea::Drag` region, double-click zooms, content inset past
    the macOS traffic lights, left/center/right slots.
  - `sidebar.rs` — dockable panel with a drag-to-resize handle on its inner
    edge. The handle only reports drag-start; the workspace tracks movement
    at the window level because the pointer outruns the handle immediately.
  - `status_badge.rs` — the status→color vocabulary and capsule badges
    (colored text on a 15% wash), matching the Swift app.

Theming is gpuikit's system (`gpuikit::theme`): a `Themeable` trait contract
accessed via `cx.theme()`, initialized with `gpuikit::theme::init`. Icons
and the input stack (`InputState` + `text_area`) also come from gpuikit;
SVG assets are served by `Application::with_assets(gpuikit::assets())`.

## Dependencies

Everything comes from crates.io — there is no git dependency, and
`grep git+ Cargo.lock` returning nothing is the check that keeps it that way.

**`gpui` and `gpui_platform` are pinned exactly (`=1.14.2`), not with a
caret.** gpuikit asks for `^1.14.2`, so an exact pin unifies with it rather
than fighting it, and the day a gpuikit release needs 1.15 that becomes a
loud resolution error here instead of a silent bump under an app that has no
CI compiling it.

**`Cargo.lock` is committed**, which needs a `!/app-gpui/Cargo.lock` negation
in the root `.gitignore` (the blanket `Cargo.lock` line above it is why this
crate went without one for so long). app-gpui is deliberately not a workspace
member, so it resolves its own graph and ships as an app — it wants that graph
reproducible. The negation is load-bearing rather than cosmetic: `git add -f`
would commit the file once and then hide every later regeneration from
`git status`.

One thing to know before regenerating it. The gpui crates are published as a
lockstep family, and the core crate requires only `^1.14.2` of its own
support crates (`gpui-util`, `gpui-shared-string`, `collections`, …) — so a
plain `cargo generate-lockfile` will happily pair a 1.14.2 core with 1.15.0
support crates, a combination upstream never ships or tests. The lockfile
holds the whole `*-gpui-unofficial` family at 1.14.2 on purpose. If you
regenerate, put them back:

```sh
cargo update -p <crate>-gpui-unofficial --precise 1.14.2
```

## Running

```sh
cargo run          # from this directory
make app           # from the repo root: install ~/Applications/Tasks.app
make run           # …and (re)launch it
cargo test         # from this directory — app-gpui is not a workspace member,
                   # so `cargo test --workspace` at the root skips it
```

Connects to `http://127.0.0.1:$TASKS_SERVER_PORT` (default 4800 — the same
variable the server reads). Without a server it shows the connecting state
and retries every 3s. Builds without the Xcode Metal toolchain
(gpui-platform's `runtime_shaders` feature compiles shaders at runtime).

The Server menu needs to find two things a Dock-launched app cannot assume
are on `PATH`:

| var | | |
| --- | --- | --- |
| `TASKS_BIN` | — | the `tasks` binary to drive. Otherwise the `exe` the running server published in `<data dir>/tasks.pid`, otherwise `tasks` on the child's `PATH` (which is this process's, prefixed with `/opt/homebrew/bin`, `/usr/local/bin`, `~/.cargo/bin` — `tasks reload` starts with a `cargo build`, and launchd's `PATH` has no `cargo` on it) |
| `TASKS_REPO` | — | passed as `--repo` to the ops that build. `reload` finds the workspace from its cwd, else from the `tasks` binary's ancestors; a bundled app's cwd is `/`, so an installed binary outside a checkout needs this |
| `TASKS_DATA_DIR` | `~/.local/state/tasks-v2` | where the pidfile and `serve.log` live. Passed through to the child, so the app and the binary it drives always mean the same server |

`make app` assembles a `Tasks.app` bundle by hand around the release binary
(`Contents/MacOS/Tasks` plus `Info.plist.in` rendered by `sed`) and passes
`TASKS_GPUI_VERSION` / `TASKS_GPUI_COMMIT` explicitly, so an installed build
is stamped exactly. A bare `cargo run` lets `build.rs` probe git itself,
which can lag the `-dirty` suffix by a build.
