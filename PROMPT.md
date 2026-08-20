You are a Builder in the Double Diamond architecture.

You are implementing 1 approved spec(s). Verify a spec's claims against the code in front of you; where a spec has a Scout behind it, trust its pitfalls.

## Spec 1 of 1: Nothing tells you what `play` will do before you press it (#993)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: Tell the person what `play` does, once — and make the charter visible

### Summary
Pressing Play starts VMs on the host, spends Anthropic API credit, opens pull
requests and — with the charter as it ships — merges them and closes issues,
none of it asking. Nothing in the app said so before the click, and the charter
that governs the sharp half was not rendered anywhere, so the answer to "how do
I stop it merging things" was `curl`. This adds three things and no gate: a
durable server-side record of whether this install's owner has ever been shown
what unattended operation means (`autonomy_notice`, one row, human-only,
nothing reads it to decide whether an action may happen); a first-run notice
window that intercepts the *first* Play press on every path that can start the
pipeline, generated from the charter rows rather than written as prose, and
whose primary button acknowledges and then carries the intercepted press
through; and a Charter window with the nine capabilities as rows and Off /
Shadow / Live per row. The charter still ships all-`live` and stays a kill
switch rather than a promotion ladder — the concession is only that the first
press explains itself, and every later one does not.

### Implementation Approach

**Wire types — `crates/tasks-api/src/models.rs`.**
- `Capability::consequence()` added as arms of the *same* exhaustive match as
  `describe()`. `describe()` speaks to the agent holding the permission ("file
  issues for work you discover"); `consequence()` speaks to the person who owns
  the repository ("file new issues on your repositories"). Two renderings, one
  match, so a tenth capability cannot answer one and not the other — it fails
  to compile. That is the whole guarantee, and it is why this lives beside
  `describe()` rather than in the app, where a ninth line would simply go
  missing.
- `Capability::is_sharp()` — changes something outside Tasks the person cannot
  take back. Also an exhaustive match, so a new capability is classified rather
  than defaulting into the quiet half. `cancel_runs` throws away a VM hour and
  is not sharp; `retire_work` closes an issue on GitHub and is.
- `Capability::title()` — the name a person reads. The slug stays in the API.
- `Capability::BY_CONSEQUENCE` — a second ordering, not a re-sort of `ALL`.
  `ALL` is the order the charter is *flipped* in (additive first, irreversible
  last) and reading it backwards does not give this one: `curate_work` sits
  last there for a good reason, and it is the wrong head for a person being
  told what is about to happen. Merging pull requests goes first. Length is
  `Capability::ALL.len()`, so a tenth capability is a compile error; a test
  pins that it is a permutation.
- `AutonomyNotice { acknowledged_at: Option<DateTime<Utc>> }` in `http.rs`.

**Store and routes.**
- `crates/tasks/migrations/20260819221412_autonomy_notice.sql` — one row,
  `CHECK (id = 1)`, `acknowledged_at` never overwritten.
- `Store::autonomy_notice()` / `acknowledge_autonomy_notice()` —
  `INSERT OR IGNORE` then read back, so the **first** acknowledgement is the
  one that stands (a second click from another surface must not rewrite "when
  was this person told" into a later, wronger answer) and the call is
  idempotent, which is what lets a client fire it without checking first.
- `GET /autonomy-notice` (readable by anyone — a client that cannot read cannot
  decide whether to explain) and `POST /autonomy-notice/ack`, **human-only and
  not charter-gated**, on the `build-now` precedent: the row records that a
  *person* was shown this, and an orchestrator that could write it would be
  clicking through its own disclosure. Two tests: first-ack-wins, and the
  orchestrator refused with every capability `live` and the refusal proven to
  be a no-op.
- `Client::autonomy_notice()` / `acknowledge_autonomy_notice()`.

**App — `app-gpui/src/autonomy.rs` (new).**
- `Permissions::read(&[CharterEntry])` sorts the nine into `on_its_own` /
  `records_only` / `withheld` in `BY_CONSEQUENCE` order. Three buckets, not
  one: collapsing `shadow` into "can" claims an effect that never happens, and
  into "cannot" hides a judgment still being made and recorded. A capability
  with no row reads `Off`, matching `Store::charter_entry` — the notice must
  not be the one place silence reads as permission.
- `Permissions::sharp_summary()` — the headline sentence, naming only the sharp
  capabilities that are `live`. `None` when there are none, rather than an
  invented warning.
- `ALWAYS` — what Play does whatever the charter says (VMs, API credit, pull
  requests). Deliberately not generated: mode gates *dispatch*, and a charter
  narrowed to nothing still spends the machine. A notice that only listed
  capabilities would read as "play does nothing now".
- `OFF_SWITCHES` — pause, stop, the charter, Kill All Containers.
- `intercepts(mode, app_state, cx) -> bool` — the one guard every path that
  sets the mode calls, because four copies of "has this person been told" is
  how three of them end up disagreeing. Returns `false` (carry on) for any mode
  but `Play`, for an acknowledged install, and when the platform refuses to
  open the window — a press must not be swallowed.
- The window itself: `then_play` carries the intercepted press, so nobody
  presses Play twice, and a raise from a press upgrades a menu-opened window.

**App — `app-gpui/src/charter_window.rs` (new).** Nine rows generated from
`BY_CONSEQUENCE` × three level buttons, each level's meaning spelled out
(`shadow` gets the longest, being the only counter-intuitive one). It observes
`AppState`, so a charter written elsewhere lands here.

**App — plumbing.**
- `AppState` became a gpui **global** (`state::init(cx)` from `main.rs`,
  mirroring `server::init`), and `Workspace::new` now takes
  `AppState::global(cx)`. This is the load-bearing structural change: the
  Server window reaches the pipeline with *no workspace focused* — that is most
  of what that window is for — so a guard living on the workspace would have
  left exactly that path unexplained, and two entities would have held two
  answers about whether anybody had been told.
- `AppState.charter: Vec<CharterEntry>`, refetched with every other list (it is
  human-writable from more than one place, and a stale copy would misreport an
  authority since narrowed — the one direction this must not be wrong in).
  `set_charter` settles the response **in place**, because a charter write
  publishes no event and nothing would arrive to refetch on.
- `AppState.autonomy_acknowledged: Option<Option<DateTime<Utc>>>` — unknown /
  never / when. `owes_autonomy_notice` is a free function so the rule is
  testable without a gpui `App`, and the rule is: **only a positive "nobody
  ever has" fires the notice.** Asked once per refresh until answered, then
  never again — it is a one-way fact with no un-acknowledge endpoint.
- Menu items `Charter…` and `What Play Does…`, in the pipeline group under Kill
  All Containers, neither `.on_workspace()`: the moment someone goes looking
  for an off switch is not the moment to grey it out for want of focus.

**`CLAUDE.md`** gains one rule stating the not-a-gate argument and the three
properties that keep the notice from becoming one.

### Discovered Pitfalls

- **Four paths set the mode, not three.** The title bar buttons, the Server
  menu radio group and the command palette all funnel through
  `Workspace::set_mode`; `server_window.rs`'s own mode row goes through
  `ServerControl::set_mode` with a different client and never touches
  `AppState`. That fourth path is what forced `AppState` to become a global.
  A fifth is easy to add by calling `AppState::set_mode` directly — anything
  new must go through `autonomy::intercepts` first.
- **A charter write publishes no event.** `Store::set_charter` appends nothing
  to the event log, so the app's "refresh on every SSE event" contract does not
  cover it. The toggle settles the returned `CharterEntry` in place; without
  that the row sits at its old level until something unrelated moves.
- **`TASKS_DEFAULT_MODE` makes "the mode changed" useless as a trigger.** Every
  boot overwrites the stored mode, so a transition-watcher fires on every
  restart. Hence the durable row. The converse gap is real and deliberate: a
  server that boots straight into `play` starts dispatching with nobody having
  pressed anything, and the notice does **not** fire — a modal on launch is the
  one that gets clicked through. `Server ▸ What Play Does…` stays available
  forever as the reachable answer.
- **Absence of evidence must not fire the notice.** The tempting reading of
  "unknown" is "probably never told, so show it", and that turns an unreachable
  endpoint into a modal on every press, which is how a notice stops being read.
  The tri-state exists for exactly this; stub `owes_autonomy_notice` to `true`
  and the test that pins it fails.
- **The notice names menu items in prose.** `OFF_SWITCHES` says "Pipeline:
  Pause", "Kill All Containers", "Server ▸ Charter…". A rename would silently
  point the person at something that is not there, so
  `every_off_switch_the_notice_names_is_in_the_server_menu` drives the
  assertion off the array itself.
- **This box OOM-kills `ld` with two concurrent link jobs.** `cargo test` in
  `app-gpui` links two test binaries; run it `-j 2`. Nothing to do with the
  change, but it costs ten minutes to rediscover.
- `Capability::ALL` reversed is *not* the order to show a person. It is
  documented as "additive and trivially reversible first, irreversible-ish
  last", which puts `curate_work` at the tail and would put it at the head of
  any reversal — where merging pull requests belongs.

### Blockers & Dependencies
None blocking. Two seams to name, both requested:

- **#991 (starting the server).** Strictly upstream, and there is no ordering
  hazard: the notice is a server row, so with nothing serving
  `autonomy_acknowledged` is `None` (unknown), the guard declines to fire, and
  a Play press has nothing to POST to anyway. The first-run sequence is
  therefore #991 → #992 → #993, three windows in a row on a fresh install; if
  #991 grows a "start the server" first-run affordance, it should hand off to
  this rather than repeat any of it, and the two should not both be modal.
- **#992 (empty states).** The concrete seam: if the Tasks list's empty state
  grows a "press Play to start work" call to action, it **must** route through
  `Workspace::set_mode` (or `autonomy::intercepts` directly) and not through
  `AppState::set_mode`, or it becomes a fifth unguarded path. #992 is also the
  natural home for the standing, non-modal answer to "the pipeline is playing
  and you have never acknowledged it", which this deliberately does not add.

### Complexity
Medium

### Notes

Every direction is accounted for below, including the one I read narrowly.

- *"#999 is precise about what this is not: not a gate."* Held throughout, and
  written into the code rather than left to a reviewer: the module doc, the
  migration comment, the handler doc and the new `CLAUDE.md` rule all say that
  nothing reads the row to decide whether an action may happen. The server
  never consults it. Turning this into a gate later would take a deliberate new
  reader, not a refactor.
- *"Two pieces: a first-time sheet, and the charter made visible."* Both built.
  The issue said a read-only list would be most of the value and nine toggles
  the whole of it; the window does the toggles.
- *"The app should show what the orchestrator may do on its own, in the same
  words the authority section uses, because that is what is actually
  enforced."* This is the one direction I did **not** follow literally, and it
  is the conflict to flag. `authority_section`'s words are second-person job
  descriptions addressed to the agent ("file issues for work you discover");
  read by a person deciding whether to let something loose on their repository,
  they systematically understate. So the app uses `consequence()` — the same
  fact, the same enforced row, a different audience — and the two are arms of
  one exhaustive match so they cannot drift or lose a line. The *structure* is
  the same source; only the voice differs. If the reviewer wants the literal
  strings, deleting `consequence()` and rendering `describe()` is a small
  change, but the notice gets meaningfully weaker.
- *"Be concrete about consequence rather than mechanism: start VMs, spend API
  credit, merge pull requests, close issues."* `ALWAYS` says the first two (and
  "open pull requests", since a finished build does that under play whatever
  the charter says); `sharp_summary` leads with "merge its own pull requests
  into your default branch" and "close your issues". It says "your default
  branch" rather than "`main`" because the app does not know
  `SCOUT_BASE_BRANCH` and inventing the name would be a claim.
- *"Meets #992 and #991 — name the seam."* Named above, with the one concrete
  instruction #992's Builder needs.
- *"Watch your run budget — checkpoint NOTES.md."* Done; `NOTES.md` was written
  as I went and carries the findings, the file map and the `-j 2` gotcha.

For the Builder:

- The whole thing is green here: `cargo clippy --workspace --all-targets` and
  the app's clippy are clean (the five remaining app warnings are pre-existing
  in `bin/tasks-menubar/popup.rs`), `cargo test -p tasks` is 389 lib + 20
  integration binaries passing, `cargo test --doc --workspace` passes, and the
  app is 223 + 35 with twelve new tests among them.
- The migration is stamped `20260819221412`. Regenerate it with
  `make migration NAME=autonomy_notice` if this lands on a later day — the
  content is what matters, not the instant.
- The menubar binary (`tasks-menubar`) can also toggle a machine into `play`
  (`machines.rs::toggled_mode`) and is **not** guarded. It is a different
  binary, it can point at several machines, and a modal there is a different
  design question; the server row means it cannot get the answer wrong, only
  skip the explanation. Worth its own issue rather than a hurried window.
- `make app-check` / `make app-test` work on Linux, so the app half of this is
  reviewable without a Mac. What is *not* reviewable here is whether the notice
  window's 560×620 is enough for the longest charter (nine "cannot" lines plus
  nine "can" lines cannot both be long, but a shadow-heavy charter renders all
  three lists); the body is a plain column, so if it overflows on a Mac, give
  it the `overflow_y_scroll` the Charter window already has.
- The acknowledge path closes the window immediately and reports a failed POST
  in the sidebar banner, so an ack that did not stick shows the notice again on
  the next press. That ordering is deliberate: a pipeline started against an
  acknowledgement that did not stick would be running with nothing on record
  saying anyone was told.

## Review feedback on these specs

A reviewer read the spec(s) above and approved them **with** the following. It is part of what was approved: the spec says what to build, this says what the reviewer required of it. It is not part of any spec text, so nothing above repeats it.

Treat every item as a requirement, not a suggestion. Where one genuinely conflicts with the spec it was written about, the feedback wins — it is the later word, written by the person who approved that spec — but **say so in `SUMMARY.md`**.

Account for every item in `SUMMARY.md` under a `## Review feedback` heading: one line per item saying you did it, or that you decided against it and why. Declines are fine and are expected to be written down; an item you silently dropped is indistinguishable from one you never read, and the reviewer reads the spec rather than this section.

### On spec 1 of 1: Nothing tells you what `play` will do before you press it (#993)

Approved with four required changes. Generating the notice from the charter rows rather than writing prose is the right call and is what makes this more than a warning dialog — `consequence()` as an arm of the same exhaustive match as `describe()`, so a tenth capability cannot answer one and not the other, is the guarantee the app half needed. Departing from my direction to reuse `authority_section`'s literal words, and saying why (second-person job descriptions addressed to the agent systematically understate when read by the person who owns the repository), is correct and I withdraw the direction.

**1. You have written a second vocabulary for the same claims, into an app that is about to grow a module whose entire purpose is preventing that.**

#984 — approved, `build_0e828ac4`, queued **ahead** of you — adds `app-gpui/src/disclaimer.rs` holding `HEADLINE`, `SUMMARY`, `PIPELINE_CAUTION`, `PLAY_TOOLTIP` and `README_POINTER`, with this reason: *"One module and not three inline strings, because the three surfaces are read at three different moments and must not drift into three different claims."* Its own notes name this issue and say it *"should reuse `disclaimer::HEADLINE` and `disclaimer::PIPELINE_CAUTION` rather than writing new words."*

`autonomy.rs` mentions neither. It introduces `ALWAYS`, `OFF_SWITCHES` and `sharp_summary()` covering substantially the same ground — what Play does, what it spends, what it does to your repositories — in words this Scout chose. So the app ends up with two modules of risk copy written by two agents that could not see each other, which is the exact failure `disclaimer.rs` was built to prevent, occurring in the same release.

To be fair to the design: your version is *better* where they overlap, because it is generated from the enforced charter rows rather than being a constant. So the fix is not "use `disclaimer.rs` instead". Required: **one vocabulary, not two.** Take `disclaimer.rs`'s constants for the static claims (`ALWAYS` and the headline are exactly `PIPELINE_CAUTION`/`HEADLINE` territory) and keep the generated parts yours; or subsume them, re-point `about.rs`, `server_window.rs` and the Play tooltip at `autonomy.rs`, and delete the duplicate — the house rule is to upgrade weaker code in place rather than leave both standing. Either is fine. Two modules disagreeing about what Play does in six months is not. Say in `SUMMARY.md` which you chose, and read `disclaimer.rs` as it actually landed rather than as I have quoted it.

**2. The #992 seam is addressed to a build that ships before you.**

You write: *"if the Tasks list's empty state grows a 'press Play to start work' call to action, it **must** route through `Workspace::set_mode`… or it becomes a fifth unguarded path."*

It already does. #992 is approved and queued **ahead** of you, and its `Action::Play` variant is exactly that call to action. So this is not guidance for #992's Builder — it is work for yours, and it is stated as a conditional about the future when it is a fact about your base.

Required: find the empty state's Play path in the tree you clone and put it behind `autonomy::intercepts`, or verify it already inherits the guard through `Workspace::set_mode` and say in `SUMMARY.md` which of the two you found. Your own pitfall — *"a fifth is easy to add by calling `AppState::set_mode` directly"* — is the thing to check for, not to warn about. If #992's build did not land, say that instead.

**3. `AppState` becoming a global is the largest-blast-radius change in the queue, and it lands last.**

Changing `Workspace::new`'s signature and moving `AppState` to a gpui global touches the constructor that four queued builds edit around: #984, #987, #992 and #995 all reach into `main.rs`, `workspace.rs`, `server_window.rs` or `state.rs`, and #987 shares six files with you including `state.rs` and `http.rs`.

The argument for it is sound — the Server window sets the mode through `ServerControl` with no workspace focused, so a workspace-held guard would miss precisely the path that window exists for, and following `server::init`'s existing precedent rather than inventing a pattern is right.

Required: keep it **mechanical and separable**. Do the global conversion as its own commit touching only what the signature change forces, ahead of the feature commits, so a conflict during the rebase is resolvable by someone reading one small diff rather than untangling it from the notice. List in `SUMMARY.md` every call site the signature change moved. Do not take the opportunity to tidy anything else in those files.

**4. Three windows in a row on a fresh install is a real problem, and you are the only one of the three who can see it.**

You name it — *"the first-run sequence is therefore #991 → #992 → #993, three windows in a row on a fresh install"* — and then leave it, with a conditional addressed to #991, which does not exist yet.

#992 does exist and ships before you. So the reachable half of this is yours: a person on a fresh install meets #992's empty state explaining that nothing is configured, and then, on the first thing they press, a 560×620 modal listing what the pipeline will do to their repositories. Required: make sure your notice cannot open over #992's first-run state or stack on another window, and say in `SUMMARY.md` what you did and what you observed — noting that whether it *looks* right is carve-out (c) and not something either of us can check here.

I am not asking you to build the coordination layer. I am asking that the one adjacency you can see be handled rather than described.

**Recorded so it is not re-litigated:** the tri-state `autonomy_acknowledged`, with only a positive "nobody ever has" firing the notice, and the observation that reading unknown as "probably never told" turns an unreachable endpoint into a modal on every press; `INSERT OR IGNORE` so the first acknowledgement stands, since a second click must not rewrite when this person was told into a later, wronger answer; the route being human-only and not charter-gated on the `build-now` precedent, with the refusal proven to be a no-op; `TASKS_DEFAULT_MODE` making mode-transition useless as a trigger, and the deliberate converse gap that a server booting into `play` does not fire it because a modal on launch is the one that gets clicked through; `BY_CONSEQUENCE` as a second ordering rather than a reversal of `ALL`, with the permutation test; three buckets rather than two, because collapsing `shadow` either way lies; a capability with no row reading `Off` so the notice is not the one place silence reads as permission; `intercepts` returning false when the platform refuses to open the window, so a press is never swallowed; `then_play` carrying the intercepted press; settling the charter response in place because a charter write publishes no event; driving `every_off_switch_the_notice_names_is_in_the_server_menu` off the array so a rename cannot silently point at nothing; and closing the window before the POST settles so an ack that did not stick shows the notice again rather than leaving a pipeline running with nothing on record.

**One thing to state rather than change.** `is_sharp()` reads as irreversibility, and the audience it serves is asking a slightly different question. Filing thirty issues under `capture_work` or commenting under `comment_on_work` is deletable and therefore not sharp by your definition, while being the most *visible* thing this can do to a public repository. Your `ALWAYS` list carries the weight here, so I am not asking for a reclassification — but say in the doc comment that `is_sharp` means "cannot be taken back" and not "worth being told about", so the next person does not read the absence of `capture_work` as a judgment that nobody minds.

**And thank you for naming the menubar gap** rather than hurrying a window into a second binary — `machines.rs::toggled_mode` can put a machine into `play` unguarded. I have filed it separately; do not touch `tasks-menubar` in this build.

---

## Amendment — required change 3 is replaced. Do not make `AppState` a global.

Nate questioned this and he is right to. I went and read the code, and the spec's argument for the global does not hold, while a *different* argument does — which points at a smaller change that also fixes a live bug.

**What is actually there.** `AppState` is not an `Arc<AppState>`; it is an `Entity<AppState>` (`workspace.rs:157`), which is already a cheap, cloneable, weakly-referenceable gpui handle. It is constructed **inside** `Workspace::new` — `let app_state = cx.new(AppState::new)` at `workspace.rs:298` — so the Workspace owns it and, as far as I can see, nothing else holds a reference.

**The spec's reason is wrong.** It says the Server window "reaches the pipeline with *no workspace focused*", and concludes a workspace-held guard would miss that path. Focus was never the obstacle: `main.rs:35` already keeps a `WorkspaceWindow` global holding the window handle, and it is read at `main.rs:102` without any focus requirement. A guard could reach the Workspace through it today.

**The real reason is that the workspace can cease to exist.** `main.rs:41-46` wires `on_reopen`, and the comment at `main.rs:100` says the handle "stays structurally valid after its window closes, so a stale one is only detectable by `update` failing." So cmd-W with the Server window still open leaves no Workspace, and therefore no `AppState`, and pressing Play there has nothing to consult. That is a real hole and it is why *something* has to outlive the window — but it is a statement about **ownership**, not about globals.

**And it surfaces a bug that exists today, independent of this issue.** Because `AppState` is constructed inside `Workspace::new`, closing the main window drops it, and reopening builds a fresh one and refetches everything. Every cmd-W is a full state reset. Nobody filed that; it falls out of reading this.

**What to build instead.** Fix the ownership, not the access path:

1. Construct `AppState` at app init, in `main.rs`, before the first window opens — once for the process.
2. Keep passing it **explicitly**: `Workspace::new(app_state, window, cx)`. The dependency stays visible in the signature, which is the property Nate is defending and the reason not to reach for a global by default.
3. Store only a **`WeakEntity<AppState>`** in a global, as the escape hatch for the one caller that cannot be handed it — `server_window.rs`, which is constructed on its own path. Weak, not strong, so the global is not what keeps it alive and a leak cannot hide behind it. This is Zed's own shape: `AppState::global` exists there and holds a `Weak`, while the ordinary path is still an explicit `Arc<AppState>` argument. Both halves of what Nate said are true of Zed simultaneously.
4. `autonomy::intercepts` keeps its signature and reads through whichever it is given.

This is smaller than what you specified — `Workspace::new` still takes its state, so the call sites move once instead of the type's whole access pattern changing — and it removes the reset-on-reopen behaviour as a side effect. Say in `SUMMARY.md` that it does, since that is a user-visible change nobody asked for and a reviewer should be told rather than discover it.

If, on reading the tree, you find a reason this cannot work — something else constructing `AppState`, or a borrow problem at init before any window exists — do **not** silently fall back to the global. Say so in `SUMMARY.md`, name the obstacle, and implement the global as originally specified. I would rather be told the smaller change failed than find it was abandoned quietly.

The rest of required change 3 stands: whatever the shape, do the ownership move as its own commit ahead of the feature commits, touching only what the signature change forces, and list every call site it moved.

## Directions for this implementation

The orchestrator agent added the following when requesting this build. It is **not** part of any spec above, and no reviewer has seen it — it is addressed to you.

Treat it as a requirement, not a suggestion. The specs are still what is being implemented; these directions say how to go about it. Where one genuinely conflicts with a spec, the direction wins — it was written after the spec was approved, with this build in view — but **say so in `SUMMARY.md`**, because the reviewer reads the spec and cannot see this section.

Account for every direction in `SUMMARY.md` — including any you decided against, and why. A direction you silently dropped is indistinguishable from one you never read.

Almost everything the review asks of you is a reading task before it is a writing task. Four builds touching `app-gpui` land ahead of you, and your spec was written before any of them existed.

Read these three things in your clone before you write a line:

`app-gpui/src/disclaimer.rs` — #984's. If it is there, it holds the app's risk copy, and you must not stand a second vocabulary beside it. Either consume its constants for the static claims in `ALWAYS` and the headline, or absorb them into `autonomy.rs` and re-point `about.rs`, `server_window.rs` and the Play tooltip at you, deleting the duplicate. Both are acceptable; two modules saying overlapping things in different words is not. Say which you chose and why in `SUMMARY.md`.

The Tasks list's empty state — #992's. It has a Play action. Trace how it sets the mode. If it goes through `Workspace::set_mode` it inherits your guard and you need only say so; if it reaches `AppState::set_mode` or a client directly, it is the fifth unguarded path your own pitfalls predicted and you fix it. Report which you found.

`workspace.rs`, `main.rs`, `state.rs`, `server_window.rs` as they now are — #984, #987 and #992 have all edited them. Line numbers and surrounding code in your spec are stale.

On the `AppState` global: do the conversion first, as its own commit, touching only what the signature change forces. Nothing else in those files, no tidying, no reordering. It is the largest-blast-radius change in the queue and it lands after everything it conflicts with, so the thing that makes it survivable is that its diff can be read on its own. List every call site it moved.

Do not touch `crates/../bin/tasks-menubar` or `machines.rs`. You correctly identified that it can put a machine into `play` unguarded and correctly declined to hurry a window into it; that is filed separately and is not this build's work.

On verification: `make test` for the server half and report the real number — the trunk has moved well past what your spec measured. For the app, use `cd app-gpui && cargo test -j 2` per your own finding that two concurrent link jobs OOM-kill `ld` on the builder. Rendering is unverifiable here, so state plainly that the notice window's size and its behaviour over another window are unchecked rather than implying otherwise.

Account for every item under `## Review feedback` in `SUMMARY.md`, declines included.

## Your job

1. Implement every spec above, in order, as one coherent change in the cloned repo (cwd). You are on the right branch already.
2. Run the project's tests / lint / typecheck — get them green.
3. Commit your work with clear messages (a git identity is configured).
4. Write `SUMMARY.md` in the repo root: one or two paragraphs describing the change, suitable as a pull request body. Do not use GitHub closing keywords (`Closes #N`, `Fixes #N`) — the server links the issues itself.
5. End `SUMMARY.md` with one line saying whether you actually ran the tests, in exactly this shape:
`Verification: PASSED — <the command you ran>`
`Verification: FAILED — <the command, and what failed>`
`Verification: NOT RUN — <why not>`
Report what actually happened. Nothing re-runs this suite for you downstream, so this line is the only evidence anyone has that the change works — claiming a run you did not make is the one thing here that cannot be caught later, and "NOT RUN" costs the batch a look from a human rather than costing you anything.
6. Do NOT push and do NOT open a PR — the server does both.
