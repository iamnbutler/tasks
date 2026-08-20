You are a Builder in the Double Diamond architecture.

You are implementing 2 approved spec(s). Verify a spec's claims against the code in front of you; where a spec has a Scout behind it, trust its pitfalls.

## Spec 1 of 2: Interrupt the turn, hold the lane: the orchestrator gets the two live controls every other run already has (#1064)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: Interrupt the turn, hold the lane — the orchestrator's two live controls

### Summary
The orchestrator turn is the only run in the system with no live control. Two additions
give the human the two basic asks. `POST /orchestrator/interrupt` ends the turn in
flight — the CC session survives, no input is lost (by construction, not by care), and
the accounting lands on the feed with the actor and an optional rationale; a request
with no turn in flight is a **200 saying so**, not a 4xx. `POST /orchestrator/hold` and
`POST /orchestrator/release` stop new turns starting, as a durable flag on the
`orchestrator` singleton row beside `checked_out_at`. Both reasons a turn may not start
are read through **one predicate** — `Store::orchestrator_lane() -> OrchestratorLane`
with `may_tick()` — so the loop and every reporting surface cannot drift into two
notions of "quiet", while both meanings stay distinct and both render. All three routes
are human-only and **not charter-gated**: they decide whether the judge convenes at all,
which is the `build-now` / `POST /projects` category.

### Implementation Approach

**Wire types (`crates/tasks-api`)**
- `models::OrchestratorLane { held: bool, held_at: Option<DateTime<Utc>>, checked_out: bool }`
  with `may_tick()` and `describe()`. **A struct, not an enum**: the two are not
  alternatives — a human can hold the lane *and* have the session checked out — and an
  enum forces a precedence that silently discards one, leaving a reader to undo the wrong
  thing. `describe()` returns both when both hold.
- `OrchestratorSessionInfo` gains `#[serde(default)] lane: OrchestratorLane`. The
  existing `checked_out` bool stays (existing clients read it) and is filled from the
  same read, so they cannot disagree.
- `http::ServerStatus` gains `#[serde(default)] orchestrator_lane: Option<OrchestratorLane>`,
  **present only when the lane is not open** — the hold shape, not the `verify_dir`
  shape: a held lane is an exception a reader must not miss, and a standing "lane open"
  row is one a reader learns to skip. `#[serde(default)]` for the standing reload-skew
  reason (`reload` reads `/status` off the *older* server).
- `http::LaneControlRequest { rationale: Option<String> }` and
  `http::InterruptResponse { interrupted, detail, lane }`. The rationale is **optional**,
  unlike a `decisions` rationale: nothing here is charter-gated, so there is no ledger row
  to leave unreviewable — it is addressed to whoever reads the feed later.

**Migration** — `<UTC>_orchestrator_hold.sql`: `ALTER TABLE orchestrator ADD COLUMN held_at TEXT`.
Durable on purpose, and deliberately *not* the mode's shape: `TASKS_DEFAULT_MODE`
overwrites the stored mode at every boot precisely because dispatch should come back
quiet, whereas a lane hold is a standing decision about the judge and a restart that
silently resumed turns would leave a control that looks applied and is not.

**Store (`crates/tasks/src/store.rs`)**
- `orchestrator_session_info` selects `o.held_at` and fills `lane`. The hold *is* the
  column's presence; the instant is parsed leniently and is decoration for a banner that
  ages it, so an unreadable timestamp can never become "not held".
- `orchestrator_lane()` is the one read. `orchestrator_checked_out()` is **rewired onto
  it** rather than left as a second hand-written query — that is how two readers come to
  disagree, and this question now has two readers.
- `orchestrator_hold()` is `UPDATE … SET held_at = ? WHERE id = 1 AND held_at IS NULL`:
  idempotent, and re-holding does not move "held since" — that is when the lane went
  quiet, not when somebody last said so. `orchestrator_release_hold()` is unconditional.

**Interrupt (`crates/tasks/src/orchestrator.rs`)**
- `TurnControl` — a `Mutex<Option<watch::Sender<Option<Interruption>>>>` slot. The turn
  takes the slot when it starts (`arm()`) and a `TurnGuard` clears it on **every** exit
  path including a panic; `interrupt()` decides under the lock, so a request that arrives
  with nothing running finds no slot, is answered honestly, and is **never stored** —
  which is what makes "a request cannot leak into the next turn" structural rather than
  careful. Armed in `tick()` around `run_agent`, not per invocation: a turn whose resume
  fails invokes twice, and a window between them answering "no turn in flight" would be a
  lie.
- **An in-process signal, not a `cancellations` row — decided, with the reason.**
  `crate::cancel`'s row exists for a premise that is false here: *"the process taking the
  request need not be the one following the run"*. A scout or a build lives in a VM and
  may be picked back up by `resume_in_flight` in a later process; an orchestrator turn is
  a local child of this one server, dies with it, and can never be reattached — the
  process taking the request **is** the process running the turn. (Workers are local
  children too and *do* use a row; the difference is that a worker has a durable id. A
  turn has none, `cancellations` is keyed `(run_kind, run_id)`, and the only id available
  is the singleton's — so a request landing a moment late would sit on record and stop a
  *later* turn nobody asked to stop.) What the row buys, an audit shape, is kept as an
  `EventPayload::Note` carrying the actor and the rationale.
- `OrchestratorError::Interrupted(Interruption)` is the **one** error that does not
  become an assistant turn. Every other error path deliberately writes itself into the
  chat because that settles the tick condition — and settling the tick condition is
  exactly what an interrupt must not do. `tick()` returns early into
  `conclude_interrupted`, which clears the turn marker, appends the Note, publishes
  `OrchestratorFeedEvent::Done` and returns `Ok(false)`. `answered_through` moves only in
  `append_orchestrator_reply`, which is never reached.
- The Note is written by the turn, **not** by the route: the select is `biased` with the
  work first (`cancel::bounded`'s rule — an outcome in hand is never discarded for a
  request that arrived in the same poll), so a note written at the request would
  sometimes claim a stop that never happened.
- **Process group, not the child.** The child gets `.process_group(0)`, its pid is read
  before it is moved into the read future, and the interrupt path sends `SIGTERM` to
  `-pgid`, drops the future (which takes `claude` itself via `kill_on_drop`), `abort()`s
  the stderr reader rather than awaiting an EOF that will never come, and sweeps with
  `SIGKILL`. `kill_on_drop` takes the agent alone and its bash children survive holding
  the pipes we are reading — `run_script`'s hazard one surface over, same answer. The
  **timeout path gets the same sweep** (`inspect_err`), so a timed-out turn cannot leave
  a `cargo` behind holding the warm build directory. `signal_group` is best-effort by
  design: every failure is `ESRCH` (already gone) or a permission error a retry cannot
  fix, and an interrupt that errored *after* freeing the lane would be worse than the
  hang it replaced. Adds `libc = "0.2"` to the workspace (one call).

**Loop (`crates/tasks/src/run.rs`)**
- `orchestrator_loop` takes `Arc<TurnControl>` and gates on `lane.may_tick()` instead of
  `orchestrator_checked_out()`.
- **The reclaim keys on `checked_out` alone, not on the lane.** Held means this loop is
  precisely what is *not* running, so a hold must not stop bounding a directory that
  reached 51 GB unattended (#1010); checked out is the one case the "nothing else starts
  a process in here" argument does not cover.
- The control is created in `serve`, handed to the loop and to `Services`.

**Routes (`crates/tasks/src/server.rs`)** — inside the private `fn routes`, so the
loopback layer covers them by construction.
- `Services.turn_control: Option<Arc<TurnControl>>`, absent on the health-record terms;
  absent is reported as "no turn in flight", **not** a 503 — the caller's question is
  whether the lane is quiet, and it is.
- `require_human_for_lane` refuses any `X-Tasks-Actor` with 403 and the standard message
  shape. Not charter-gated: there is no row that could be set to `off`, and if lane
  control is ever wanted as autonomy it wants its own named capability and its own issue.
- Hold/release append one `Note` each on the **edge only** (the `POST /mode` shape), with
  the rationale appended to a statement of what happened — one event rather than a bare
  rationale, which would be unreadable a week later.

**Reporting** — `reload::render_orchestrator_lane` (`tasks status`), silent when the lane
is open, printing both reasons and naming each one's discharge. `GET /status` carries the
lane. `OrchestratorSessionInfo.lane` is what the app reads.

**App (`app-gpui`, not implemented here)** — an Interrupt button on the chat surface,
enabled off `in_flight.orchestrator` (the "owed turn" line it already polls), and a
Hold/Release toggle whose *held* state renders as a standing banner on the chat rather
than a pressed button. **No confirms on either**: both are reversible, and the modal
budget (#1013) is spent on acts that are not.

### Discovered Pitfalls
- **The error path in `tick` advances the watermark.** Every other failure becomes an
  assistant turn and calls `append_orchestrator_reply`. One `return` added on the
  interrupted path "to record what happened" through that function would silently eat the
  input the interrupt exists to preserve. `an_interrupted_turn_loses_no_input_and_keeps_its_session`
  pins it.
- **`tokio::pin!` gives a `Pin<&mut F>`, and dropping *that* does not drop the future.**
  Use `Box::pin` where the interrupt arm needs a real drop, or the child outlives the
  decision to kill it. clippy catches this one (`drop_non_drop`) — do not silence it.
- **A killed grandchild is a zombie, not an absence.** Its parent is dead and nothing here
  reaps it, so `kill(pid, 0)` reports every corpse as alive; liveness has to be re-derived
  from `/proc/<pid>/stat`'s state field (`Z` is dead), which is the pidfile rule verbatim.
- Interrupt alone re-answers the same input on the next `ORCHESTRATOR_TICK`. That is
  correct and is *why* quieting a lane is two acts; the detail string and the returned
  `lane` both say so, and the docs must too.
- `orchestrator_checked_out` had a second reader (`maintain_verify_dir`'s `may_reclaim`)
  asking a **different** question. Rewiring it onto the lane is safe only because the
  reclaim keeps keying on `checked_out`.
- A `Note` source of `"orchestrator"` matches the string `maintain_verify_dir` already
  uses; keep them one constant if a third appears.
- `tasks::server::router(store)` builds `Services::default()`, so `turn_control` is `None`
  in every router-only test — the interrupt route there answers `interrupted: false`
  rather than failing, which is what makes those tests keep working untouched.

### Blockers & Dependencies
None. #1053 (worker runs) removes the *reason* turns run long but not the need for the
control, and nothing here touches the worker lane. A side benefit worth one sentence in
the docs and deliberately not designed away: shutdown currently waits the turn out, and
`TurnControl` gives `reload --force` a cleaner option than the SIGKILL path.

### Complexity
Medium

### Notes
- Working implementation exists on this Scout's branch (server side complete, app side
  specified only); it is **not** a deliverable — the spec is. Test names to reproduce:
  `an_interrupted_turn_loses_no_input_and_keeps_its_session`,
  `an_interrupt_with_no_turn_in_flight_is_a_no_op_and_never_leaks_forward`,
  `holding_the_lane_leaves_the_turn_in_flight_alone`,
  `the_hold_is_idempotent_and_keeps_the_instant_it_was_placed`,
  `a_held_and_checked_out_lane_reports_both_reasons`,
  `the_lane_controls_refuse_every_actor_but_the_human` — all in
  `crates/tasks/tests/orchestrator.rs`, 29/29 green.
- The `TurnControl` slot is the whole concurrency argument; resist replacing it with an
  `AtomicBool` plus a stored request, which reintroduces both races it closes.
- CLAUDE.md wants a bullet: the lane controls, why the interrupt is a signal and the hold
  is a column, and that the reclaim keys on the checkout rather than on the lane.

## Spec 2 of 2: Nothing tells you what `play` will do before you press it (#993)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: Tell the person what `play` will do, once — and put the charter in the app

### Summary
Pressing `play` starts a pipeline that queues tasks, dispatches scouts and builds
into VMs, comments on issues, merges its own pull requests and closes issues,
none of it asking first. Nothing in the app says so before the click, and the
charter — the kill switch that governs all of it — is not rendered anywhere, so
the honest answer to "how do I stop it merging things" is `curl`. This adds two
things and deliberately not a gate: a **one-time sheet** on the first
user-initiated transition to `play` on a given install, whose body is *generated
from the charter rows* rather than written as prose, and a **charter panel** in
the Server window with a level control per capability. The sheet is dismissed
permanently once acknowledged, keyed off an on-disk acknowledgement and never
off "the mode changed" — every boot overwrites the stored mode from
`TASKS_DEFAULT_MODE`, so a mode-keyed sheet fires on every restart and gets
clicked through, which is the failure the issue names by name.

### Implementation Approach

**The generated half lives in `tasks-api`, not in the app.** `app-gpui` is not a
workspace member, so `make test` never runs its tests — the same argument that
put `crates/tasks/tests/disclaimer.rs` in the server's tree. Everything with a
rule in it goes where the suite already runs.

- `crates/tasks-api/src/models.rs` — add `Capability::permits()`: one
  human-facing, third-person clause per capability, beside the existing
  `describe()`. Two sentences of one fact and deliberately not one: `describe`
  is second person and instructional ("file issues for work you discover")
  because it is a line of the orchestrator's generated authority section, and
  rendering that at a human reads as though the human is being told to file
  issues. `permits()` names the act sharply and says whose account it acts on —
  "merge its own pull requests into your default branch, or close them
  unmerged", not `land_builds`. The match is exhaustive, so a capability added
  later is in the sheet because the enum is, not because somebody remembered.
- `crates/tasks-api/src/first_play.rs` (new) — the acknowledgement record and
  the grouping:
  - `<data dir>/first-play.json`, holding `{ "acknowledged_at": <rfc3339> }`.
    `acknowledged()` / `read()` / `record()`. It lives here rather than in the
    app for the two reasons `paths` does: more than one client reads it (both
    app windows can start the pipeline, and a future client that grows a play
    button must find the same acknowledgement rather than raise its own sheet),
    and it is testable where the app is not.
  - `Sheet::from_entries(&[CharterEntry]) -> Sheet { live, shadow, off }`,
    preserving `Capability::ALL`'s order (additive and reversible first,
    irreversible last, so a reader who stops early has already met the sharp
    ones). `off` is rendered too rather than filtered out: seeing a switch that
    is genuinely off is how a person learns the switches are real.
  - Reference implementation and 8 passing tests exist in this Scout's tree;
    reproduce them, they are the falsifiable part of this change.

**The gate is at each window's mode choke point, and there are two of them.**
The obvious reading — "`Workspace::set_mode` (`workspace.rs:647`) is the one
path" — is wrong, and it is the mistake to avoid: `ServerWindow::render_pipeline`
(`server_window.rs:397`) draws its own `[Play, Pause, Stop]` row whose buttons
call `ServerControl::set_mode` (`server.rs:757`) directly. Gate both, and gate
them at those two functions rather than at their call sites (the rail button,
the menu item, the palette command, the empty-state CTA all funnel through
`Workspace::set_mode`; a call site nobody has written yet inherits the gate —
the same argument that put the rationale check in `server::authorize`).

- A `first_play` module in `app-gpui` holding one **gpui `Global`** with the
  process-wide answer: `acknowledged: bool`, seeded from disk at startup, set
  by whichever window's sheet is confirmed. One answer, so acknowledging in the
  Server window does not leave the Workspace about to ask again.
- A request for `Mode::Play` while unacknowledged **opens the sheet and does not
  change the mode**. The sheet's confirm button writes the acknowledgement and
  *then* sets Play. `Pause` and `Stop` are never gated.
- The Workspace has no `ModalLayer` yet and grows one (`ModalLayer::new(self
  .focus_handle.clone())`, `self.modals.hold_focus(window, cx)` once per frame
  from `render`, `.relative()` on the root div). The Server window already has
  one; its sheet and its Stop confirmation are two ids in one layer, and a
  request for one while the other is up is the `ModalConflict` the caller
  surfaces rather than resolves.
- Sheet shape: `Scrim::Dim`, `Placement::Center`, `Dismissal::Dismissible` —
  escape and the scrim mean "not now" and start nothing, which is a real
  answer, and a modal whose safe exit needs a specific button is one whose
  other button is the easier target. **No `on_submit`**: ⌘-Enter deliberately
  does nothing here, because a hand that reflexively hits it has not read the
  sheet, and this is the one surface whose whole purpose is that the words get
  read.
- Body: the fixed half is `disclaimer::PIPELINE_CAUTION` (a fourth reader of
  those constants, never new prose) plus `README_POINTER`; the generated half
  is `Sheet`, three groups, one `Capability::permits()` line each; the last
  paragraph names the off switches — `pause` and `stop` in this same row, the
  charter panel below it (any capability to `off`, human-writable), and "Kill
  All Containers" in the command palette (`commands.rs:424`).

**The charter panel** goes in the Server window under the pipeline row and
`PIPELINE_CAUTION` — that window is already where the off switches and the
caution live. Eleven rows, each `Capability::permits()` plus an
off/shadow/live control, written through `Client::set_charter(cap, level,
daily_limit)` (already exists, `crates/tasks-client/src/lib.rs`, nothing calls
it). Carry the charter on `ServerControl`'s existing poll for that window and on
`Snapshot::fetch` (`state.rs:192`) for the Workspace's sheet — two readers of an
eleven-row endpoint, mirroring how mode is already read twice, rather than one
window reaching into the other's state. `daily_limit` is not exposed: it is
explicitly not part of the design's safety story.

### Discovered Pitfalls

- **"No charter answer" and "an answer that omits a row" are different, and
  collapsing them lies in the dangerous direction.** A missing *row* is `Off`,
  matching `Store::charter_entry` — and that is not merely conservative, it is
  correct, since the server genuinely refuses a capability it has no row for.
  But a charter that was never fetched (old server, network error) fed into the
  same function yields "it will not: everything", which is a false reassurance
  on the one surface that exists to warn. Render `None` as its own state — the
  fixed caution plus "the charter could not be read; assume every capability is
  on" — and never as an all-`off` sheet.
- **The Server window's Play button is easy to miss.** See above; a change that
  only gates `Workspace::set_mode` ships a sheet the second window walks past.
- **`Dismissal::MustAnswer` is the wrong instinct here.** It exists, and using
  it would make the sheet unescapable; escape must mean "then don't start it".
- **A failed write is not a refusal.** No `$HOME`, a read-only data dir: the
  pipeline still starts and the `Global` remembers for the session. A sheet that
  cannot be dismissed permanently is exactly the trained-out-of-use surface the
  issue argues against.
- **An unreadable record reads as "not acknowledged".** `paths`' hint rule
  points this way here: showing it twice costs a click, never showing it is the
  bug.
- **A host with `TASKS_DEFAULT_MODE=play` never sees the sheet**, because the
  gate is on the click and there is no click. Deliberate — that variable is a
  typed choice by someone who went looking — and the same reason the CLI
  (`tasks resume`, `POST /mode`) is out of scope. Nothing server-side changes.
- `app-gpui` builds and tests on Linux (`make app-check` / `make app-test`), but
  **run it with `-j 2`**: the previous attempt on this issue was OOM-killed at
  6144 MB with default parallelism. `cargo check -p tasks-api --tests` is 7s
  warm.
- The `permits()` strings are drift-prone against `disclaimer.rs` and the
  README. Pin what matters in `tasks-api`'s own tests (LandBuilds says "merge"
  and "pull request", RetireWork says "close", no clause renders its own slug,
  the acts on the reader's account say "your") rather than adding a fourth
  reader to `crates/tasks/tests/disclaimer.rs`.

### Blockers & Dependencies
None. `GET /charter`, `POST /charter/{capability}`, `Client::charter()` and
`Client::set_charter()` all exist; `crate::modal` exists and names this sheet as
a queued consumer in its own module doc. No migration, no server change, no
image rebuild.

### Complexity
Medium. The `tasks-api` half is small and fully testable; the bulk is GPUI
wiring across two windows, one of which grows its first `ModalLayer`.

### Notes
- Do not turn this into a confirmation dialog. It is one-shot per install, and
  every later `play` is unimpeded — the charter is a kill switch, not a
  promotion ladder, and pre-approval is the bottleneck the design rejects.
- The read-only charter list is most of the value and the eleven controls are
  the whole of it; if the panel has to be cut down under time pressure, ship the
  list, not a partial set of toggles.
- `app-gpui`'s tests are not run by `make test`. A rule that matters belongs in
  `tasks-api`; anything left in the app is chrome, and should be.

## Review feedback on these specs

A reviewer read the spec(s) above and approved them **with** the following. It is part of what was approved: the spec says what to build, this says what the reviewer required of it. It is not part of any spec text, so nothing above repeats it.

Treat every item as a requirement, not a suggestion. Where one genuinely conflicts with the spec it was written about, the feedback wins — it is the later word, written by the person who approved that spec — but **say so in `SUMMARY.md`**.

Account for every item in `SUMMARY.md` under a `## Review feedback` heading: one line per item saying you did it, or that you decided against it and why. Declines are fine and are expected to be written down; an item you silently dropped is indistinguishable from one you never read, and the reviewer reads the spec rather than this section.

### On spec 1 of 2: Interrupt the turn, hold the lane: the orchestrator gets the two live controls every other run already has (#1064)

Approved. The design decisions are right and I am not asking you to revisit them — the in-process signal over a `cancellations` row, the struct-not-enum lane, the hold as a durable column, the human-only refusal. Three required items, each accounted for in SUMMARY.md.

1. THE APP HALF IS NOT OPTIONAL, AND THE SPEC DEFERS IT. The issue says in its own words: "The knobs in the app — this is half the point — 'knobs' means buttons, not curl." The spec specifies the app surface and then marks it "not implemented here", which would ship a control whose only user interface is `curl`, against an issue that names that outcome as the failure. Implement both: an Interrupt button on the chat surface enabled off `in_flight.orchestrator`, and a Hold/Release control whose held state renders as a standing banner rather than a pressed button. No confirm dialogs on either — both are reversible. `app-gpui` compiles and unit-tests in your VM (`make app-check`, `make app-test`); only whether it rendered takes a Mac, so "could not build the app" is not an available reason. If you genuinely cannot land the app half, say so explicitly in SUMMARY.md and name what stopped you.

2. `/proc/<pid>/stat` DOES NOT EXIST ON THE DEPLOYMENT PLATFORM. The Discovered Pitfall says a killed grandchild's liveness "has to be re-derived from `/proc/<pid>/stat`'s state field, which is the pidfile rule verbatim". It is not verbatim and it is not portable: this server runs on macOS, which has no `/proc`. The existing rule is `crate::pidfile::pid_alive` — `ps -o state= -p <pid>`, treating an empty row or a leading `Z` as dead, with `kill -0` as the fallback for when `ps` could not be run at all. Reuse that function; do not hand-roll a second liveness check, and do not write `/proc` paths.

3. THE TRUNK MOVED UNDER THIS SPEC. It was written at 16:46 UTC; PR #1074 merged into main at 17:28 carrying #1070, #1049, #1054 and #1004, and it touched six of the files here including `crates/tasks/src/reload.rs`, `crates/tasks/src/server.rs`, `crates/tasks-api/src/http.rs`, `crates/tasks-api/src/models.rs` and `app-gpui/src/server_window.rs`. #1070 in particular changed how host maintenance gates on in-flight runs, which is adjacent to the lane predicate you are adding. Re-read those files at current main rather than trusting the spec's descriptions of them, and check whether `reload`'s drain and your hold now have anything to say to each other.

One note, not a required change: the same `kill_on_drop`-takes-the-child-only hazard your process-group sweep fixes also exists on the worker lane (`crates/tasks/src/worker.rs` spawns with `kill_on_drop(true)` and is dropped by `cancel::bounded`). Do NOT widen this build to fix it — I have filed it separately — but write `signal_group` and the sweep so that a second caller can use them rather than burying them in the orchestrator's turn path.

### On spec 2 of 2: Nothing tells you what `play` will do before you press it (#993)

Approved. The shape is right — generated from the charter rather than written as prose, keyed off an on-disk acknowledgement rather than off the mode, dismissible, and not a gate on every later `play`. Do not turn it into a confirmation dialog. Four required items, each accounted for in SUMMARY.md.

1. THERE IS A THIRD PLAY BUTTON, AND THE SPEC MISSES IT. The spec's own best pitfall is that gating `Workspace::set_mode` alone ships a sheet the Server window walks past — the same sentence applies once more. `app-gpui/src/bin/tasks-menubar/popup.rs:497` toggles the mode from the menubar popup, through `machines::toggled_mode`, and both `Pause` and `Stop` toggle to `Play` there. So a menubar user starts the pipeline having seen nothing. Gating inside `ServerControl::set_mode` (`app-gpui/src/server.rs:748`) catches it, but then the menubar has a button that silently does nothing, because a status-item panel is not a window that can host the sheet. Decide explicitly and say which you chose in SUMMARY.md; the two acceptable answers are that the menubar's play routes the person to the Server window with the sheet up, or that it is disabled with a stated reason until the acknowledgement exists. Silently starting an unacknowledged pipeline is not one of them. While you are there: the menubar can point at other machines (`TASKS_MENUBAR_MACHINES`), so an acknowledgement stored in the local data dir says nothing about the remote server whose charter would actually govern the run — one sentence on what you did about that.

2. DECLARE THE FILES YOU ACTUALLY TOUCH. The spec's `files_touched` names three `tasks-api` files, while the bulk of the work is `app-gpui`: a new `first_play` module, a sheet in two windows, the charter panel in `server_window.rs`, and a first `ModalLayer` on the Workspace. That field is what the server's overlap check reads, so as declared it could not see that this collides with the approved spec for #1064 (which also adds to `server_window.rs` and to the chat surface) or with #998's `workspace.rs` work. List the real set.

3. THE SUPERVISOR'S GREEN RUN PROVES NOTHING ABOUT THE APP HALF. `.tasks/verify` is `make test-ci` over the workspace, and `app-gpui` is not a workspace member. Run `make app-check` and `make app-test` yourself and report the exact commands and outcomes. Use `-j 2` as the spec says: the previous scout on this issue was OOM-killed in the same 6144 MB VM at default parallelism, and that is a real constraint rather than a suggestion. Putting the rule-bearing half in `tasks-api` is the right call and I checked the precedent — `tasks-api/src/paths.rs` already reads and writes files, so `first_play.rs` is not a new kind of thing in that crate.

4. RENDER "THE CHARTER COULD NOT BE READ" AS ITS OWN STATE. The spec says this and it is the single most important line in it: a fetch that failed must never flow through the same path as a charter of all-`off` rows, because the sheet would then promise the pipeline will do nothing on exactly the surface that exists to warn. Pin it with a test.

Verified against the tree so you do not have to: `app-gpui/src/modal.rs` is on main, the Workspace genuinely has no `ModalLayer` yet, `Capability::ALL` is eleven entries including `DispatchWorkers`, `Client::set_charter` exists and no app code calls it, `disclaimer::PIPELINE_CAUTION` is at `disclaimer.rs:43` with readers in `server_window.rs:431` and `workspace.rs:2866`, and `commands.rs:424` is "Kill All Containers". If the charter panel has to be cut for time, ship the read-only list rather than a partial set of toggles, as the spec says.

## Directions for this implementation

The orchestrator agent added the following when requesting this build. It is **not** part of any spec above, and no reviewer has seen it — it is addressed to you.

Treat it as a requirement, not a suggestion. The specs are still what is being implemented; these directions say how to go about it. Where one genuinely conflicts with a spec, the direction wins — it was written after the spec was approved, with this build in view — but **say so in `SUMMARY.md`**, because the reviewer reads the spec and cannot see this section.

Account for every direction in `SUMMARY.md` — including any you decided against, and why. A direction you silently dropped is indistinguishable from one you never read.

These two specs both change `crates/tasks-api/src/models.rs`, which is why they are one build rather than two. Implement both on this branch and reconcile that file deliberately rather than letting whichever you write second overwrite the first: read what your own earlier edit left there before you add to it, and if the two want the same type or field to mean different things, say so in SUMMARY.md rather than silently picking one.

Order them #993 first (what `play` will do before you press it) and then #1064 (the orchestrator's interrupt and lane-hold controls). #993 is the smaller surface and is mostly additive to the model; doing it first means #1064's larger changes land against a settled shape rather than the other way round.

The app half is not covered by the workspace suite: `app-gpui` is not a workspace member, so `make test` and `cargo clippy --workspace` compile none of it. If you touch anything under `app-gpui/`, run `make app-check` and `make app-test` as well and report both results in SUMMARY.md. Do not report a result you did not actually run.

## Your job

1. Implement every spec above, in order, as one coherent change in the cloned repo (cwd). You are on the right branch already.
2. Run the project's tests / lint / typecheck — get them green.
3. Commit your work with clear messages (a git identity is configured).
4. Write `SUMMARY.md` in the repo root: one or two paragraphs describing the change, suitable as a pull request body. Do not use GitHub closing keywords (`Closes #N`, `Fixes #N`) — the server links the issues itself.
5. Do NOT push and do NOT open a PR — the server does both.

**You have 60 minutes, once.** That is the whole run — the clone before you started, this turn, the supervisor's own test run and the packaging after it — measured on the wall clock from dispatch. There is no later: when you end your turn the run is over. A backgrounded command buys you nothing — its child is killed with the turn — so anything whose result you need must be awaited inline, and a poll loop over a file another process will write can only report to a turn that has already ended. Nor should you start what cannot finish: a cold build in a large workspace can run forty minutes, so weigh what a command will cost against what is left.

On step 2: when this project declares a test suite at `.tasks/verify`, the supervisor runs it itself after you finish, against the committed tree your branch carries. If it fails you get one chance to fix it and then the build fails with no pull request, so getting there first is entirely in your interest. It reads that script out of the build's BASE commit, so editing it changes nothing about what runs.
