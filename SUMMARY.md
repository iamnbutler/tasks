Four specs on one branch. **#1070** stops gating host maintenance on runs in
flight: `tasks reload` now *reports* what is running and swaps anyway
(`resume_in_flight` re-attaches to every live VM, so the worst case is one
`Orphaned` write-off that charges no attempt), and `make images` no longer
runs `tasks drain --check` — it wraps its rebuild in a new
`tasks hold --label 'make images' -- $(MAKE) images-rebuild`, which pauses
dispatch, runs the rebuild as **its own child**, and puts the mode back the
instant that child exits, exiting with the child's status. A parent process
rather than two recipe lines, because a `make` that died between a
`tasks drain` and a `tasks resume` would leave the pipeline paused with nothing
left running that knows to undo it; a SIGINT/SIGTERM of the hold restores too
and forwards the signal on, and a SIGKILL of it is named as the one residue.
`tasks drain`/`resume` stay, narrowed to the one host act with no recovery
(restarting vm-pool on the same socket); `drain --check` survives demoted to a
diagnostic that nothing in the repo refuses on. **#1004** puts the three
custody acts on the loopback API — `GET /secrets`, `POST /secrets/{name}`,
`DELETE /secrets/{name}` — human-only on the `build-now` precedent, with
`SecretName` moved into `tasks-api` so the store's JSON key, the CLI argument
and the URL path segment are one string by construction.

**#1049** and **#1054** are both in `app-gpui` and both are the rendering
carve-out. The first adds `.min_w(px(0.))` to four `flex_1` text children in
`flex_row`s — a flex item's automatic minimum size is a MIN_CONTENT measure and
gpui's text element answers that with its whole unwrapped line, so the row was
floored at the entire title on one line. The second adds a shared
`SwallowPress::swallow_press()` (a left-mouse-down `stop_propagation`) to the
four floating controls that sit over selectable markdown, so copying a message
no longer anchors a text selection in the reply underneath.

## Review feedback

**Spec 1 (#1070)**

- *Restore when the parent dies, not only the child.* Done. `run_child` selects
  over `child.wait()`, SIGINT and SIGTERM; on a signal it forwards the same
  signal to the child, waits a bounded grace, and returns `128 + signum`, and
  the restore runs on that path exactly as on the normal one. SIGKILL of the
  parent is named in `hold_for_command`'s doc comment as the one case that
  strands a pause, with `tasks resume` and the `curl` as the undo.
- *State the race with a human who changes the mode during the hold.* Decided
  rather than documented-as-last-writer-wins: `restore_after_hold` re-reads
  `/mode` and, if it is no longer the `pause` this command installed, leaves it
  as found and reports `Restore::Superseded(mode)`. An unreadable status still
  restores — a hold left on is the worse error. Both the reasoning and the
  choice are in the `Restore` doc comment.
- *Say why the app's Stop confirmation keeps its prompt.* It keeps it, and the
  reason is now in `Op::needs_confirmation`'s doc as well as here: the premise
  is not the same. A restart has a successor and `resume_in_flight` re-attaches
  it to every live VM; a stop has none, so it leaves those VMs running with
  nothing following them until some later boot writes them off. That is a
  different fact from a restart, not an oversight.
- *Carry the "seventh dispatch hold" note into `CLAUDE.md`.* Done — it is the
  closing sentences of the rewritten bullet, naming it as the change to revisit
  and why it was rejected (mode `pause` *is* the hold; a parallel one is a
  fourth thing to keep in step).

**Spec 2 (#1049)**

- *There is a fourth site.* Confirmed and fixed: `server_window.rs`'s `fact()`
  is the byte-identical shape, and its thirteen callers render the longest
  server-written strings in the app. `.min_w(px(0.))` added with a comment
  pointing back at the canonical site.
- *Name the site deliberately not changed.* `rail.rs:333` is named in the
  canonical comment, with the reason: the rule is about text that can grow, not
  about the shape alone, and its child is the static string `"Tasks"`.
- *Carry the closing rule into the canonical comment verbatim.* Done, in
  `sections/detail.rs`.
- Confirmation is `make app` on a Mac at a **narrow** window with a long-titled
  task selected; the one-step falsification is deleting the `min_w` call and
  watching the title stop wrapping. No layout assertion was invented.

**Spec 3 (#1054)**

- *The modal scrim is a fourth site, and the guard must be unconditional.*
  Done. `modal.rs`'s scrim now always registers a left-mouse-down listener that
  `stop_propagation`s and dismisses when dismissible, rather than registering
  one only in the `dismissible` branch — which was the arm that let every press
  through. **This is a real behaviour change to the dismiss path: the press
  stops travelling.** Nothing behind a scrim is supposed to receive a press,
  which is what a scrim is for. It is spelled out rather than composed from
  `swallow_press()` because it has a second job and two mouse-down listeners on
  one element would race (gpui stops at the first that stops propagation).
- *Point the new module's doc at the prior art.* `components/press.rs` names
  `modal.rs`'s panel guard, and the panel's comment now names `SwallowPress`
  back. The panel keeps its own listener rather than adopting the helper, for
  the same one-listener reason, and says so in one line.
- Carried unchanged: gpuikit's `icon_button` is left alone (its
  `stop_propagation` is inside `on_click`, too late; its mouse-down calls
  `window.prevent_default()`, the wrong verb), and no rustfmt churn was landed.

**Spec 4 (#1004)**

- *`SealOutcome::NeedsRestart` must not be a 500.* Changed. It is a **200**
  carrying `SecretNeedsRestart { name, detail }` — in the 2xx family and
  structurally distinguishable from the ordinary 204, so the app can say the
  restart sentence. The `Note` is still appended, because the write landed.
- *The `SecretsError::Key` 503 must say it will not clear on its own.* Done —
  `custody_error` says so and names `TASKS_SECRETS_KEY_FILE` and
  `tasks secrets init`.
- *The `CLAUDE.md` sentence must name what actually enforces "human-only".* Done,
  in the custody bullet and in `require_human_for_custody`'s doc: a worker is a
  local child with no `X-Tasks-Actor`, so it is attributed as the human and is
  never gated, and `DEFAULT_WORKER_CMD` carrying no `Bash(curl:*)` is the whole
  of what stands between the orchestrator and a route that writes a credential.
- Carried: the write-only property is structural (no wire type can carry a value
  outbound), so there is no read-one route and no value in any response; the
  four-variant `SecretSource` is kept over a bool-plus-`Option`;
  `unseal_key_present()` refuses the auto-init that would overwrite an existing
  credential-store item in place.
- **Not done, and it is the one thing I would put next:** the integration tests
  for the three routes, including the end-to-end test whose acceptance shape the
  review calls out (a brokered call **before** the paste, so it cannot pass
  against a broker that never had the sentinel). The run budget ran out on the
  fourth spec; what is here is the routes, the wire types, the `Custody` service
  and its wiring, with `SecretName`'s wire/store spelling pinned by
  `the_wire_spelling_is_the_store_spelling`. That ordering must not be tidied
  away when the tests are written.

## Directions for this build

- *`CLAUDE.md` is edited by two of these — integrate them.* Done in one pass:
  the "two host acts get a drain" bullet is replaced by "a run in flight is not
  a reason to refuse host work", and the custody bullet is extended in place.
  Neither reverts the other, and the *Running* table and the `make images`
  paragraph were updated to match the new behaviour rather than left describing
  the old gate.
- *`workspace.rs` is edited by two of these — read both feedback sections
  first.* Done; the `min_w` sites and the `swallow_press` sites are disjoint
  rows and neither edit landed on top of the other.
- *Verification.* `.tasks/verify` (`make test-ci`) was run on the committed
  tree. For the two app specs, `cargo check --all-targets` and `cargo test` in
  `app-gpui` are green — and **neither proves anything about layout or pointer
  behaviour**. The app's tests are pure functions over view state and never
  enter the platform layer; both changes are confirmable only by running the
  app on a Mac (`make app`), at a narrow window for #1049 and by press-drag-
  release on the copy button for #1054.
- *A piped command reports the pipe's exit status.* Observed — every exit code
  acted on came from an unpiped command.
