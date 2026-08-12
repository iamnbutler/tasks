# tasks-gpui

The gpui port of the Tasks app (`tasks/app`, the Swift client). Built on
[gpui-unofficial](https://github.com/iamnbutler/gpui-unofficial) 1.14.2 and
[gpuikit](https://github.com/iamnbutler/gpuikit), talking to the tasks
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
  (`cmd-r`, `cmd-alt-b`) — Zed's defaults. Title bar carries play/pause
  (mode gates *new* work only) and refresh.
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

## Running

```sh
cargo run
```

Connects to `http://127.0.0.1:$TASKS_SERVER_PORT` (default 4800 — the same
variable the server reads). Without a server it shows the connecting state
and retries every 3s. Builds without the Xcode Metal toolchain
(gpui-platform's `runtime_shaders` feature compiles shaders at runtime).
