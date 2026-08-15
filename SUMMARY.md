# app-gpui: a Server menu

Adds a **Server** menu to `app-gpui`, between View and Window, for the one
thing the app cannot do over HTTP: a server cannot gracefully swap itself out
through its own API, so the menu shells out to the `tasks` binary (`reload` /
`stop`) instead. The binary is found from `$TASKS_BIN`, else the `exe` the
running server published in `<data dir>/tasks.pid`, else `tasks` on the
child's `PATH` — `$PATH` is deliberately last, because an app launched from
the Dock inherits launchd's minimal `PATH` and would find `tasks` only for
people who would have used a terminal anyway. The same trap is why the child
gets a `PATH` with `/opt/homebrew/bin`, `/usr/local/bin` and `~/.cargo/bin`
prepended: `tasks reload` starts with a `cargo build`. A restart is minutes of
staged work, so every op opens a **Server window** that streams the child's
stdout and stderr line by line (both pipes drained on their own threads before
`wait()` — a workspace build prints far more than a pipe buffer holds) and
then reports the verdict its exit code earned: 3 busy, 4 drain timed out, 5
the swap did not land, and 1 called out separately as "the build failed; the
running server was not touched". A refusal is the one outcome that grows
buttons — *Wait, then restart* / *Restart anyway* — because a GUI can ask
where the CLI could only refuse. The same window shows `GET /status` and `GET
/version` (polled every 5s while it is open, and only then: a stopped server
publishes no events to refresh on, and a stopped server is a state this window
exists to produce). Pipeline mode joins the menu as a checked radio group,
prefixed `Pipeline:` and kept in its own group, because it governs dispatch
rather than the process. Nothing in the menu takes a key equivalent — a
one-keystroke server restart is the foot-gun this is trying not to build.

Around that: `menus()` becomes a pure function of a `MenuState { serving,
mode, busy }` and `sync()` reinstalls the bar only on a real change, because
`set_menus` leaks a boxed action per item on every rebuild; the workspace
joins those three facts from `AppState` (connected, mode) and the new
`ServerControl` global (busy) via observers on both, and gains `SetModePlay` /
`SetModePause` / `SetModeStop` actions that the title-bar buttons now share.
While a run is in flight the sidebar banner reports the app's own disconnect
as the restart it is rather than as a transport error — checked above the
build warning, since a stale build is usually why someone hit restart in the
first place. Finally, the pidfile's shape and location move to a new
`tasks-api::paths` module (`data_dir`, `PidFile`, `pid_file`, `serve_log`,
`read_pid_file`): the server writes that record and two clients now read it,
so it gets one definition rather than a copy per client, on the same argument
that keeps the build stamp in one crate. `crates/tasks/src/pidfile.rs`
re-exports it and keeps only what a local process can answer (`write`,
`read_live`, `remove_if_ours`, `pid_alive`), and `run::data_dir()` delegates.

Tested: `cd app-gpui && cargo test` → 34 passed (exit-code→verdict mapping,
binary resolution including a stale pidfile record, `--repo` reaching only the
ops that build, the 5000-line both-pipes-drain case, and the menu's shape
under every combination of serving / mode / busy); root `cargo test
--workspace` green with `TASKS_TEST_BIN_DIR` / `VM_POOL_TEST_BIN_DIR` exported
as `make test` does, doctests included; `cargo clippy --all-targets` and
`cargo fmt` clean in both trees. One caveat on `make test`: 466 of 467 pass,
and `tasks::reload when_idle_waits_for_the_drain_and_restores_the_mode` hits
nextest's 60s timeout on this machine. It does so on the unmodified tree too
(checked with the crate changes stashed), so it is this box being slow rather
than anything here — `cargo test --workspace`, which has no per-test timeout,
runs it green. Not verified: the work was developed on
Linux, so the checkmarks, the greying, the `open -R` reveal, and the placement
of a new top-level menu between View and Window have not been seen on screen —
`make app` on a Mac is the only real exercise those can get. Everything
asserted about the menu here is asserted about `menus()`'s data.
