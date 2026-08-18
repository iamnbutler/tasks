# Refuse before the effect — `authorize` carries the rationale

Every charter-gated write endpoint calls `authorize(...)` before it touches
GitHub, but the rationale requirement lived in `require_rationale`, buried
inside the *store* call that writes the ledger row — which on every enforced
path runs **after** the GitHub write it explains. So a rationale-less
`POST /issues` created the issue upstream and then returned 400 from the
bookkeeping step, and an agent doing the obviously correct thing with a 4xx
filed one issue per retry, each with no `decisions` row behind it (#957). A 400
has to be a no-op, or the one thing that status code tells a caller to do is
the thing it must not do.

`authorize` now takes the whole `DecisionInput` rather than just the `Actor`
and applies `require_rationale` itself, at the one call every gated handler
already makes before its effect — so a handler nobody has written yet inherits
the ordering. The alternative, a per-handler `if rationale.is_empty()`, is not
a partial fix but the demonstration that the shape cannot hold: three of the
nine write routes had one (`/issues/{n}/edit`, `/pull-requests/{n}/merge`,
`/pull-requests/{n}/close`), six did not, and nothing made them. `close_task`
was the worse case — its 400 also closed the issue upstream, invisible in the
ledger *and* in the task state, since closure is only ever learned from the
poller. Inside `authorize`, `Off` → 403 still answers before the rationale 400
(a rationale cannot rescue a capability that was never going to act, and
telling a caller to write one sends it to fix the wrong thing), and the
rationale 400 answers before `Shadow` (a shadow row *is* recorded, and one with
an empty rationale is exactly the unreviewable artifact the rule exists to
prevent — shadow was never the leaky half, so this only moves *when* it
refuses). The response to a rejected call is byte-identical: `StoreError::
Invalid` maps to 400 at the same `From` impl, so the status and the sentence
are unchanged and only the side effect is gone.

Mechanically: `store::require_rationale` becomes `pub` and keeps its six store
call sites as a backstop for callers that never went through a handler — one
`pub fn`, so the two ends cannot drift. All 14 `authorize` call sites pass
`&decision`; four had built a `DecisionInput` inline per branch and now build
one above the gate (`capture_issue`, `close_task`, `edit_issue`, and
`cancel_all_runs`, which clones it per target). `queue_under_charter` is the
one gated action with a *default* rationale and had two — the default now
resolves above `authorize` and there is one, with `"queued in shadow"`
deliberately dropped: `enforced = 0` on the row already says nothing was
queued, the string had no reader anywhere in the workspace, and keeping two
defaults would mean validating a value other than the one recorded. Effect
still precedes `record_decision`, and must: recording first would leave a
decision claiming an effect a failed GitHub call never had. The three
route-specific pre-checks stay, and `CLAUDE.md` now says why they are not
duplicates to be cleaned up.

Six tests in `crates/tasks/tests/custodial.rs`, against the fake GitHub already
there: the reported retry shape, a nine-route sweep asserting 400 and that
every one of `Seen`'s seven vectors is untouched (so a tenth route that forgets
fails here rather than on a repository), the strengthened close test whose doc
comment already claimed "refuses it before touching GitHub", and the three ways
the reordering could have broken something — shadow, `off`, and the human.
Verified non-vacuous: with the one new line in `authorize` commented out, those
three GitHub-side assertions fail and the other 15 custodial tests pass.

## Review feedback

- **Required 1 — the `CLAUDE.md` bullet must say why the three bespoke checks
  stay.** Done. The bullet names `merge_pull_request`, `abandon_pull_request`
  and `edit_issue` in bold as deliberate, says they survive because each names
  what *kind* of rationale its route wants (quoting "an autonomous merge must
  say why it is safe to land"), that they now fire strictly earlier than the
  generic check, and that deleting one loses a better message rather than
  removing a duplicate.
- **Required 2 — report the suite honestly against #958.** `make test` was
  green on this branch: **776 passed / 0 failed**, 7 leaky (the documented
  scout/cancel timeout ones) and 3 slow, doctests included.
  `a_timed_out_scout_keeps_the_checkpoint_it_had_already_streamed` passed in
  that run. **That does not clear #958** — it is an ordering flake that passes
  in-suite some runs, and nothing here touches the scout path. I did not weaken
  any assertion.

## Directions

- **A known flake lives in the suite (#958), and do not weaken it.**
  Acknowledged; it did not go red on my run, and I changed nothing in
  `crates/tasks/tests/scout.rs`. See Required 2 above for the honest reading.
- **Do not widen the change to close the record-after-effect window.** Not
  widened. A GitHub write that succeeds and then fails to record still leaves
  an unattributed artifact; the `CLAUDE.md` bullet states that gap and why
  recording first would be worse, and leaves the fix (an intent-then-confirm
  record) to its own issue.

No direction or feedback item conflicted with the spec, so nothing here
overrides it.

Verification: PASSED — `make test` (776 passed / 0 failed, 7 leaky as documented; doctests included), plus `cargo fmt --all --check` and `cargo clippy --workspace --all-targets`, all clean
