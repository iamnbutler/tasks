You are a Builder in the Double Diamond architecture.

You are implementing 4 approved spec(s). Verify a spec's claims against the code in front of you; where a spec has a Scout behind it, trust its pitfalls.

## Spec 1 of 4: Scouts and builders are disposable — stop gating host maintenance on in-flight runs (#1070)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: A run in flight is not a reason to refuse host work — `tasks hold`, and the gates that go with it

### Summary
Host maintenance stops asking a human for permission. `make images` no longer runs
`tasks drain --check`; it wraps the rebuild in a new `tasks hold --label 'make images'
-- <command>`, which pauses dispatch, runs the rebuild **as its own child**, and puts
the mode back the instant that child exits — success, failure or signal — exiting with
the child's status. It waits for nothing and cancels nothing: what a rebuild can
actually spoil is a run *dispatched into it* (which starts in the old image — the #909
staleness `UpdateWatch` structurally cannot see, since the identity it reads comes only
from a run that has already started), and a run that started earlier is not that case.
`tasks reload` stops refusing on work in flight and reports it instead, because
`resume_in_flight` re-attaches to every live VM and the worst case is one `Orphaned`
write-off that charges no attempt. `stop --when-idle` turns out never to have left a
hold at all — `apply_startup_mode` overwrites the stored mode from `TASKS_DEFAULT_MODE`
before the next listener binds, and between the SIGTERM and that boot nothing reads the
column — so its closing sentence was the whole of that problem and is rewritten rather
than the behaviour. `tasks drain` / `tasks resume` stay, narrowed to the one host act
with no recovery: restarting vm-pool on the same socket. `drain --check` survives
demoted from a gate to a diagnostic, and **nothing in the repo refuses on it**. The
doctrine in `CLAUDE.md` is rewritten with the rest, because a fix that edits the recipe
and leaves the doctrine grows back.

### Implementation Approach
- **`crates/tasks/src/reload.rs`**
  - `ModeAfterDrain::HeldForCommand`, a fourth variant carrying its own
    `pause_note`/`nothing_happened`/`feed_note`/`restore_note`. Named in every match
    rather than caught by a wildcard, so a fifth variant stays a compile error.
  - `HoldOptions { command, label }`, `Held { NotServing | Unheld | Held }`,
    `hold_for_command()`, `render_held()`, `run_child()`, `exit_code()`.
    `hold_for_command` reuses `pause_dispatch` — one pause rule in the binary, including
    its refusal to rewrite a `Stop` into the looser `Pause` — and deliberately does
    **not** call `wait_for_drain_point`: it is not a drain.
  - The restore runs **before the child's outcome is inspected** and is unconditional,
    because the whole reason the child is ours is that its failure must not strand the
    hold. A failed restore prints the `curl` that fixes it.
  - `reload()`'s `ReloadError::Busy` arm for `is_destructible()` deleted; replaced by a
    line naming what is in flight and that the swap re-attaches to it. `--when-idle`
    keeps its wait as the opt-in; `--force` keeps its *other* job (the alive-but-silent
    server), and is no longer the way past work in flight.
  - `render_left_paused` rewritten: names `TASKS_DEFAULT_MODE` as what decides the
    successor's mode, drops the "release this later" framing, keeps the `curl` for a
    server that is already up. `Stopped::left_paused`'s doc says the same.
- **`crates/tasks/src/main.rs`** — `tasks hold [--label TEXT] -- <command>`, `HOLD_USAGE`,
  top-level usage line, `usage_for` entry. Flags are parsed **only ahead of `--`**: the
  command routinely carries flags of its own (`make -j4`), and a parser that kept looking
  would eat them. `std::process::exit(code)` propagates the child's status verbatim, so a
  recipe is unchanged by the wrapper. Reload/stop/drain help text updated to match.
- **`Makefile`** — `images:` becomes `server` + `tasks hold --label 'make images' --
  $(MAKE) images-rebuild`; the three build steps move into a new `images-rebuild` target
  (one command for `hold` to be the parent of). `check-quiesced` deleted, `.PHONY`
  updated, and the `drain:`/`FORCE` comments rewritten to say what is now true.
- **`CLAUDE.md`** — the "two host acts get a drain" bullet fully rewritten as "A run in
  flight is not a reason to refuse host work"; the *Running* command table and the
  `make images` paragraph updated. **`README.md`** — the `make images` paragraph, the
  *Day to day* drain block, and the troubleshooting row that said
  `make drain` → `make images` → `make resume`.
- **`app-gpui/src/server.rs`** — `Op::Restart` / `Op::RestartAnyway` /
  `Op::needs_confirmation` docs, `Outcome::Busy`'s meaning, and the non-stop `Busy`
  headline, which claimed "work is in flight that a restart would destroy" — a refusal
  that can no longer happen.
- **Tests.** `a_scout_in_flight_refuses_the_swap_until_forced` becomes
  `a_scout_in_flight_does_not_refuse_the_swap` (asserts exit 0, the report, and that
  `--force` is named nowhere). Four new integration tests in `crates/tasks/tests/reload.rs`:
  the pause is observed *by the command itself* (it `curl`s `/mode` into a file), the mode
  is put back although the command exited 7, the scout in flight is still running
  afterwards, both feed edges are recorded; plus the not-playing, nothing-serving and
  argument-parsing arms. Two unit tests for `render_held`'s five arms and the new
  `ModeAfterDrain` notes.

### Discovered Pitfalls
- **The restore must be a parent process, not two recipe lines.** `tasks hold` before and
  `tasks resume` after would reintroduce exactly the failure being removed: a `make` that
  dies in between leaves the pipeline paused with nothing left running that knows to undo
  it. This is why `images-rebuild` exists as its own target.
- **`pause_dispatch` returns whether *this* call installed the hold**, and the restore is
  gated on it. A pipeline already `pause`d or `stop`ped must not be *promoted* to `play`
  when the command exits — `Stop` is tighter than `Pause`, and "restoring" it would turn
  intake back on in the name of having held something.
- **The old refusal's one honest arm is inverted, not deleted.** A live server that will
  not answer `/status` used to refuse, because "quiesced" about a server you cannot see
  into is the wrong direction to be wrong in. That was right when the promise was
  quiescence; it is not the promise now. It runs the command unheld and says so — the
  cost is one run possibly starting in the old image, which reports itself through
  `ImageFreshness` the moment it does, against a refused rebuild.
- **What a `container build` does to an already-running container is still not
  established** and the argument deliberately does not rest on it. If it does disturb a
  live VM, that run dies `Transport`/`Orphaned`, charges no attempt, and is re-dispatched
  — which is the outcome this change already accepts. Do not "improve" the doc by
  asserting the container is safe; the point is that it does not need to be.
- **`stop --when-idle`'s pause was never a debt.** Verified against
  `run::apply_startup_mode`, which overwrites the stored mode before the listener binds.
  It still cannot be put back before the SIGTERM (that hands the dispatcher a window for
  one last scout) and nothing in `reload.rs` may open the store to do it after — so the
  fix is the sentence, not the behaviour.
- **Exit codes.** A signal death is reported as `128 + signum`, the shell convention, so a
  recipe that propagates `tasks hold`'s status says what it would have said unwrapped.
  `ReloadError::Busy` (exit 3) still exists — `drain --check` and the alive-but-silent
  server raise it — so the variant is not removable.
- `nothing_happened()`'s `HeldForCommand` arm is unreachable (a hold never waits for a
  drain point) and is spelled out anyway, so the match stays exhaustive by construction.

### Blockers & Dependencies
None. Nothing here touches the orphan ledger, the pool's stop-on-boot sweep, or the strike
rules — a run killed for host maintenance is already `Cancelled`/`Orphaned` and charges
nothing, which is the premise this change leans on rather than something it alters. No
migration, no API change, no charter capability: `tasks hold` speaks only `GET`/`POST
/mode`, the routes `tasks drain` already used, over loopback.

### Complexity
Medium

### Notes
- Verified: `cargo test -p tasks --lib reload::` (26 passed), `cargo test -p tasks --test
  reload` (37 passed, including the 5 new/rewritten), `cd app-gpui && cargo test` (35
  passed), `cargo fmt --all`, `cargo clippy -p tasks --all-targets` clean. The `make
  images` path itself cannot be exercised here — it needs a Mac with apple/container and
  the cross toolchain — so the `hold` half is covered by the integration tests directly
  and the recipe change is one line plus a target split.
- The integration test proves the pause by having the *held command* read `/mode` back,
  not by racing the parent. Anything that samples the mode from the test process instead
  is testing its own scheduler.
- `tasks hold` is deliberately general (`-- <any command>`) rather than an `images`-shaped
  flag: the same shape is the honest answer for any future host act that can only be
  spoiled by a *new* dispatch. It is not the answer for one that destroys running work —
  that is still `tasks drain`, and the vm-pool restart is still its only caller.
- If a seventh server-side dispatch hold is ever wanted (a real `maintenance_hold` with a
  TTL beside `github_hold`/`update_hold`), this is the change to revisit — it would remove
  the mode juggling entirely. It was rejected here for the reason `CLAUDE.md` already
  gives: mode `pause` *is* the hold, and a parallel one is a fourth thing to keep in step.

## Spec 2 of 4: Overview headlines don't wrap: a long task title overflows its row instead of breaking onto a second line (#1049)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: Overview headline wraps — `min-width: 0` on the flex-row text children

### Summary
A long task title in the Overview tab runs past its row instead of breaking onto a
second line. The cause is neither the missing `.truncate()` nor an unbounded
container — it is the flex item's *automatic minimum size*, and it is confirmed from
library source rather than inferred. The title is a `flex_1` child of a `flex_row`,
so taffy floors its main size at a MIN_CONTENT measure of its content
(`taffy-0.12.2/src/compute/flexbox.rs:816-840`, "4.5. Automatic Minimum Size of Flex
Items"), and gpui's text element answers a MIN_CONTENT probe with its **whole
unwrapped line** — `wrap_width` is `None` for anything but
`AvailableSpace::Definite` (`gpui-unofficial-1.14.2/src/elements/text.rs:650-658`),
and a `None` wrap width is passed straight to `shape_text`. The row is therefore
floored at the entire title on one line, and `flex_1`'s `0%` basis
(`gpui/src/styled.rs:181`) is clamped *up* to that floor. Adding `.min_w(px(0.))` —
CSS `min-width: 0` — drops the floor to zero: the item takes the row's remaining
width, taffy's final pass hands the text element a definite `known_dimensions.width`,
and the same gpui code path wraps it. Two other rows in the app have the
byte-identical shape and the identical defect; they are fixed in the same pass.

### Implementation Approach
- `app-gpui/src/sections/detail.rs`, `Workspace::render_overview` headline row
  (~:158): add `.min_w(px(0.))` to the `flex_1` title `div`, with a comment naming
  the automatic-minimum-size rule and why `.overflow_hidden()` is *not* the fix here.
  This is the canonical site; the explanation lives here and the other two point at
  it.
- `app-gpui/src/workspace.rs` ~:1834 (`ChatRole::Event`, the orchestrator
  conversation) and ~:2693 (`FeedRowKind::Notice`, the Agent Feed): the same
  `.min_w(px(0.))` on their `flex_1` text child, each with a one-line comment
  referring back. Both carry long server-written text by construction —
  `[worker <job>]` / `[pipeline]` / `[agent <name>]` turns and feed notes like the
  target-directory reclaim announcement — and both sit next to a `flex_none` `●`
  bullet in a `flex_row`, which is the shape that breaks.
- **No helper and no shared constant.** `min_h(px(0.))` is already spelled inline at
  five sites in this app for the same class of bug on the other axis
  (`workspace.rs:1517`, `:2005`, `:2625`, `:2916`, `sections/tasks.rs:101`), and a
  style attribute is not a question two readers can answer differently. Use
  `min_w(px(0.))` rather than the generated `min_w_0()` helper — it exists
  (`gpui-macros/src/styles.rs:870`, prefix `min_w` × suffix `0`), but nothing in this
  app uses the `_0` suffix form, and matching the `min_h(px(0.))` neighbours is what
  makes the two read as one idiom.
- Do **not** add `.truncate()` / `.overflow_hidden()` to the headline. Wrapping is
  the intent, and `overflow_hidden` installs a content mask (`gpui::Style::
  overflow_mask`) that would clip the second line this change exists to reveal.
- `items_start()` is already on the headline row, so a two-line title keeps the `✕`
  aligned to the first line. Nothing to change there, and nothing to change about the
  `flex_1` basis either — `flex_1()` is `flex: 1 1 0%`, so the basis was always
  correct; it was only ever being clamped up.

### Discovered Pitfalls
- **`.overflow_hidden()` looks like an equivalent fix and is not.** It reaches the
  same taffy branch — `Overflow::maybe_into_automatic_min_size()`
  (`taffy/src/style/mod.rs:374`) returns `Some(0.0)` for a scroll container, i.e.
  `hidden`/`scroll`, and `None` otherwise — which is the real reason every
  `.truncate()` site in this app is paired with `.overflow_hidden()`
  (`rail.rs:706/798/895`, `palette.rs:687`, `sections/tasks.rs:168`,
  `sections/changes.rs:212`, `workspace.rs:1736/2679`). The pairing was never about
  the ellipsis. Here it would clip.
- **The Brief tab needs no change**, and the issue's "if it does not, whatever it
  does differently is probably the fix" is answered: `render_brief`
  (`detail.rs:445`) renders no task title at all — its header is
  `"SPEC · <COMPLEXITY>"`. Its prose (`review_feedback`, spec content) lives in a
  `flex_col`, where the automatic minimum applies to the *main* axis (height) and
  width is definite from the stretched cross axis, so that text already wraps. The
  defect is specific to **`flex_row` + a text child**.
- **The symptom is window-width dependent, and it is overflow *and* clip rather than
  one of the two.** `Style::overflow_mask` (`gpui/src/style.rs:634-680`) returns
  `None` only when both axes are Visible; `tab_pane` sets `overflow_y_scroll()`, so
  the `(x visible, y hidden)` arm applies and its mask spans
  `[bounds.origin.x, bounds.bottom_right().x]` — the full element width, on both
  axes. But `tab_pane` is `size_full()` (the whole middle column) while `pane` is
  `max_w(CONTENT_MAX_WIDTH)` (760px). So the title spills past the *reading column*
  and is cut only at the *middle column's* edge. Wide window: the title runs into the
  gutter and the `✕` drifts right, out of line with everything below it. Narrow
  window: the title is cut and the `✕` leaves the screen. **Reproduce at a narrow
  window**, or you may see only the milder half and think the fix did nothing.
- A single unbreakable token longer than the row (a bare URL in a title) still
  overflows — gpui's line wrapper breaks on word boundaries. Out of scope; titles
  here are prose.

### Blockers & Dependencies
None. Independent of PR #1047 (#993) as the issue says — it adds windows and does not
touch `sections/detail.rs`, and the two `workspace.rs` sites are single style calls
inside existing rows.

### Complexity
Simple

### Notes
- **This is the rendering carve-out, and the verification claim is exactly that.**
  What was run here, in a Linux scout VM:
  - `cd app-gpui && cargo check --all-targets` — clean (1m23s cold, deps included).
    The only warnings are pre-existing `dead_code` in `src/modal.rs`
    (`Scrim::Clear`, `Placement::Top`), untouched by this change.
  - `cargo test -j 1` — **296 passed, 0 failed** (261 + 35 across the two bins).
    `-j 1` is required in a 6 GB Scout VM: at default parallelism two concurrent
    `ld`s over the gpui tree get OOM-killed
    (`collect2: fatal error: ld terminated with signal 9`), which is a VM memory
    limit and not a code failure. Worth knowing before reading that error as one.
  - `rustfmt --edition 2021 --check` on both changed files — clean. Note that a
    repo-wide `cargo fmt --check` inside `app-gpui` reports a **pre-existing** diff
    at `src/sections/changes.rs:15` (an import this rustfmt would join onto one
    line). Do not fold that into this change.
  - These all pass **before** the change too, and would pass on a broken fix. The
    app's tests are pure functions over view state and never enter the platform
    layer, and no layout assertion is available without standing up a real gpui
    platform — which would be a far larger change than the bug. Compiling is the only
    mechanical check there is.
- **Confirmation is `make app` on a Mac**, a task with a long title selected, at a
  narrow window. Falsifiable by hand in one step: delete the `min_w` call and the
  title stops wrapping. Real titles in this repo that exercise it — #1046 "A Scout
  that finishes the work and runs out of budget still loses the spec, because SPEC.md
  is written last", #1020 "A Builder should not be able to return untested work: the
  supervisor runs the suite, not the agent", #938 "A build owns its batch's state
  only until a later build carries the same specs".
- The rule worth carrying forward, and the reason the three sites are fixed together:
  **in a `flex_row`, a text child needs either `min_w(px(0.))` (to wrap) or
  `overflow_hidden() + truncate()` (to ellipsize). The default — neither — is the one
  combination that misbehaves**, and it misbehaves silently, since nothing in the
  test suite can see it. Every other `flex_1()` in the app is a column main axis
  already carrying `min_h(px(0.))`, a bare spacer, or already truncating; these three
  were the whole set.
- One environment note for whoever picks this up in a VM: `~/.cargo/registry` starts
  empty, `cargo fetch` in `app-gpui` works and takes ~20s, and that is what makes the
  gpui and taffy sources cited above readable under
  `~/.cargo/registry/src/index.crates.io-*/`.

## Spec 3 of 4: Copying an orchestrator message starts a text selection: the copy button does not swallow the press (#1054)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: Overlay controls swallow the press, so they stop starting text selections

### Summary
Clicking the hover-revealed copy button on an orchestrator message also anchors a text
selection in the reply underneath, because gpuikit's selectable text begins its drag on a
bubble-phase `MouseDownEvent` and nothing above it stops that event. The fix is one shared
extension — a left-mouse-down listener that calls `cx.stop_propagation()` — applied to the
three floating controls that sit over the conversation. Two things about it are easy to get
wrong and are the reason this is worth a spec rather than a one-liner: the guard has to be
on **mouse down**, not in `on_click`, and it has to sit on an **ancestor** of the control,
because gpui bubbles in reverse paint order and the control's own mouse-down bookkeeping is
what makes the following mouse-up a click. The obvious-looking alternative, `.occlude()`, is
wrong here for a specific reason given below. The change is already implemented on this
branch; it is ~10 lines plus doc comments and has **not been compiled** (see Notes).

### Implementation Approach
- **New `app-gpui/src/components/press.rs`**, re-exported from `components/mod.rs`: a
  blanket extension trait `SwallowPress` over `gpui::InteractiveElement`, whose single
  method `swallow_press()` is
  `on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())`. A trait rather than
  a free function so it composes into the existing builder chains; a blanket impl so it is
  available on `Div` and on anything else interactive. The module doc carries the whole
  argument (bubble ordering, why not `on_click`, why not `occlude`) — that reasoning is not
  recoverable from the call sites and is what stops the next overlay control reintroducing
  this.
- **`app-gpui/src/workspace.rs`, three call sites**, each on the floating *container*
  (which is an ancestor of the interactive child, which is what keeps the click working):
  - `render_chat_message`'s affordance row (`.absolute().top(-6).right(4)`) — the reported
    bug;
  - the `jump-to-newest` wrapper (`div().absolute().bottom(10).right(16)`) — floats over
    the last message;
  - `render_chat_chip`'s root (`.absolute().top_0().right_0()`) — the mic + avatar chip,
    "floated last so it paints above the conversation"; the avatar is clickable.
- **Deliberately not fixed here: gpuikit's `IconButton`.** The issue proposes putting the
  guard in the shared `icon_button` helper. That helper is not ours — it is
  `gpuikit::elements::icon_button`, a git dependency pinned in `app-gpui/Cargo.toml`. It
  already carries `cx.stop_propagation()` inside its `on_click` (too late) and
  `window.prevent_default()` on its mouse-down (the wrong verb). Moving a
  `stop_propagation` onto that mouse-down is the correct **upstream** change; if it ever
  lands, these three call sites become redundant but stay harmless.

### Discovered Pitfalls
- **`on_click` cannot fix this, and this is settled from source rather than by experiment.**
  `gpuikit/src/markdown/selectable_text.rs` anchors the drag from a `MouseDownEvent` in
  `DispatchPhase::Bubble`; a click resolves on the mouse *up* that follows, by which point
  the drag has been live for the whole gesture.
- **The guard must be an ancestor of the control.** gpui runs bubble listeners in reverse
  registration order (`window.rs::dispatch_mouse_event`) and `Interactivity::paint`
  registers an element's listeners *before* painting its children, so an ancestor's listener
  runs after every listener its descendants registered — including the bubble-phase
  mouse-down that stores `pending_mouse_down`, which is what `on_click` consumes on
  mouse-up. Put the same guard on a preceding *sibling*, or ahead of the control's own
  listeners, and the control silently stops clicking.
- **Do not use `.occlude()` or `.block_mouse_except_scroll()`.** They are documented as
  exactly "block the mouse from elements behind this", and they do stop the drag — by making
  every hitbox behind them read un-hovered. The element whose hover *reveals* a
  hover-revealed affordance is one of those: `group_hover` resolves through the group
  element's hitbox, the group is an ancestor of the row, and an ancestor's hitbox is behind
  its descendant's. The affordances would fade to `opacity(0.)` the moment the pointer
  reached them, while still being clickable. `occlude()` additionally swallows the scroll
  wheel over an invisible strip in every message's top-right corner. Stopping one event
  leaves hit testing untouched, which is why it is the right-sized tool.
- **No `.id()` is needed** for a plain `div()` to receive `on_mouse_down`: gpui's
  `should_insert_hitbox` lists `!mouse_down_listeners.is_empty()`. (The comment in
  `modal.rs` about ids and hitboxes is about a div that had no listeners of its own.)
- **Only the left button, and only the press.** Mouse *up* still propagates, which matters:
  a selection dragged from elsewhere and released over an overlay must still end its drag.
  Right-press still reaches the markdown, which does nothing with it today, so a context
  menu added there later is not silently eaten.
- The affordance row is pinned at `top(-6)`, so a ~6px sliver of it hangs over the message
  above. A press in that sliver is now swallowed too. Pre-existing geometry, mentioned so it
  is not rediscovered as a new bug.
- **Three of the four sites the issue names are not instances of this bug.** Only a control
  that geometrically overlaps a gpuikit markdown document can trigger it (the drag needs
  `hitbox.is_hovered` on a markdown *run*; a plain `String` child is `StyledText` and is not
  selectable). `sections/detail.rs:762` is an in-flow button in a row *below* a non-markdown
  command block; `workspace.rs:1037/1042` are the `RowAction::CopyNumber`/`CopyUrl`
  *handlers* reached from the row menu and key equivalents; `workspace.rs:3091` is an
  `on_action` handler. None of them involves a press over text. The genuine siblings are the
  two extra sites listed above, which the issue did not name.
- The repo is formatted with rustfmt **`--style-edition 2021`** even though the crates are
  edition 2024. A current `rustfmt --edition 2024` (1.9.0) defaults to style-edition 2024
  and reorders every `use` list in every file it touches. Verified: with
  `--style-edition 2021` a pristine `workspace.rs` is a byte-for-byte no-op. Don't land that
  churn alongside this fix.

### Blockers & Dependencies
None. `app-gpui` only — no API, server, protocol or schema change, and `app-gpui` is not a
workspace member, so `make test` is untouched.

### Complexity
Simple

### Notes
- **This was written without compiling.** The Scout VM has a 19 MB `~/.cargo` and no
  `target/`, so `make app-check` would be a cold fetch and build of the entire gpui tree —
  well past the run budget. The signatures were checked against the real dependency sources
  instead: `InteractiveElement: Sized` with
  `fn on_mouse_down(mut self, MouseButton, impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static) -> Self`.
  A Builder should still run `make app-check` first; if it does not compile it will be a
  trivial import or bound, not the design.
- **`make app-check` and `make app-test` pass either way** — this is the rendering carve-out
  (#1049's class). A green suite is not confirmation. The confirming check is `make app` on
  a Mac: hover a message, **press and hold** on the copy button, drag, release. No highlight
  may appear; the clipboard must receive the message; the affordance must stay visible while
  the pointer is on it (that is the `occlude` regression, and it is what tells the two fixes
  apart on screen); and the chat must still scroll with the pointer over that corner. Repeat
  over "jump to newest" and over the avatar chip.
- If a real regression test is ever wanted, gpui's `TestAppContext`/`VisualTestContext` can
  simulate a `MouseDownEvent` headlessly. That needs gpui's `test-support` feature on the
  app and is a change of a different size and risk than this fix; it is not proposed here.
- Reading the dependency sources is what made this tractable, and neither is in the repo:
  gpuikit at the pinned rev (`git clone https://github.com/iamnbutler/gpuikit`, checkout
  `b28732f…`) and `gpui-unofficial` 1.14.2 from crates.io. Network reaches both from a
  Scout/Builder VM.

## Spec 4 of 4: Secrets over the loopback API: write-only set, status and remove, human-only (#1004)

*A Scout wrote this spec after exploring the work by implementing it once in a throwaway branch you cannot see, and a reviewer approved it. The spec is the distilled result — trust its pitfalls.*

## Spec: Secrets over the loopback API — write-only set, status and remove, human-only (#1004)

### Summary
Three routes give the app the custody acts `tasks secrets` gives an operator at
a terminal, with no second implementation of custody behind them: `GET
/secrets` (names, `set_at`, the key-source line, and per key what is *currently*
serving it — never a value, and deliberately no read-one route), `POST
/secrets/{name}` (value in the JSON body, write-only), and `DELETE
/secrets/{name}`. All three are **human-only on the `build-now` precedent** and
refuse the orchestrator outright rather than being charter-gated: the charter
governs units of work *inside* the pipeline, and these change what the pipeline
**authenticates as**. The sealed store's mtime reload is what makes a paste take
effect on the next read with no restart, so the change is the routes, their wire
types and one shared service handle — the custody machinery itself is untouched.
Implemented and green: 11 new integration tests plus 2 wire-type unit tests,
`cargo clippy -p tasks -p tasks-api --all-targets` clean.

### Implementation Approach

**`crates/tasks-api/src/models.rs`** — `SecretName` **moves here** from
`tasks::secrets`, which re-exports it (`pub use tasks_api::models::SecretName;`)
so all existing call sites keep working. One definition rather than a wire copy
beside a store copy: the sealed store's JSON key, the CLI argument and the URL
path segment are now the same string by construction. `#[serde(rename_all =
"kebab-case")]` and `as_str()` are pinned against each other by
`the_wire_spelling_is_the_store_spelling`, because a divergence would make a
CLI-written store silently unaddressable by the route, for one name only. Adds
`names()` so the closed set is rendered from one place in every refusal.

**`crates/tasks-api/src/http.rs`** — `SetSecret { value }` (the only place a
credential appears in the wire vocabulary at all, and it only travels inbound);
`SecretSource`, a tagged enum `sealed | environment {var} | api_key_helper
{path} | unset` with `is_fallback()` and `describe()`; `SecretEntry { name,
set_at, serving }`; `SecretsStatus { store_path, initialized, key_source,
entries }` with `entry(name)`; `SecretsInitialized { store_path, key_source }`
for the 201. An enum rather than a `bool` beside an `Option`, on the `Viewer`
argument — "sealed but the environment serves it" and "nothing serves this" are
different sentences a human acts on differently, and a renderer is forced by the
compiler to answer all four.

**`crates/tasks/src/secrets.rs`** — new `Custody { data_dir, secrets }`:
- `status()` → `SecretsStatus`. Store facts from `secrets::status` (which needs
  no unseal key — status must work exactly when the key is what is missing);
  `serving` from `Secrets::source_of`, which resolves through the same three
  steps `get` does, so the report cannot disagree with what the pipeline spends.
  `NotInitialized` is folded to `initialized: false` rather than propagated.
- `seal(name, &Secret)` → `(SealedInto, SealOutcome)`. Auto-init goes through
  `secrets::init` **itself**, which is the whole argument for allowing it: `init`
  is the only function that decides where an unseal key goes, so a paste-created
  store cannot pick a different key source than a terminal-created one, and there
  is no second code path for a second custody location to exist in.
- `unseal(name)` → whether it was there.
- Both `SealOutcome` and `SealedInto` exist so the handler reports what
  *happened* rather than what was attempted.

`unseal_key_present()` is new beside `keychain_read` — see Pitfalls.

**`crates/tasks/src/server.rs`** — `Services.secrets:
Option<Arc<secrets::Custody>>` + `FromRef`, the two route lines inside `fn
routes` (never chained after it — the guard split), and:
- `require_human_for_custody` — the shared 403, called **first** in all three
  handlers, before the service lookup, the name parse and the body. So a refused
  caller learns nothing about how the host is configured and "nothing was
  written" holds structurally rather than by reading down the function.
- `custody_service` — 503 on the `bundles` shape and for the `bundles` reason.
- `parse_secret_name` — 400 naming the closed set, rather than `Path<SecretName>`
  whose axum rejection is a serde message about an enum.
- `custody_error` — `SecretsError::Key` → **503** (no unseal key, locked
  keychain, no Keychain on this platform: nothing went wrong, the capability is
  not configured, stop retrying), everything else → 500.

**`crates/tasks/src/run.rs`** — builds the `Custody` from `config.data_dir` and
`config.secrets`, the *same* live handle the poller and the broker read through.

**Value handling.** Body only, never path or query. Wrapped in
`redact::Secret` at the moment of extraction — no `Display` at all, so
interpolating it is a compile error rather than a silent `<redacted>` — trimmed
and refused empty exactly as the CLI does, exposed once into the encryptor, and
dropped. Nothing on the path logs a body. The recorded breadcrumb is
`EventPayload::Note { source: "secrets" }` carrying the **name only**: a `Note`
rather than a new typed variant because it needs no exhaustive-match churn and,
more to the point, `Note` is **not `nudge_worthy`** — a key rotation must not
spend an orchestrator turn telling the one actor that is refused this route.

**Status codes.** `GET` 200 always (including `initialized: false`). `POST`
**204**, or **201** with `SecretsInitialized` when the call created the store —
which is how "the response must say where the unseal key went" is satisfied
without inventing a body for the ordinary case. `DELETE` **204** whether or not
anything was there. 400 unknown name / empty value, 403 non-human, 503 no
service or no unseal key, 500 the write landed but this process cannot read it
back.

### Discovered Pitfalls

- **`secrets::init(dir, None)` is macOS-only**, and that is the correct shape
  rather than a gap. Off macOS it returns `Key("no Keychain on this platform —
  pass --key-file")`, which `custody_error` renders as a 503 naming the fix.
  Honouring `TASKS_SECRETS_KEY_FILE` in auto-init would be **exactly** the
  "silently pick a different key source than `init` would have" the issue
  forbids: that variable overrides where a key is *read*, never where `init`
  writes one. Do not add it.
- **`keychain_write` is `set_password`, which on macOS is
  find-then-modify-in-place.** `init` guards on the *store* existing and never on
  the credential-store item, so a data dir with no store on a host that already
  holds a `tasks-v2-secrets` item (an older data dir, a restored backup, a
  deleted `<data dir>/secrets/`) would auto-init and **overwrite the unseal key
  for that other store, stranding it permanently**. The CLI has the same hazard,
  but a human typed those words; a paste field must not decide it for them.
  `unseal_key_present()` (both the native read and the legacy `security` read,
  because either answering means an item is there) makes `Custody::seal` refuse
  that case with a message naming `TASKS_SECRETS_KEY_FILE` and `tasks secrets
  init`.
- **`Secrets::refresh_if_changed`'s late unlock is one-shot per process**
  (`late_unlock_attempted: AtomicBool`). A server whose single attempt already
  failed writes ciphertext it cannot read back, and a bare 204 there would be a
  lie in the expensive direction — the paste looks accepted and the pipeline goes
  on spending the old credential. Hence `SealOutcome`: `seal` ends by asking
  `source_of(name) == Sealed`, and `NeedsRestart` is a 500 that says the value is
  safe and a restart is needed. The `Note` is still appended, because the write
  did land.
- **`TASKS_SECRETS_KEY_FILE` is process-global**, so a test binary that
  `set_var`s it clobbers its sibling tests under `make test-cargo` (threads, not
  processes). It is not needed: a store created with `init(dir, Some(key_file))`
  records `KeySource::File(path)` in its header and `key_location` falls back to
  it. The tests set nothing.
- **This VM has `ANTHROPIC_API_KEY` in its environment** (the agent's own
  lease), so a test asserting `serving == Unset` for an unsealed name asserts
  that the fallback reporting is *broken*. The invariant that always holds is
  `!= Sealed`; the positive fallback claim is asserted only where the variable is
  actually present. Anthropic has a third resolution step (the host's
  `apiKeyHelper`) that a test cannot rule out.
- **`set_at` and `serving` are independent on purpose.** `set_at` is read from
  the store file, `serving` through the live handle; "sealed, but the
  environment is what serves it" is a real state (the unseal key moved) and the
  one a human most needs to see. Deriving one from the other flattens it into a
  lie whichever way it falls.
- The routes go inside `fn routes`, never chained onto
  `router_with_services` — the loopback guard's split exists precisely so a later
  route cannot be appended past the layer.

### Blockers & Dependencies
None. Independent of #1003 (the keyring change) — `unseal_key_present` uses
`native_key_read`/`legacy_security_read` as they stand and does not care which
wins. #1005 is the UI half and consumes these wire types; nothing here waits on
it.

### Complexity
Medium

### Notes
- **Do not add a read-one route, and do not put a value in any response.** The
  write-only property is currently structural: no type in the wire vocabulary
  can carry a value outbound. That is worth more than a rule somebody remembers.
- The end-to-end test is the acceptance criterion and is the one to keep honest:
  it makes a brokered call **before** the paste (asserting the boot-time value
  reached the recording upstream), then pastes, then calls again — so it cannot
  pass by the sentinel never having been there. A test that only asserted the
  new value would pass against a broker that reads the file on every request.
- The `GET` is refused to the orchestrator too. Not belt-and-braces: it names the
  key source and the store path, which is a map of what an agent would have to
  reach to take custody. It carries no value, so this is not a leak being closed
  — it is the surface not being advertised.
- Residue, stated rather than pretended away: serde built a `String` out of the
  request buffer to get here, and neither that buffer nor that `String` is
  zeroized. What the route controls is that no *copy* outlives the call and none
  of them is printable. Closing it fully means a zeroizing body extractor, which
  is its own change.
- CLAUDE.md's custody rule and `docs/clients.md` (both the interaction-surface
  table and the reads list) are updated in this branch, including the one
  sentence #1004 asks for on the trust model: the API is loopback and
  unauthenticated by design, so any local process can *set* (never read) a
  credential — the same standing every human-only route already has.

## Review feedback on these specs

A reviewer read the spec(s) above and approved them **with** the following. It is part of what was approved: the spec says what to build, this says what the reviewer required of it. It is not part of any spec text, so nothing above repeats it.

Treat every item as a requirement, not a suggestion. Where one genuinely conflicts with the spec it was written about, the feedback wins — it is the later word, written by the person who approved that spec — but **say so in `SUMMARY.md`**.

Account for every item in `SUMMARY.md` under a `## Review feedback` heading: one line per item saying you did it, or that you decided against it and why. Declines are fine and are expected to be written down; an item you silently dropped is indistinguishable from one you never read, and the reviewer reads the spec rather than this section.

### On spec 1 of 4: Scouts and builders are disposable — stop gating host maintenance on in-flight runs (#1070)

Approved. The shape is right and the reasoning is better than the issue's: making the hold a *parent process* rather than two recipe lines, holding for the rebuild's own duration rather than for a human's drain-resume cycle, and refusing to rest the argument on what a `container build` does to a running container. I checked the internals it leans on — `pause_dispatch` does return `Result<bool, _>` at `reload.rs:687`, `ReloadError::Busy` has four other raisers so the variant genuinely is not removable, and no `hold` subcommand exists to collide with. Three changes.

1. **`tasks hold` must restore the mode when the *parent* dies, not only when the child does.** This is the spec's own pitfall one level up, and the spec does not name it: "a `make` that dies in between leaves the pipeline paused with nothing left running that knows to undo it" is exactly what happens if `tasks hold` itself takes a SIGINT. Ctrl-C during a multi-minute image rebuild is ordinary behaviour, not an edge case, and there is no signal handling anywhere in `reload.rs` today to inherit — I grepped. Install a SIGINT/SIGTERM handler that runs the same restore and then exits `128 + signum`, and forward the signal to the child so the rebuild actually stops. SIGKILL of the parent is genuinely unrecoverable and should be named in the doc comment as the one case that strands a pause, with the `curl` that fixes it — an honest residue beats an unstated one.

2. **State the race with a human who changes the mode during the hold.** The restore is gated on whether this call installed the pause, which is right, but it then writes the captured mode back unconditionally. If someone sets `stop` from the app while the rebuild runs, the restore promotes them back to `play` — the same direction of error the spec is careful about for a *pre-existing* `Stop`. Either re-read the mode before restoring and leave it alone if it is no longer the pause this command installed, or say in the doc comment that the last writer wins and the window is the rebuild's duration. Deciding it is fine; leaving it undecided is not.

3. **Say in `SUMMARY.md` why the app's Stop confirmation keeps its prompt.** You are rewriting `Op::Restart`/`RestartAnyway`/`needs_confirmation` and the non-stop `Busy` headline on the premise that a restart destroys nothing — true, `resume_in_flight` re-attaches. The immediate-Stop prompt rests on the same premise and you are leaving it. I think leaving it is correct, because a stop with no successor leaves VMs running with nothing following them until some later boot, which is a different fact from a restart — but that reasoning has to be in the summary, or the next person reads the asymmetry as an oversight and "finishes the job".

One thing to carry rather than change: the note that a seventh server-side dispatch hold with a TTL would remove the mode juggling entirely, and was rejected because mode `pause` is the hold. Keep that in the `CLAUDE.md` rewrite, not only in the spec — it is the paragraph that stops someone adding the fourth hold in six months.

Ignore the file-overlap flag against the two gpuikit specs: that is `README.md` in two different repositories.

### On spec 2 of 4: Overview headlines don't wrap: a long task title overflows its row instead of breaking onto a second line (#1049)

Approved, and the diagnosis is the best part — the automatic minimum size of a flex item, traced to taffy and gpui source rather than guessed at, with `.overflow_hidden()` correctly identified as reaching the same branch and therefore *not* an equivalent fix here. I confirmed the target site against the working tree: `sections/detail.rs:158` is `flex_row` + `items_start()` + a `flex_1()` text child with no `min_w`, next to a `flex_none` `✕`, exactly as described. Two changes.

1. **There is a fourth site, and the "these three were the whole set" claim is wrong.** `app-gpui/src/server_window.rs`, `fn fact()` (~:288): `flex()` `.flex_row()` `.items_start()` `.gap(px(10.))`, a `flex_none().w(px(96.))` label, and `.child(div().flex_1().text_color(theme.fg()).child(value))` — the byte-identical shape, no `min_w`. It is not a cosmetic instance: its thirteen callers render the longest server-written strings in the app, including the GitHub hold sentence, each update-hold reason (which names its own discharge command), the broker, vm-pool and runtime hold text, the verify-directory reclaim line and the data-dir path. It already carries `items_start()`, which is what you write when you expect a value to wrap. Add `.min_w(px(0.))` there with the same comment pointing back at the canonical site.

2. **Name the one site you are deliberately not changing**, so the next reader does not take it for a miss: `rail.rs:333` is the same `flex_row` + `flex_1` text shape, but its child is the static string `"Tasks"`, which cannot overflow. Leave it and say why in the canonical comment — the rule is about text that can grow, not about the shape alone.

I checked the rest of the `flex_1()` sites in the app so you do not have to: 43 of them, and every other flagged one is either a bare spacer (`div().flex_1()` with no child), an already-truncating row, or a `flex_col` centred empty state where the spec's own reasoning applies and the text already wraps. With `fact()` added, the set is complete.

One thing to carry rather than change: the closing rule — in a `flex_row`, a text child needs either `min_w(px(0.))` to wrap or `overflow_hidden() + truncate()` to ellipsize, and the default of neither is the one combination that misbehaves — belongs in the canonical comment verbatim. It is the sentence that stops the fifth site being written.

Verification: this is the rendering carve-out, and the spec is right that compiling is the only mechanical check available. Do not invent a layout assertion to satisfy it. Do repeat in `SUMMARY.md` that confirmation is `make app` on a Mac at a narrow window with a long-titled task selected, and that deleting the `min_w` call is the one-step falsification.

### On spec 3 of 4: Copying an orchestrator message starts a text selection: the copy button does not swallow the press (#1054)

Approved. The two things the spec calls easy to get wrong are the two things that make it right — the guard on mouse *down* rather than in `on_click`, and on an **ancestor** of the control, since gpui registers an element's listeners before painting its children and dispatches bubble listeners in reverse registration order. The `.occlude()` rejection is the sharpest part: it would make the hover-revealed affordance fade to `opacity(0.)` the moment the pointer reached it, because `group_hover` resolves through an ancestor hitbox that `occlude` puts behind. I confirmed the three call sites against the working tree — `workspace.rs:1582` (chat chip), `:1881` (affordance row), `:2036` (jump-to-newest) — and they are the complete set of `.absolute()` overlays in that file. Two changes.

1. **The modal scrim is a fourth site and it is worse than the three, because it is not conditional on hover.** `app-gpui/src/modal.rs:508`: the scrim takes an `on_mouse_down(Left, …)` that calls the dismiss closure and **does not** `stop_propagation`, so dismissing a modal by pressing the scrim over the conversation dismisses it *and* anchors a selection in the message underneath — the reported gesture with a different trigger. Add the guard there, and add it **unconditionally rather than inside the `if dismissible` branch**: a non-dismissible scrim registers no mouse-down listener at all today, so it is the arm that lets every press through, and a scrim exists to block. This is a real behaviour change to the dismiss path — the press stops travelling — so say in `SUMMARY.md` that nothing behind a scrim is supposed to receive a press.

2. **Point the new module's doc at the prior art instead of writing the argument twice.** `modal.rs:466` already carries this exact idiom on the modal panel, with a comment that already says "Mouse *down*, so a click on a button inside still runs on the mouse up that follows". Two unlinked explanations of one gpui behaviour is how they drift. Name it from `components/press.rs`, and if the panel's guard can adopt `swallow_press()` without losing its second job (it also focuses the target), do that; if it cannot, say so in one line rather than leaving the reader to wonder why the new helper skipped the site that invented it.

Carry, do not change: leaving gpuikit's `icon_button` alone is the right call, and the reason is worth keeping in the module doc — it already carries `stop_propagation` inside its `on_click`, which is too late, and `window.prevent_default()` on the mouse-down, which is the wrong verb. If that is ever fixed upstream these call sites become redundant and stay harmless. Same for the rustfmt note: this repo is formatted `--style-edition 2021` on edition 2024 crates, and a current `rustfmt --edition 2024` reorders every `use` list it touches. Do not land that churn here.

Verification is the rendering carve-out again, and the spec was written without compiling, which it says plainly. Run `make app-check` first; a failure there will be an import or a bound, not the design. A green `make app-test` is not confirmation of anything in this change.

### On spec 4 of 4: Secrets over the loopback API: write-only set, status and remove, human-only (#1004)

Approved. Two findings in this spec are the kind that only come from reading the custody code rather than the issue, and both should survive review intact: that `keychain_write` is `set_password` and therefore find-then-modify-in-place, so an auto-init against a data dir with no store on a host that still holds a `tasks-v2-secrets` item would overwrite the unseal key for *that* store and strand it permanently — hence `unseal_key_present()` refusing rather than initialising; and that `refresh_if_changed`'s late unlock is one-shot per process, so a bare 204 after a write this process cannot read back would be a lie in the expensive direction. Two changes.

1. **`SealOutcome::NeedsRestart` must not be a 500.** The reasoning for not returning a bare 204 is right and the conclusion is wrong: the write **landed**, the ciphertext is on disk, and the only thing that failed is this process's ability to read it back. A 5xx says the server failed, every client renders it as "your paste did not work", and the obvious human response is to paste again — which will produce the same 500 forever while the value has been correctly stored each time. Return a success with an explicit outcome instead: a 200 whose body names the state ("sealed; this process cannot read it back until a restart") or a 202, your choice, but it has to be in the 2xx family and it has to be structurally distinguishable from the ordinary 204 so the app can say the restart sentence. Keep the `Note`; it is right that the write landed. While you are there, make the `SecretsError::Key` 503 say in its message that the condition will not clear on its own — 503 is the code that conventionally means "try again shortly", and this one means "configure a key source".

2. **The CLAUDE.md sentence has to name what actually enforces "human-only" here.** "Refuses the orchestrator" holds only because a worker cannot reach the API: a worker is a local child with no `X-Tasks-Actor`, so it is attributed as the *human* and is never gated, and the only thing standing between the orchestrator and this route is `DEFAULT_WORKER_CMD` carrying no `curl`. CLAUDE.md already makes that argument for `build-now`; `POST /secrets/{name}` is the sharper instance — a route around a refusal that writes a **credential** rather than dispatching a build — and the worker-lane bullet should name it so that anyone who ever adds `Bash(curl:*)` to a worker command finds out what it costs. One clause in each place, not a new bullet.

Carry rather than change, and put each in `SUMMARY.md`: the write-only property is **structural** — no type in the wire vocabulary can carry a value outbound — and that is worth more than a rule someone remembers, so no read-one route and no value in any response, ever. The four-variant `SecretSource` over a bool-plus-Option is right for the reason given ("sealed but the environment serves it" is the state a human most needs to see, and a renderer is forced by the compiler to answer all four). And the end-to-end test's shape is the acceptance criterion: the brokered call **before** the paste is what stops it passing against a broker that never had the sentinel at all — do not let that ordering be tidied away.

One thing I checked so you do not have to: `source_of` calls `refresh_if_changed`, which is mtime-gated, and the late unlock is one-shot, so `GET /secrets` performs no keychain read on the hot path. That matters because `GET` is the one shape the loopback guard does not fully cover, and a forced read that prompted for keychain access would have been a nuisance vector. It does not; leave it that way.

## Directions for this implementation

The orchestrator agent added the following when requesting this build. It is **not** part of any spec above, and no reviewer has seen it — it is addressed to you.

Treat it as a requirement, not a suggestion. The specs are still what is being implemented; these directions say how to go about it. Where one genuinely conflicts with a spec, the direction wins — it was written after the spec was approved, with this build in view — but **say so in `SUMMARY.md`**, because the reviewer reads the spec and cannot see this section.

Account for every direction in `SUMMARY.md` — including any you decided against, and why. A direction you silently dropped is indistinguishable from one you never read.

Four specs, one branch. Two notes about where they touch each other, and one about verification.

**CLAUDE.md is edited by two of these.** #1070 rewrites the "two host acts get a drain" bullet into "a run in flight is not a reason to refuse host work"; #1004 updates the custody rule. Integrate both into one coherent document — do not leave two paragraphs that disagree about whether the pipeline refuses things, and do not let the second edit revert the first.

**app-gpui/src/workspace.rs is edited by two of these**, in different rows: #1049 adds `min_w(px(0.))` to `flex_1` text children so titles wrap, #1054 adds a mouse-down guard to the floating overlays. They do not collide, but read both review-feedback sections before you start so you do not write the second on top of the first.

**Verification.** This repository declares `.tasks/verify` and the supervisor runs it — a red suite fails this build inside the VM and opens no pull request, so do not treat a green run as optional or route around it. `app-gpui` is not a workspace member, so `make test` does not cover the two app specs: run `make app-check` and `make app-test` for those, and say in SUMMARY.md that neither proves anything about layout or pointer behaviour, which is confirmable only by running the app on a Mac.

One standing rule while you are in `crates/tasks`: **a piped command reports the pipe's exit status, not the command's**. `cargo test 2>&1 | tail` reads as success when the suite was killed. Use `set -o pipefail` or check `${PIPESTATUS[0]}` any time you act on the exit code of something you piped.

## Your job

1. Implement every spec above, in order, as one coherent change in the cloned repo (cwd). You are on the right branch already.
2. Run the project's tests / lint / typecheck — get them green.
3. Commit your work with clear messages (a git identity is configured).
4. Write `SUMMARY.md` in the repo root: one or two paragraphs describing the change, suitable as a pull request body. Do not use GitHub closing keywords (`Closes #N`, `Fixes #N`) — the server links the issues itself.
5. Do NOT push and do NOT open a PR — the server does both.

**You have 60 minutes, once.** That is the whole run — the clone before you started, this turn, the supervisor's own test run and the packaging after it — measured on the wall clock from dispatch. There is no later: when you end your turn the run is over. A backgrounded command buys you nothing — its child is killed with the turn — so anything whose result you need must be awaited inline, and a poll loop over a file another process will write can only report to a turn that has already ended. Nor should you start what cannot finish: a cold build in a large workspace can run forty minutes, so weigh what a command will cost against what is left.

On step 2: when this project declares a test suite at `.tasks/verify`, the supervisor runs it itself after you finish, against the committed tree your branch carries. If it fails you get one chance to fix it and then the build fails with no pull request, so getting there first is entirely in your interest. It reads that script out of the build's BASE commit, so editing it changes nothing about what runs.
