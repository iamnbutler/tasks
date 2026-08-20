# Tell the person what `play` does, once — and make the charter visible

Pressing Play starts VMs on this Mac, spends Anthropic API credit, pushes
branches and opens pull requests, and — with the charter as it ships — merges
those pull requests and closes the issues behind them, none of it asking.
Nothing in the app said so before the click, and the charter that governs the
sharp half was rendered nowhere, so the answer to "how do I stop it merging
things" was `curl`. This adds three things and no gate: a durable server-side
record of whether this install's owner has ever been shown what unattended
operation means (`autonomy_notice`, one row, human-only); a first-run notice
window that intercepts the **first** Play press on every path that can start
the pipeline, generated from the charter rows rather than written as prose,
whose primary button acknowledges and then carries the intercepted press
through; and a Charter window with the nine capabilities as rows and Off /
Shadow / Live per row.

The words come from the charter because that is what the server actually
enforces: `Capability::consequence()` is an arm of the *same* exhaustive match
as `describe()`, so a tenth capability cannot answer one and not the other — it
fails to compile — and the two voices differ because `describe` addresses the
agent holding the permission ("file issues for work you discover") while the
person deciding what to let loose on their repository is asking what appears on
it ("file new issues on your repositories"). `is_sharp()` means *cannot be
taken back* and its doc comment says so, so the absence of `capture_work` is
not read as a judgement that nobody minds thirty new issues. `BY_CONSEQUENCE`
is a second ordering rather than a reversal of `ALL`, with a permutation test
behind it. Three buckets, not two: collapsing `shadow` into "can" claims an
effect that never happens and into "cannot" hides a judgment still being made
and recorded, and a capability with no row reads `Off`, matching
`Store::charter_entry`. **It is not a gate** — nothing on the server reads the
row to decide whether an action may happen, `intercepts` fires for `Play` alone
and at most once per install, and it answers *carry on* for every uncertainty
there is, because reading unknown as "probably never told" turns an unreachable
endpoint into a modal on every press.

## Review feedback

- **1 — one vocabulary, not two (`disclaimer.rs`).** Done, by **consuming**
  rather than absorbing. `disclaimer.rs` landed as quoted, and the deciding
  fact is one the review could not have known: `crates/tasks/tests/disclaimer.rs`
  reads `app-gpui/src/disclaimer.rs` *by path* and asserts eight named acts
  appear in both it and the README's `## Read this first`. It lives in the
  server's tree precisely because `make test` never runs the app's tests.
  Absorbing the constants into `autonomy.rs` would have broken that drift guard
  or forced it to be rewritten, and the guard is the more valuable artifact —
  it is the only thing keeping two independent bodies of prose honest. So
  `autonomy.rs` quotes `HEADLINE`, `PIPELINE_CAUTION` and `README_POINTER`, and
  `ALWAYS` carries only what `disclaimer.rs` does not say (VMs on this machine,
  API credit, that it keeps going until paused). The division is pinned in both
  directions: `the_notice_quotes_the_disclaimer_rather_than_restating_it`
  asserts the constants are used verbatim, and `always_says_what_the_disclaimer_does_not`
  fails if `ALWAYS` grows the word "merge", "close" or "pull request". Nothing
  in `about.rs`, `server_window.rs` or the Play tooltip changed — they already
  read the one source.
- **2 — the #992 seam is work, not guidance.** Traced it: `empty_state.rs`'s
  `Action::Play` reaches `workspace.rs` as `EmptyStateAction::Play =>
  self.set_mode(Mode::Play, cx)`, which is `Workspace::set_mode` — so it
  **already inherits the guard**, and `empty_state.rs`'s own module doc says
  that is deliberate. It needed no fix; I added a comment at the call site
  naming why it must not become `self.app_state.update(…)`. I re-grepped every
  `set_mode` reach in the app to confirm there is no fifth path: the title bar,
  the palette and the empty state funnel through `Workspace::set_mode`, the
  Server window's row goes through `ServerControl` and now checks the guard
  itself, and the only unguarded `AppState::set_mode` left is the notice's own
  carry-through, which is the point of it.
- **3 (as amended) — do not make `AppState` a global.** Followed the
  amendment, not the spec: constructed once in `main` before the first window,
  passed explicitly as `Workspace::new(app_state, window, cx)`, with a global
  as the escape hatch. **One obstacle, named rather than worked around:** the
  global holds the entity *strongly*, not weakly. Weak does not work here, and
  the reason is `on_reopen` — it is registered on `Application` before `run`,
  so the closure cannot capture anything built inside `run`, and `open_workspace`
  is therefore a bare `fn` that must read the state back from somewhere. With a
  weak global and the workspace as the only strong owner, cmd-W drops the
  entity, the weak handle dangles, and reopening builds a fresh one — which is
  exactly the reset-on-reopen the amendment set out to remove. Zed can hold
  `Weak` because its `Arc<AppState>` has owners outside the window; here there
  are none available. One process-lifetime singleton is the honest description,
  and there is no leak for a strong global to hide because nothing is ever
  meant to drop it. Everything else in the amendment stands: the conversion is
  its own commit (`AppState is built once for the process, not once per
  window`), ahead of the feature commits, touching only what the signature
  change forces, with nothing tidied.
  - Call sites the signature change moved: `app-gpui/src/main.rs` — the
    `Workspace::new(app_state, window, cx)` construction inside
    `open_workspace`, and `open_workspace`'s new read of `state::global`. That
    is the complete list; `Workspace::new` had exactly one caller.
  - **User-visible change nobody asked for, as instructed:** closing the main
    window no longer discards all app state. Before, cmd-W dropped `AppState`
    and reopening refetched every list from scratch and re-opened both event
    streams; now the window reopens onto the state that was already there.
- **4 — three windows in a row on a fresh install.** The notice is a
  **singleton**: a second press raises the window that is open rather than
  stacking another, and a press arriving at a window someone opened from
  `Server ▸ What Play Does…` *upgrades* it (the pending Play rides on it) rather
  than opening a second. It also cannot appear over #992's first-run state
  unprompted, because it fires on nothing but an explicit press — there is no
  launch-time trigger at all, which is the same reason a server booting into
  `play` does not fire it. What I could **not** check here is stated plainly
  below rather than implied.
- **`is_sharp` reads as irreversibility — state it.** Done, in the doc comment:
  it says the predicate means "cannot be taken back" and not "worth being told
  about", and names `capture_work` and `comment_on_work` specifically as the
  most *visible* things this system does while being deletable and therefore
  quiet there. No reclassification.
- **Do not touch `tasks-menubar` / `machines.rs`.** Untouched.

## Directions

- **Read `disclaimer.rs`, the empty state's Play path, and
  `workspace.rs`/`main.rs`/`state.rs`/`server_window.rs` as they now are,
  before writing.** Done first; the findings are items 1–3 above. The spec's
  line numbers were stale, as expected.
- **Do the `AppState` conversion first, as its own commit, listing every call
  site.** Done — commit 1 of three, listed above.
- **Verification: `make test` for the server half with the real number, and
  `cd app-gpui && cargo test -j 2` for the app.** Both run; numbers below.
- **State plainly that rendering is unverifiable here.** Stated: this was built
  and tested on Linux. **The notice window's 560×620, whether the body
  overflows on a shadow-heavy charter, and how it behaves over another window
  are unchecked** — no pixel was rendered. The body is wrapped in an
  `overflow_y_scroll` container so an overflow scrolls rather than truncates,
  which is the mitigation the spec suggested applying only if it overflowed;
  applying it unconditionally was cheaper than a guess. This is carve-out (c)
  and goes to a human.

## Notes

- `POST /autonomy-notice/ack` uses `INSERT OR IGNORE`, so the first
  acknowledgement stands and the call is idempotent; it is human-only and not
  charter-gated on the `build-now` precedent, and the test proves the refusal
  is a **no-op** rather than a 403 issued after the write.
- `set_charter` settles the server's answer in place, because a charter write
  appends nothing to the event log and nothing would arrive to refetch on.
- The migration is stamped `20260820015213` (regenerated for today, as the
  spec asked).
- `CLAUDE.md` gains the rule, including the not-a-gate argument, the three
  properties that keep it from becoming one, and the deliberate converse gap
  that a server booting into `play` dispatches without firing it.

## Verification

- `make test` — **987 tests run, 987 passed**, 0 skipped, plus
  `cargo test --doc --workspace` clean (exit 0). The trunk has moved well past
  the 389 + 20 the spec measured. Five of those are new: two in
  `crates/tasks/tests/charter.rs` (first-ack-wins over real HTTP; the
  orchestrator refused with every capability `live`, with the refusal proven to
  be a no-op) and five in `crates/tasks-api/src/models.rs` covering
  `BY_CONSEQUENCE` as a permutation that is not a reversal, the two voices, and
  the sharpness classification.
- `cd app-gpui && cargo test -j 2` — **270 + 35 = 305 passed, 0 failed**, with
  thirteen new tests across `autonomy.rs` and `charter_window.rs`. `-j 2` per
  the spec's finding that two concurrent link jobs OOM-kill `ld` on this box.
- `cargo clippy --workspace --all-targets` clean; `cargo clippy --all-targets`
  in `app-gpui` leaves only the five pre-existing warnings in
  `bin/tasks-menubar/popup.rs`. `cargo fmt` clean.
- **Not verified, and not verifiable here:** anything about how this *looks*.
  No pixel was rendered — this is a Linux builder. The notice window's 560×620,
  whether its body overflows on a shadow-heavy charter, and how it sits over
  another window are unchecked.

Verification: PASSED — `make test` (987 passed, 0 failed, plus doctests) and `cd app-gpui && cargo test -j 2` (305 passed, 0 failed)
