# tasks-gpui

The gpui port of the Tasks app (`tasks/app`, the Swift client). Built on
[gpui-unofficial](https://github.com/iamnbutler/gpui-unofficial) 1.14.2 and
[gpuikit](https://github.com/iamnbutler/gpuikit) 0.7 (pinned to a git rev — see
Dependencies), talking to the tasks server through the workspace's own crates:
`tasks-api` (shared wire types) and `tasks-client` (typed blocking client +
reconnecting SSE streams).

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
  per-sidebar open/width, selection, resize-drag tracking, the Tasks
  archive toggle), observes `AppState`, and registers action handlers.
  Actions: `workspace::ToggleLeftDock` (`cmd-b`),
  `workspace::ToggleRightDock` (`cmd-r`) — Zed's defaults —
  `workspace::ToggleShowDone` (`shift-cmd-d`), and `⌘1`–`⌘4` for the
  sections.

  **Every section is named exactly once.** The title bar's centre carries
  the name of the section you are looking at; the sidebar's nav rows are
  icons, and `render_center` draws no header. The rows carry the word as a
  tooltip (`"Tasks (⌘1)"`) and as an accessible name — `role(Role::Tab)` +
  `aria_label` + `aria_keyshortcuts`, and the role is load-bearing: a node
  reaches the a11y tree only with *both* a global element id and a
  non-`None` role, so an `aria_label` on a roleless div is dropped
  silently. (gpuikit's `IconButton` sets an id and a tooltip but no role or
  label, so the title bar's icon buttons still have no accessible name.
  That is upstream's to fix.)

  The title bar's left slot names the current project (`owner/name`) as a
  label, not a button — the server offers no way to switch projects yet,
  and a control that looks live and isn't is worse than a word. Nothing
  renders there before the first snapshot. The right slot carries
  play/pause (mode gates *new* work only) and refresh; chat is reached from
  the sidebar, `⌘4` or `View ▸ Chat` rather than from the chrome.
- **`menus.rs`** — the menu bar (App / File / Edit / View / Server /
  Window) and its actions. Handlers are global, not the workspace's,
  because Minimize / Zoom / Close act on whichever window is focused; the
  Edit menu dispatches gpuikit's own input actions so the items drive the
  same code path `cmd-c` already drove. Bindings are registered before
  `set_menus` — gpui reads shortcuts out of the keymap once, while building
  the bar. `menus()` is a pure function of `MenuState { serving, mode,
  busy }`; `sync()` reinstalls the bar only when that state actually
  changes, because `set_menus` leaks a boxed action per item on every
  rebuild. `MenuState` also carries `show_done`, so `View ▸ Show Done
  Tasks` is a checkmark over live state rather than an item that renames
  itself.
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
  Tasks (Linear-style rows → inspector, with done tasks archived behind a
  toggle — see below), Queue (attention-ordered groups:
  Needs you / Running / Building / Up next / Ready to build, with live
  elapsed clocks — a clock reads as working, a spinner reads as hung),
  Activity (typed event sentences — exhaustive match, so a new event kind is
  a compile error), Chat
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

## The Tasks archive

`GET /tasks` already drops closed-issue terminal work, so what piles up in
the list is terminal work whose GitHub issue is still open. The Tasks
section archives `done` out of the list by default, behind a footer that
always states the count: `"3 done · Show"` / `"3 done · Hide"`, also
`View ▸ Show Done Tasks` and `shift-cmd-d`.

Three properties, and each is deliberate:

- **It is a client-side view filter, not a query parameter.** `GET /tasks`
  is shared with the orchestrator and `tasks status`; one client's view
  preference does not belong in it. The rows are
  a few hundred at most, so filtering locally is cheaper than a refetch per
  toggle, and the server stays the single authority on which tasks exist
  and in what order.
- **It drops rows, never sorts them** (`archive()` in `sections/tasks.rs`,
  a pure function with unit tests). Whatever ordering the server ships
  survives the filter untouched.
- **The count is of what is *done*, not of what is hidden**, so the number
  does not move when the toggle does. Hiding work is only a problem when it
  is silent; a footer that says "5 done · Show" and reverses in one click
  is not.

`Rejected` is not archived — the archive is about work that finished, and
rejected work is one predicate away, so changing that should be a decision
rather than a refactor. `show_done` is per-window and resets on relaunch;
the app has no settings store, and a filter that states its own position
does not need one.

Theming is gpuikit's system (`gpuikit::theme`): a `Themeable` trait contract
accessed via `cx.theme()`, initialized with `gpuikit::theme::init`. Icons
and the input stack (`InputState` + `text_area`) also come from gpuikit;
SVG assets are served by `Application::with_assets(gpuikit::assets())`.

## Dependencies

Everything comes from crates.io **except gpuikit**, which is pinned to a git
rev. `grep git+ Cargo.lock` should return exactly one line, naming gpuikit and
nothing else; a second one is a regression.

That is a deliberate exception to what this section used to promise, and it is
temporary. The chat's streaming markdown needs two things that exist only on
gpuikit `main` (added together in
[#134](https://github.com/iamnbutler/gpuikit/pull/134), commit `dde1e9c`):
`Markdown::append`, which keeps a selection across a delta where `set_source`
drops it, and the `stitch` feature, which closes the inline syntax a
half-streamed document leaves open. `main`'s own `Cargo.toml` still reads
`version = "0.7.0"` — the version already on crates.io — so there is no release
to depend on and no version to ask for. A `rev`, not a `branch`, so the build
stays reproducible.

Two consequences worth knowing:

- **The effective MSRV rises to 1.95**, via `stitch`'s `mdstitch` dependency.
  gpuikit's own `rust-version = "1.75"` is stale either way (it uses async
  closures and needs 1.85). Accepted: this crate has no MSRV policy of its own
  and nothing else in the repo builds it.
- **The revert is one line.** Dropping `features = ["stitch"]` gives up only
  the flicker fix and nothing else; `components/markdown.rs` is unaffected
  either way, and its `partial_markdown_preprocessing_is_compiled_in` test is
  what makes that a decision rather than an accident. Going all the way back is
  `gpuikit = "0.7"` plus swapping the `Update::Append` arm in `MarkdownCache`
  for a `set_source`.

The end state is `gpuikit = { version = "0.8", features = ["stitch"] }` the day
upstream cuts a release — worth asking for, alongside the stale `rust-version`.

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

On Linux the build needs five packages — `pkg-config libfontconfig-dev
libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev` — which the Scout and
Builder images install, so a bare `cargo test` works in an agent VM off a
current image. Without them, `make app-check` / `make app-test` from the repo
root still compile and test the crate (they set `RUST_FONTCONFIG_DLOPEN=1` and
link against empty stub `.so`s); the Makefile picks whichever path applies.
Neither needs a display. Running the app is still macOS only.

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

### Optimizing dependencies in dev builds — a local opt-in

`cargo run` from this directory builds gpui and wgpu unoptimized, and a debug
gpui is slow enough that the app's own frame rate is the thing you notice. The
usual fix is `opt-level = 2` for dependencies only. It is **not** in
`Cargo.toml`, deliberately, and this is where to turn it on if you want it.

**Nothing upstream can turn it on for you.** Cargo reads profiles only from
the root manifest of the build actually being run, and silently ignores a
profile in a *dependency's* manifest. app-gpui is deliberately not a member of
the `tasks` workspace, so it is its own root: no profile in the workspace root
reaches it, and neither does gpuikit's own stanza
([iamnbutler/gpuikit#140](https://github.com/iamnbutler/gpuikit/pull/140)) —
gpuikit's changelog will tell you the problem is fixed, and here it is not.

The opt-in, in `app-gpui/.cargo/config.toml` (gitignored, so it stays yours):

```toml
[profile.dev.package."*"]
opt-level = 2
```

Cargo discovers config from the cwd upward plus `$CARGO_HOME`, so that file
covers every cargo command run from inside `app-gpui/` — including `make
app-check` and `make app-test`, which `cd` here first. Putting the same stanza
in `~/.cargo/config.toml` instead covers every Rust project on the machine,
the `tasks` workspace included, which is usually not what you want.

Measured on an aarch64 Linux VM, 4 cpus / 6 GB, `CARGO_BUILD_JOBS=2`:

| | default | opt-in |
| --- | --- | --- |
| cold `cargo build` | 182s | 644s |
| rebuild after `touch src/main.rs` | 21–25s | 14–18s |
| `target/debug/tasks-gpui` | 592 MB | 761 MB |
| `target/` | 3.8G | 8.3G (both artifact sets) |

Every effect that was measured is a cost. The edit loop does not move — the
optimized side came in a few seconds faster across three samples each way, and
six samples on a shared VM do not support claiming a win in either direction.
The benefit is real but unmeasured, and it accrues to one person running
`cargo run` on a Mac, while the 3.5× cold build is paid by every fresh clone,
including every agent VM that touches this crate. Defaulting to the certain
cost for the unmeasured win is the wrong way round; opting in on your own
machine is not.

Three things to know before you do:

- **The first build after adding *or* removing the file rebuilds the whole
  graph** — the profile is part of Cargo's fingerprint. Switching back is
  cheap (~30s) only while both artifact sets are still in `target/`; a `cargo
  clean` collects the losing one and the next switch is another full build.
- **Peak rustc memory rises**, to ~2.6G of 6G free at two jobs. On a small
  machine, drop `CARGO_BUILD_JOBS` before concluding the build is broken.
- **`debug = "line-tables-only"` is a separate knob.** It cuts the debug
  binary hard (761 MB → 279 MB was observed on a scratch build; its link-time
  effect was never measured cleanly) and would confound the comparison
  entirely if added at the same time. Its own question, its own issue.

**Measuring the frame-rate side, which is the open question.** It needs a Mac,
and it needs `cargo run` on both sides — **not `make app` or `make run`**,
which are `cargo build --release` and cannot see a `[profile.dev]` setting at
all. A before/after that uses them measures a binary the setting never touched
and reports "no difference" for entirely the wrong reason, which is the single
most likely way this gets answered wrongly. Exercise the paths that actually
push frames: scrolling a long Tasks list, a streaming chat reply, dragging a
sidebar resize handle. Then record what you saw on
[#922](https://github.com/iamnbutler/tasks/issues/922) — including "no
difference", which is the answer that settles it.

For the record, this is an explicit disagreement with gpuikit rather than an
oversight: gpuikit's stanza buys its `cargo run --example showcase` loop the
same unmeasured thing, and two crates being consistently unmeasured is not
better than one being deliberate. If the Mac measurement comes back large,
committing the stanza is a one-line follow-up made on evidence, and it goes
where the comment in `Cargo.toml` says it would have gone.
