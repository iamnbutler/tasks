# tasks-gpui

The gpui port of the Tasks app (`tasks/app`, the Swift client). Built on
[gpui-unofficial](https://github.com/iamnbutler/gpui-unofficial) 1.14.2 and
[gpuikit](https://github.com/iamnbutler/gpuikit) 0.5.0.

## Architecture

Follows Zed's workspace patterns (Zed is the source of truth for how
components are built and how data moves):

- **`workspace.rs`** — the root view. Owns all UI state (active section,
  per-sidebar open/width, resize-drag tracking) and registers action
  handlers. Actions: `workspace::ToggleLeftDock` (`cmd-b`),
  `workspace::ToggleRightDock` (`cmd-r`, `cmd-alt-b`) — Zed's defaults.
- **`components/`** — presentation-only chrome. Components never reach into
  workspace state; they talk back by dispatching actions (title bar toggle
  buttons) or via callbacks the workspace hands them (sidebar resize).
  - `titlebar.rs` — 28px fixed height, 1px bottom border, whole bar is a
    `WindowControlArea::Drag` region, double-click zooms, content inset past
    the macOS traffic lights (Zed's 71px constant), left/center/right slots.
  - `sidebar.rs` — dockable panel with a drag-to-resize handle on its inner
    edge. The handle only reports drag-start; the workspace tracks movement
    at the window level because the pointer outruns the handle immediately.
Theming is gpuikit's system (`gpuikit::theme`): a `Themeable` trait contract
accessed via `cx.theme()`, initialized with `gpuikit::theme::init`. We add
to or override that theme as the port needs rather than keeping a parallel
palette. Icons and the input stack (`InputState` + `text_area`) also come
from gpuikit; SVG assets are served by
`Application::with_assets(gpuikit::assets())`.

## Running

```sh
cargo run
```

Builds without the Xcode Metal toolchain (gpui-platform's `runtime_shaders`
feature compiles shaders at runtime).
