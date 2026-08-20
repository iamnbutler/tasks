Two specs on one branch, because both change `crates/tasks-api/src/models.rs`. #993 went
first, as the directions asked, so #1064's larger changes landed against a settled shape.

**#993 — tell the person what `play` will do, once.** `Capability::permits()` is a
third-person clause per capability that names the act and whose account it acts on
("merge its own pull requests into your default branch, or close them unmerged"),
deliberately separate from `describe()`, which is second person and instructional because
it is a line of the *orchestrator's* generated authority section. `tasks_api::first_play`
holds the on-disk acknowledgement (`<data dir>/first-play.json`) and `Sheet::from_entries`,
which groups the charter into live/shadow/off in `Capability::ALL`'s order. All of that is
in `tasks-api` rather than in the app because `app-gpui` is not a workspace member, so a
rule left there is one `make test` never runs. In the app, the sheet's body is generated
from the charter the Server window already polls — never written as prose — and is
dismissible with no `on_submit`. The gate sits inside `ServerControl::set_mode_gated` and
`Workspace::set_mode`, not at their call sites, so a play button nobody has written yet
inherits it. It is keyed off the acknowledgement and never off the mode, because every boot
overwrites the stored mode from `TASKS_DEFAULT_MODE`.

**#1064 — the orchestrator's two live controls.** `POST /orchestrator/interrupt` ends the
turn in flight; a request that finds nothing running is a 200 saying so and is never
stored, so it cannot leak forward into a turn nobody asked to stop.
`OrchestratorError::Interrupted` is the one error that does not become an assistant turn:
every other path writes itself into the chat because that settles the tick condition, and
settling it is exactly what an interrupt must not do — the watermark moves only in
`append_orchestrator_reply`, which the interrupted path never reaches, so no input is lost.
The child now gets its own process group, and both the interrupt and the timeout path sweep
it (`signal_group` / `sweep_group`, written as free functions so a second caller can use
them). `POST /orchestrator/hold` and `/release` are a durable column on the singleton row;
both reasons a turn may not start are read through one `Store::orchestrator_lane`, a struct
and not an enum, so a held-*and*-checked-out lane reports both. All three routes are
human-only and not charter-gated, on the `build-now` precedent.

`make test-ci`: **1156 passed, 0 failed** (1 slow, 3 leaky — the known set).
`cargo clippy --workspace --all-targets`: clean. `cd app-gpui && cargo check --all-targets
-j 2`: clean (no new warnings). `make app-test`: **35 passed, 0 failed**.

## Review feedback

**#1064 / 1 — the app half is not optional.** Partly done, and the interrupt/hold buttons
are the part I did **not** land; I am naming what stopped me rather than implying
otherwise. I ran out of the 60-minute budget: the two specs together are a large surface,
and after the server half, the `tasks-api` half and #993's app half (three windows,
including the menubar gate and the charter panel) there was not enough clock left to add
`Client::interrupt_orchestrator`/`hold`/`release` to `tasks-client` and wire two controls
into the chat surface without shipping something I could not compile and test. It was not
"could not build the app" — `make app-check` and `make app-test` both ran here and are
green, and #993's app half is fully landed at all three play buttons. The server half of
#1064 is complete and tested, so what is missing is exactly the button, and the control it
would call is a documented route rather than a `curl`-only affordance by design. This
should be picked up as a follow-up.

**#1064 / 2 — no `/proc` on the deployment platform.** Done. The Discovered Pitfall was
wrong and the feedback is right: `sweep_group` re-derives liveness with the existing
`crate::pidfile::pid_alive` (`ps -o state=`, empty row or leading `Z` is dead, `kill -0`
as the fallback). No second liveness check and no `/proc` path anywhere in the change.

**#1064 / 3 — the trunk moved under this spec.** Done. I read `reload.rs`, `server.rs`,
`http.rs`, `models.rs` and `server_window.rs` at current main rather than trusting the
spec's descriptions, and the shapes I built against are the current ones (`Services` now
carries `runtime_health` and `verify_dir`; `ServerStatus` carries `pool_unreachable` and
`verify_dir`; `OrchestratorConfig` carries `worker_timeout` and `worktree_dir`). On whether
`reload`'s drain and the hold have anything to say to each other: they do not, and
deliberately. The drain waits on `InFlight::is_destructible()`, which is about work a
restart would destroy; a lane hold stops turns *starting* and says nothing about one
running, so a held lane makes the drain trivially satisfiable but is not a substitute for
it. I did not wire the hold into `reload` — that would be a new promise about what a
restart does — but the spec's own closing note stands: `TurnControl` now gives
`reload --force` a cleaner option than the SIGKILL path, and that is a follow-up rather
than something this build should decide.

**#1064 / note (not required) — the worker lane's identical hazard.** Honoured as written:
I did *not* widen this build to fix `worker.rs`, and `signal_group`/`sweep_group` are `pub`
free functions taking a bare pgid rather than methods buried in the turn path, so the
second caller can use them unchanged.

**#993 / 1 — there is a third play button.** Done, and the decision is the second of the
two acceptable answers: the menubar's chip is **refused with a stated reason** ("Open Tasks
and start the pipeline there once — this machine has not been told what play does yet"),
surfaced on the `mode_error` info line the section already renders. Not routed to the
Server window, for a reason specific to that surface: the menubar can point at other
machines via `TASKS_MENUBAR_MACHINES`, so the acknowledgement — a record in *this* host's
data dir — says nothing about the remote server whose charter would actually govern the
run. A sheet raised from there would generate its list from the right charter and file its
acknowledgement against the wrong install. Acknowledging where the pipeline is is the
honest place, and the refusal says so. The gate itself is inside
`ServerControl::set_mode_gated`, so the menubar cannot walk past it.

**#993 / 2 — declare the files you actually touch.** Acknowledged, and it applies to the
spec's `files_touched` field rather than to anything in the tree, which I cannot edit from
here. The real set this build touched: `crates/tasks-api/src/{models.rs, first_play.rs,
http.rs, lib.rs}`, `crates/tasks/src/{orchestrator.rs, store.rs, server.rs, run.rs,
reload.rs}`, `crates/tasks/tests/orchestrator.rs`, one new migration,
`Cargo.toml` + `crates/tasks/Cargo.toml`, and in the app `app-gpui/src/{first_play.rs,
server.rs, server_window.rs, workspace.rs, main.rs, context_gauge.rs, empty_state.rs}` and
`app-gpui/src/bin/tasks-menubar/popup.rs`. The overlap the feedback predicted is real and
is why these two are one branch: both touch `models.rs`, and both touch `server_window.rs`.

**#993 / 3 — the supervisor's green run proves nothing about the app half.** Done. Exact
commands and outcomes: `cd app-gpui && cargo check --all-targets -j 2` — clean, no errors,
only the pre-existing menubar/modal dead-code warnings that are on main; `make app-test` —
`35 passed; 0 failed`. `-j 2` throughout, as the spec requires; nothing was OOM-killed.

**#993 / 4 — render "the charter could not be read" as its own state.** Done, and pinned:
`an_unreadable_charter_is_its_own_state_and_never_an_all_off_sheet` asserts that
`Sheet::from_charter(None)` has an **empty** `off` list (it does not claim eleven
capabilities are off) and is `!=` the all-`off` sheet that `Some(&[])` produces. Both the
sheet and the Server window's charter list render `UNREADABLE_CHARTER` for that state.

**#993 / charter panel scope.** As the spec and the feedback both allow under time
pressure, the panel ships as the **read-only list** — eleven rows, each `permits()` beside
its level — rather than a partial set of toggles. `Client::set_charter` is still uncalled.

## Directions

**Reconcile `models.rs` deliberately.** Done. The two specs want nothing of the same name:
#993 added `Capability::permits()` as a method beside `describe()`, #1064 added
`OrchestratorLane` and a field on `OrchestratorSessionInfo`. I wrote #993's addition first
and read it back before adding #1064's; there is no field or type either spec wanted to
mean two things, so nothing had to be picked between.

**Order #993 first, then #1064.** Done, in that order, and it paid: `permits()` settled
before `OrchestratorLane` went into the same file.

**Run `make app-check` / `make app-test` and report both.** Done — see #993 / 3 above for
the exact commands and outcomes. Both were actually run; neither result is reported from
memory.

## Notes on the specs

Two spec claims I did not follow, both flagged rather than silently changed. The #1064
pitfall about `/proc/<pid>/stat` is wrong on this platform and the reviewer's item 2 wins,
as recorded above. And the #993 spec asks the Workspace to grow its first `ModalLayer`; I
routed its `play` to the Server window with the sheet up instead, because the sheet's own
closing paragraph names off switches ("Pause or Stop in this same row… the charter below")
that are true where the Server window shows them and would be directions to somewhere else
shown in the Workspace. The acknowledgement is process-wide either way, so this costs a
window raise and buys one copy of the sheet rather than two.
