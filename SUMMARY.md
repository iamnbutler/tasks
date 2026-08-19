# A generated `reporting_section` in the orchestrator prompt

The orchestrator's chat output is read as a *stream* — pipeline notifications
arrive hours apart and each is read cold, by someone who was not here for the
one before it — and nothing in the system prompt said so. So reports opened
with an issue number ("#984 approved with five required changes") that names no
subject, and the review's detail got pasted into chat at a shorter length. This
adds `reporting_section(&[CharterEntry]) -> String` to
`crates/tasks/src/orchestrator.rs`, beside `authority_section` /
`landing_section` / `verification_section`, spliced into `system_prompt` after
"Never fabricate activity." and ahead of `{authority}` — output guidance sits
with the turn-handling guidance it qualifies, before what the agent is
permitted to do. Three bullets are shared across every charter level: lead with
what the thing IS (explicitly not a rule about reviews — a failed build, a
status answer and a declined obligation are named inside the bullet so it
cannot be narrowed back later), report facts rather than assessments (the axis
is factual-versus-evaluative and *not* positive-versus-negative, so "no praise"
cannot collapse into "report nothing favourable"), and there is no form —
`Good:` / `Bad:` named as the shape to avoid, optional-`Good:` named as the
non-fix, and no template offered in its place.

The fourth bullet is generated off the `auto_review_specs` charter row, which
is the load-bearing decision: "keep the chat half terse, the detail is in
`feedback`" is true only where a verdict is actually applied. Under `shadow`,
`server::review_spec` returns straight after `record_decision` and never passes
`body.feedback` to `Store::review_spec` — the feedback is discarded at the
handler and stored nowhere, not even on the spec's queue entry; under `off`
there is no verdict route at all. On both, the conversation is the only copy
the review has, so the `Live` arm ("YOUR CHAT REPORT IS NOT AN ABRIDGED
REVIEW") and the `Shadow`/`Off` arm ("THE DETAIL HAS NOWHERE ELSE TO GO") are
mutually exclusive by construction — one `match` producing one bullet, never a
shared bullet with a caveat appended, which would put a permission to drop
findings directly above a statement that nothing else carries them. A missing
row reads `Off`, matching `Store::charter_entry`, and here that is also the
direction that carries the detail rather than dropping it. One existing
sentence was scoped rather than deleted — "**Within the review itself**, lead
with your strongest objection…" — because two unqualified instructions about
what a report leads with is the same two-sources failure in miniature. Six
tests cover it; stubbing `reporting_section` to `String::new()` turns exactly 5
of the 6 red (the sixth pins the spliced bullet edit, which does not live in
the section), which was re-checked after the final wording.

## Review feedback

- **Qualify the `Live` arm's "one finding out of five" permission.** Done, in
  the bullet text itself rather than in a comment about it: what may be left
  out of chat is *bounded by* what `feedback` carries to someone who can act on
  it (a wrong layer, a missing test, an unchecked claim — the Builder reads
  those and acts), while a finding only the human can act on "has no home in
  `feedback` at all" — the task may not be worth doing, it contradicts
  something decided last week, it breaks work shipped three commits ago, two
  specs in flight are solving the same problem. The bullet says such a finding
  in `feedback` is addressed to a Builder that cannot act on it and will
  account for it in `SUMMARY.md` while building the thing anyway, and that it
  is reported in chat however terse the rest is, because there is no second
  copy. `the_chat_report_and_the_review_feedback_are_different_artifacts` pins
  the qualifier, not just the permission.
- **`the_reporting_format_cannot_invite_praise` should pin a property, not a
  word.** Done. The whole-prompt occurrence count of "congratulatory" is gone.
  The test now asserts the reporting section does not restate the rule (neither
  "congratulatory" nor "praise" appears in it) — that being the thing that
  could actually drift — plus one whole-prompt assertion on "praise is noise",
  the clause the existing rule cannot lose without changing meaning. The doc
  comment says which edit each half is meant to catch: restatement, and
  deletion but not rewording.
- **State that this is guidance and not enforcement.** Done, in the module doc
  as asked: `authority_section` mirrors rows `authorize` applies,
  `landing_section` a capability the endpoint enforces, `verification_section`
  a directory that either exists or does not — `reporting_section` has no such
  half, so generation buys only that it cannot *contradict* the charter, and
  nothing stops an agent drifting from it forty turns in or detects a report
  that ignores it. `reporting_section`'s own doc comment carries one clause
  pointing at it, so a reader of the function sees the limit without the
  argument being written twice.
- **The stronger, checkable fact about `shadow`.** Adopted over the spec's own
  wording (the spec said the feedback "reaches no Scout and no Builder"), and
  verified in the tree: the `Authority::Shadow` arm of `server::review_spec`
  returns after `record_decision`, so `body.feedback` never reaches
  `store.review_spec` and is discarded at the handler — stored nowhere, not
  even on the queue entry. That is what the function's doc comment and the
  `Shadow`/`Off` prompt arm now say.
- Recorded as not-required and unchanged: the charter split, the mutually
  exclusive arms, scoping rather than deleting "lead with your strongest
  objection", keeping `What it does:` / `Risks & defects:` out of the prompt
  text with a test that it did not harden into the form the bullet warns
  against, not restating "never be congratulatory", and always-present rather
  than emptyable.

## Directions

- **Account for all three required changes, declines included.** Done above;
  none was declined.
- **Get the `Live` qualifier into the bullet, not into a comment about it.**
  Done — it is prompt text the agent reads, and the test asserts against the
  rendered string.
- **The second change is to a test, the third is a doc comment.** Applied that
  way: `reporting_section` itself is untouched by items 2 and 3.
- **Use the stronger, checkable `shadow` fact and correct the spec's text.**
  Done, verified at `server::review_spec` before writing it down. The
  correction is noted under Review feedback above so a reader of the spec sees
  where the implementation departs from it.
- **One file, no contended regions.** The change is confined to
  `crates/tasks/src/orchestrator.rs`: no migration, no API surface, no
  `tasks-api` type, no charter row. `brief.rs`, `format_nudge`,
  `format_obligations`, the `feedback` channel's behaviour and every sentence
  about how adversarially the orchestrator reviews are untouched.

Also carried through from the spec's "not to change while implementing": the
section is emitted at every charter level and is deliberately not emptyable by
analogy with `degradation_section` / `verification_section` (there is no boot
on which the orchestrator makes no reports), and the scoping clause on the
Spec-landed bullet is in place rather than reverted.

Verification: PASSED — `make test` (916 tests, 0 failed, plus doctests), `cargo clippy --workspace --all-targets` clean, `cargo fmt --check` clean
